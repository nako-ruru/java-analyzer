//! 服务器的全部可变状态和主循环。
//!
//! 主线程只做三件事：收消息、应用变更、分发任务。所有分析都在线程池里对着不可变快照做，
//! 结果经 `Task` 通道送回主线程再发给客户端。新的编辑到来时，数据库会取消仍在进行的查询，
//! 对应的请求以 `ContentModified` 回复，由客户端重发。

use std::{
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Instant,
};

use anyhow::Context;
use base_db::{ProjectGraph, SourceRootKind};
use classfile::ClassPath;
use crossbeam_channel::{select, unbounded, Receiver, Sender};
use ide::{Analysis, AnalysisHost, Change, FileChange, FileId, FileKind, FilePosition};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::{notification::Notification as _, request::Request as _, Url};
use project_model::LoadedProject;
use rustc_hash::FxHashMap;
use vfs::{ChangeKind, LoadedFile, Vfs};

use crate::{
    config::Config,
    handlers,
    library_files::LibraryFiles,
    lsp_ext, project_classpath,
    proto::{self, Doc},
    task_pool::{TaskPool, Tier},
};

/// 工作线程送回主线程的结果。
pub enum Task {
    Response(Response),
    Diagnostics {
        url: Url,
        version: Option<i32>,
        diagnostics: Vec<lsp_types::Diagnostic>,
    },
    ProjectLoaded(Box<ProjectLoad>),
    Primed {
        files: usize,
        symbols: usize,
        millis: u128,
    },
    /// 预热被编辑打断，需要重新开始
    PrimeCancelled,
    /// 自带的文件监听发现磁盘上的文件变了
    FilesChanged(Vec<PathBuf>),
    /// 依赖库的源码包下载结束（`None` 表示没下载到）
    SourcesDownloaded {
        jar: PathBuf,
        sources: Option<PathBuf>,
    },
}

pub struct ProjectLoad {
    pub project: LoadedProject,
    pub class_path: ClassPath,
    pub files: Vec<LoadedFile>,
    /// Spring 配置文件
    pub configs: Vec<LoadedFile>,
}

/// 发到工作线程上的只读视图。
pub struct Snapshot {
    pub analysis: Analysis,
    pub config: Arc<Config>,
    pub vfs: Arc<RwLock<Vfs>>,
    pub libraries: Arc<LibraryFiles>,
    pub semantic_tokens: Arc<proto::SemanticTokensCache>,
    pub signatures: Arc<RwLock<Signatures>>,
}

/// 打开的文件里签名被改过的声明（见 [`ide::SignatureTracker`]）。
pub type Signatures = FxHashMap<FileId, ide::SignatureTracker>;

impl Snapshot {
    pub fn file_id(&self, url: &Url) -> anyhow::Result<FileId> {
        let path = proto::url_to_path(url)?;
        let vfs = self.vfs.read().unwrap_or_else(|e| e.into_inner());
        vfs.file_id(&path)
            .with_context(|| base_db::tr!("未知的文件: {}", "Unknown file: {}", path.display()))
    }

    pub fn url(&self, file_id: FileId) -> Option<Url> {
        let vfs = self.vfs.read().unwrap_or_else(|e| e.into_inner());
        proto::path_to_url(vfs.file_path(file_id)).ok()
    }

    pub fn doc(&self, file_id: FileId) -> anyhow::Result<Doc> {
        if self.analysis.file_text(file_id)?.is_none() {
            // 不归分析器管的文件（客户端交过来的其他 YAML 等）：按 VFS 里的文本回空结果
            let vfs = self.vfs.read().unwrap_or_else(|e| e.into_inner());
            if let Some(text) = vfs.file_contents(file_id) {
                return Ok(Doc::new(text.clone(), self.config.encoding));
            }
        }
        let text = self.analysis.file_text(file_id)?.context(base_db::trs!(
            "文件不在数据库里",
            "The file is not in the database"
        ))?;
        let index = self.analysis.line_index(file_id)?.context(base_db::trs!(
            "文件不在数据库里",
            "The file is not in the database"
        ))?;
        Ok(Doc {
            text,
            index,
            encoding: self.config.encoding,
        })
    }
}

type ResponseHandler = fn(&mut GlobalState, Response);

type Resources = Arc<Vec<base_db::ResourceFile>>;

const LOAD_TOKEN: &str = "java-analyzer/load";
const INDEX_TOKEN: &str = "java-analyzer/index";

/// 这些文件变化后需要重新导入工程。
fn is_build_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    matches!(
        name,
        "pom.xml"
            | "build.gradle"
            | "build.gradle.kts"
            | "settings.gradle"
            | "settings.gradle.kts"
            | "gradle.properties"
            | "libs.versions.toml"
            | "java-analyzer.toml"
    )
}

pub struct GlobalState {
    sender: Sender<Message>,
    task_tx: Sender<Task>,
    task_rx: Receiver<Task>,
    pool: TaskPool,
    req_queue: lsp_server::ReqQueue<(String, Instant), ResponseHandler>,
    host: AnalysisHost,
    vfs: Arc<RwLock<Vfs>>,
    pub(crate) config: Arc<Config>,
    libraries: Arc<LibraryFiles>,
    semantic_tokens: Arc<proto::SemanticTokensCache>,
    /// 打开的文件里签名被改过的声明：按新签名更新用法时用来知道旧签名
    signatures: Arc<RwLock<Signatures>>,
    /// 工作区根目录；可以有多个（多根工作区）
    roots: Vec<PathBuf>,
    project: Arc<ProjectGraph>,
    /// 各源码根的源文件编码：从磁盘读没在编辑器里打开的源文件时用
    source_encodings: vfs::SourceEncodings,
    build_files: Vec<PathBuf>,
    /// 编辑器里打开着的文件及其版本号
    open_files: FxHashMap<FileId, i32>,
    loading: bool,
    reload_requested: bool,
    priming: bool,
    shutdown_requested: bool,
    /// 客户端不支持动态注册文件监听时的兜底监听器
    watcher: Option<notify::RecommendedWatcher>,
    /// 打开时为空、等工程导入完再填初始内容的 `.java` 文件
    pending_skeletons: Vec<PathBuf>,
    /// 已经填过初始内容的文件（清空后重新打开不再填）
    skeleton_done: rustc_hash::FxHashSet<PathBuf>,
    /// 排队下载源码包的 jar；一次只下载一个
    source_downloads: Vec<PathBuf>,
    downloading_sources: bool,
    /// 工程导入期间收到、要等导入完成再处理的请求（结果依赖完整的类路径）
    deferred_requests: Vec<Request>,
}

impl GlobalState {
    pub fn new(sender: Sender<Message>, config: Config, roots: Vec<PathBuf>) -> GlobalState {
        let (task_tx, task_rx) = unbounded();
        GlobalState {
            sender,
            task_tx,
            task_rx,
            pool: TaskPool::new(),
            req_queue: lsp_server::ReqQueue::default(),
            host: AnalysisHost::new(),
            vfs: Arc::new(RwLock::new(Vfs::default())),
            config: Arc::new(config),
            libraries: Arc::new(LibraryFiles::new()),
            semantic_tokens: Arc::default(),
            signatures: Arc::default(),
            roots,
            project: Arc::new(ProjectGraph::default()),
            source_encodings: vfs::SourceEncodings::default(),
            build_files: Vec::new(),
            open_files: FxHashMap::default(),
            loading: false,
            reload_requested: false,
            priming: false,
            shutdown_requested: false,
            watcher: None,
            pending_skeletons: Vec::new(),
            skeleton_done: Default::default(),
            source_downloads: Vec::new(),
            downloading_sources: false,
            deferred_requests: Vec::new(),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            analysis: self.host.analysis(),
            config: self.config.clone(),
            vfs: self.vfs.clone(),
            libraries: self.libraries.clone(),
            semantic_tokens: self.semantic_tokens.clone(),
            signatures: self.signatures.clone(),
        }
    }

    pub fn run(mut self, connection: Connection) -> anyhow::Result<()> {
        self.register_watchers();
        self.start_project_load();

        loop {
            select! {
                recv(connection.receiver) -> message => {
                    let Ok(message) = message else { return Ok(()) };
                    match message {
                        Message::Request(request) => self.on_request(request),
                        Message::Notification(notification) => {
                            if notification.method == lsp_types::notification::Exit::METHOD {
                                return Ok(());
                            }
                            self.on_notification(notification);
                        }
                        Message::Response(response) => self.on_client_response(response),
                    }
                }
                recv(self.task_rx) -> task => {
                    if let Ok(task) = task {
                        self.on_task(task);
                    }
                }
            }
            self.process_changes();
            self.download_wanted_sources();
        }
    }

    // ------------------------------------------------------------------
    // 发送
    // ------------------------------------------------------------------

    fn send(&self, message: Message) {
        // 客户端断开后发送会失败；主循环随后会因为接收端关闭而退出
        let _ = self.sender.send(message);
    }

    pub(crate) fn notify<N: lsp_types::notification::Notification>(&self, params: N::Params) {
        let params = serde_json::to_value(params).unwrap_or(serde_json::Value::Null);
        self.send(Message::Notification(Notification {
            method: N::METHOD.to_owned(),
            params,
        }));
    }

    fn request<R: lsp_types::request::Request>(
        &mut self,
        params: R::Params,
        handler: ResponseHandler,
    ) {
        let request = self
            .req_queue
            .outgoing
            .register(R::METHOD.to_owned(), params, handler);
        self.send(Message::Request(request));
    }

    /// 回复一个请求。已经被取消（并回复过）的请求不再回复。
    fn respond(&mut self, response: Response) {
        if let Some((method, start)) = self.req_queue.incoming.complete(&response.id) {
            tracing::debug!("{method} 耗时 {:?}", start.elapsed());
            self.send(Message::Response(response));
        }
    }

    pub(crate) fn show_message(&self, typ: lsp_types::MessageType, message: String) {
        self.notify::<lsp_types::notification::ShowMessage>(lsp_types::ShowMessageParams {
            typ,
            message,
        });
    }

    fn begin_progress(&mut self, token: &str, title: &str) {
        if !self.config.work_done_progress {
            return;
        }
        let token = lsp_types::NumberOrString::String(token.to_owned());
        self.request::<lsp_types::request::WorkDoneProgressCreate>(
            lsp_types::WorkDoneProgressCreateParams {
                token: token.clone(),
            },
            |_, _| {},
        );
        self.notify::<lsp_types::notification::Progress>(lsp_types::ProgressParams {
            token,
            value: lsp_types::ProgressParamsValue::WorkDone(lsp_types::WorkDoneProgress::Begin(
                lsp_types::WorkDoneProgressBegin {
                    title: title.to_owned(),
                    cancellable: Some(false),
                    message: None,
                    percentage: None,
                },
            )),
        });
    }

    fn end_progress(&self, token: &str, message: Option<String>) {
        if !self.config.work_done_progress {
            return;
        }
        self.notify::<lsp_types::notification::Progress>(lsp_types::ProgressParams {
            token: lsp_types::NumberOrString::String(token.to_owned()),
            value: lsp_types::ProgressParamsValue::WorkDone(lsp_types::WorkDoneProgress::End(
                lsp_types::WorkDoneProgressEnd { message },
            )),
        });
    }

    /// 请客户端监听源码和构建文件的变化。
    fn register_watchers(&mut self) {
        if !self.config.dynamic_watchers {
            return;
        }
        let globs = [
            "**/*.java",
            "**/pom.xml",
            "**/*.gradle",
            "**/*.gradle.kts",
            "**/gradle.properties",
            "**/libs.versions.toml",
            "**/java-analyzer.toml",
            "**/application*.properties",
            "**/application*.yml",
            "**/application*.yaml",
            "**/bootstrap*.properties",
            "**/bootstrap*.yml",
            "**/bootstrap*.yaml",
            "**/lombok.config",
            "**/.editorconfig",
            "**/META-INF/*spring-configuration-metadata.json",
        ];
        let watchers = globs
            .iter()
            .map(|glob| lsp_types::FileSystemWatcher {
                glob_pattern: lsp_types::GlobPattern::String((*glob).to_owned()),
                kind: None,
            })
            .collect();
        let options = lsp_types::DidChangeWatchedFilesRegistrationOptions { watchers };
        let registration = lsp_types::Registration {
            id: "java-analyzer-watched-files".to_owned(),
            method: lsp_types::notification::DidChangeWatchedFiles::METHOD.to_owned(),
            register_options: serde_json::to_value(options).ok(),
        };
        self.request::<lsp_types::request::RegisterCapability>(
            lsp_types::RegistrationParams {
                registrations: vec![registration],
            },
            |_, _| {},
        );
    }

    // ------------------------------------------------------------------
    // 请求
    // ------------------------------------------------------------------

    fn on_request(&mut self, request: Request) {
        self.req_queue
            .incoming
            .register(request.id.clone(), (request.method.clone(), Instant::now()));

        if self.shutdown_requested {
            self.respond(Response::new_err(
                request.id,
                ErrorCode::InvalidRequest as i32,
                base_db::tr!("服务器正在关闭", "The server is shutting down"),
            ));
            return;
        }
        if request.method == lsp_types::request::Shutdown::METHOD {
            self.shutdown_requested = true;
            for deferred in std::mem::take(&mut self.deferred_requests) {
                self.respond(Response::new_err(
                    deferred.id,
                    ErrorCode::InvalidRequest as i32,
                    base_db::tr!("服务器正在关闭", "The server is shutting down"),
                ));
            }
            self.respond(Response::new_ok(request.id, serde_json::Value::Null));
            return;
        }
        // 命令改的是主线程上的状态，直接在这里处理
        if request.method == lsp_types::request::ExecuteCommand::METHOD {
            let result = serde_json::from_value::<lsp_types::ExecuteCommandParams>(request.params)
                .map_err(anyhow::Error::from)
                .and_then(|params| self.execute_command(params));
            let response = match result {
                Ok(()) => Response::new_ok(request.id, serde_json::Value::Null),
                Err(err) => {
                    Response::new_err(request.id, ErrorCode::InvalidParams as i32, err.to_string())
                }
            };
            self.respond(response);
            return;
        }
        // 导入完成前类路径和工程模型不完整，答案可能是错的：先存起来
        if self.loading
            && (request.method == lsp_ext::ClassFile::METHOD
                || request.method == lsp_ext::RevisionDefinition::METHOD
                || request.method == lsp_ext::RevisionSemanticTokens::METHOD
                || request.method == project_classpath::METHOD)
        {
            self.deferred_requests.push(request);
            return;
        }
        self.dispatch(request);
    }

    /// 把请求交给对应的处理函数（在线程池上执行）。
    fn dispatch(&mut self, request: Request) {
        // 修订版分析要临时改数据库里的一个文件，只能在主线程上做
        if request.method == lsp_ext::RevisionDefinition::METHOD
            || request.method == lsp_ext::RevisionSemanticTokens::METHOD
        {
            self.answer_revision_request(request);
            return;
        }
        // 只读工程模型，直接在主线程上回答
        if request.method == project_classpath::METHOD {
            self.answer_project_classpath(request);
            return;
        }
        use lsp_types::request as req;
        let mut dispatcher = Dispatcher {
            state: self,
            request: Some(request),
        };
        dispatcher
            .on::<req::Completion>(Tier::High, handlers::completion)
            .on::<req::ResolveCompletionItem>(Tier::High, handlers::completion_resolve)
            .on::<req::SignatureHelpRequest>(Tier::High, handlers::signature_help)
            .on::<req::HoverRequest>(Tier::Normal, handlers::hover)
            .on::<req::GotoDefinition>(Tier::Normal, handlers::goto_definition)
            .on::<req::GotoDeclaration>(Tier::Normal, handlers::goto_declaration)
            .on::<req::GotoTypeDefinition>(Tier::Normal, handlers::goto_type_definition)
            .on::<req::GotoImplementation>(Tier::Background, handlers::goto_implementation)
            .on::<req::References>(Tier::Background, handlers::references)
            .on::<req::DocumentHighlightRequest>(Tier::Normal, handlers::document_highlight)
            .on::<req::DocumentSymbolRequest>(Tier::Normal, handlers::document_symbol)
            .on::<req::WorkspaceSymbolRequest>(Tier::Normal, handlers::workspace_symbol)
            .on::<req::FoldingRangeRequest>(Tier::Normal, handlers::folding_range)
            .on::<req::SelectionRangeRequest>(Tier::Normal, handlers::selection_range)
            .on::<req::DocumentLinkRequest>(Tier::Normal, handlers::document_link)
            .on::<req::SemanticTokensFullRequest>(Tier::Normal, handlers::semantic_tokens_full)
            .on::<req::SemanticTokensFullDeltaRequest>(
                Tier::Normal,
                handlers::semantic_tokens_full_delta,
            )
            .on::<req::SemanticTokensRangeRequest>(Tier::Normal, handlers::semantic_tokens_range)
            .on::<req::DocumentDiagnosticRequest>(Tier::Normal, handlers::document_diagnostic)
            .on::<req::WorkspaceDiagnosticRequest>(Tier::Background, handlers::workspace_diagnostic)
            .on::<req::CallHierarchyPrepare>(Tier::Normal, handlers::call_hierarchy_prepare)
            .on::<req::CallHierarchyIncomingCalls>(Tier::Background, handlers::incoming_calls)
            .on::<req::CallHierarchyOutgoingCalls>(Tier::Normal, handlers::outgoing_calls)
            .on::<req::TypeHierarchyPrepare>(Tier::Normal, handlers::type_hierarchy_prepare)
            .on::<req::TypeHierarchySupertypes>(Tier::Normal, handlers::supertypes)
            .on::<req::TypeHierarchySubtypes>(Tier::Background, handlers::subtypes)
            .on::<req::PrepareRenameRequest>(Tier::Normal, handlers::prepare_rename)
            .on::<req::Rename>(Tier::Background, handlers::rename)
            .on::<req::CodeActionRequest>(Tier::Normal, handlers::code_action)
            .on::<req::CodeActionResolveRequest>(Tier::Normal, handlers::code_action_resolve)
            .on::<req::Formatting>(Tier::High, handlers::formatting)
            .on::<req::RangeFormatting>(Tier::High, handlers::range_formatting)
            .on::<req::OnTypeFormatting>(Tier::High, handlers::on_type_formatting)
            .on::<req::InlayHintRequest>(Tier::Normal, handlers::inlay_hint)
            .on::<req::CodeLensRequest>(Tier::Normal, handlers::code_lens)
            .on::<req::WillRenameFiles>(Tier::Normal, handlers::will_rename_files)
            .on::<req::CodeLensResolve>(Tier::Background, handlers::code_lens_resolve)
            .on::<lsp_ext::ClassFile>(Tier::Normal, handlers::class_file)
            .finish();
    }

    fn execute_command(&mut self, params: lsp_types::ExecuteCommandParams) -> anyhow::Result<()> {
        if params.command != handlers::ACCEPT_SIGNATURE_COMMAND {
            anyhow::bail!(base_db::tr!(
                "不认识的命令: {}",
                "Unknown command: {}",
                params.command
            ));
        }
        // 参数：[文件, 记录编号, 计算这个操作时下一条记录的编号]
        let args = &params.arguments;
        let url: Url = serde_json::from_value(args.first().cloned().unwrap_or_default())?;
        let entry = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0);
        let since = args.get(2).and_then(|v| v.as_u64()).unwrap_or(u64::MAX);
        let file_id = proto::url_to_path(&url)
            .ok()
            .and_then(|path| self.vfs_mut().file_id(&path));
        let mut signatures = self.signatures.write().unwrap_or_else(|e| e.into_inner());
        if let Some(tracker) = file_id.and_then(|id| signatures.get_mut(&id)) {
            tracker.accept(entry);
        }
        // 应用这次修改时改到的其它签名（覆写方法的形参列表）不算用户的修改
        for tracker in signatures.values_mut() {
            tracker.forget_since(since);
        }
        Ok(())
    }

    fn answer_project_classpath(&mut self, request: Request) {
        let response = match project_classpath::handle(&self.project, request.params) {
            Ok(result) => Response::new_ok(request.id, result),
            Err((code, message)) => Response::new_err(request.id, code as i32, message),
        };
        self.respond(response);
    }

    fn on_client_response(&mut self, response: Response) {
        if let Some(handler) = self.req_queue.outgoing.complete(response.id.clone()) {
            handler(self, response);
        }
    }

    // ------------------------------------------------------------------
    // 通知
    // ------------------------------------------------------------------

    fn on_notification(&mut self, notification: Notification) {
        use lsp_types::notification as notif;

        fn params<N: lsp_types::notification::Notification>(
            notification: Notification,
        ) -> Option<N::Params> {
            match serde_json::from_value(notification.params) {
                Ok(params) => Some(params),
                Err(err) => {
                    tracing::error!("{} 的参数无法解析: {err}", N::METHOD);
                    None
                }
            }
        }

        match notification.method.as_str() {
            notif::Cancel::METHOD => {
                let Some(p) = params::<notif::Cancel>(notification) else {
                    return;
                };
                let id: RequestId = match p.id {
                    lsp_types::NumberOrString::Number(id) => id.into(),
                    lsp_types::NumberOrString::String(id) => id.into(),
                };
                // 立刻回复"已取消"；任务稍后算完时发现请求已不在队列里，结果被丢弃
                if let Some(response) = self.req_queue.incoming.cancel(id) {
                    self.send(Message::Response(response));
                }
            }
            notif::DidOpenTextDocument::METHOD => {
                let Some(p) = params::<notif::DidOpenTextDocument>(notification) else {
                    return;
                };
                let Ok(path) = proto::url_to_path(&p.text_document.uri) else {
                    return;
                };
                let empty = p.text_document.text.trim().is_empty();
                let file_id = self
                    .vfs_mut()
                    .set_overlay(&path, Some(p.text_document.text));
                self.open_files.insert(file_id, p.text_document.version);
                self.signatures
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(file_id, ide::SignatureTracker::new());
                if empty {
                    self.fill_new_file(path);
                }
            }
            notif::DidChangeTextDocument::METHOD => {
                let Some(p) = params::<notif::DidChangeTextDocument>(notification) else {
                    return;
                };
                let Ok(path) = proto::url_to_path(&p.text_document.uri) else {
                    return;
                };
                let encoding = self.config.encoding;
                let mut vfs = self.vfs_mut();
                let Some(file_id) = vfs.file_id(&path) else {
                    return;
                };
                let Some(current) = vfs.file_contents(file_id).cloned() else {
                    return;
                };
                let mut text = current.to_string();
                // 数据库里还是编辑之前的文本：第一处修改直接用它的语法树记录签名
                let analysis = self.host.analysis();
                let in_db = analysis
                    .file_text(file_id)
                    .ok()
                    .flatten()
                    .is_some_and(|db_text| db_text == current);
                let mut signatures = self.signatures.write().unwrap_or_else(|e| e.into_inner());
                let mut tracker = signatures.get_mut(&file_id);
                for (idx, change) in p.content_changes.into_iter().enumerate() {
                    match change.range {
                        None => {
                            text = change.text;
                            if let Some(tracker) = tracker.as_deref_mut() {
                                tracker.clear();
                            }
                        }
                        Some(range) => {
                            let doc = Doc::new(Arc::from(text.as_str()), encoding);
                            let range = doc.text_range(range);
                            if let Some(tracker) = tracker.as_deref_mut() {
                                let len = syntax::TextSize::from(change.text.len() as u32);
                                if idx == 0 && in_db {
                                    let _ = analysis
                                        .record_signature_edit(tracker, file_id, range, len);
                                } else {
                                    tracker.record_edit_in_text(&text, range, len);
                                }
                            }
                            text.replace_range(
                                usize::from(range.start())..usize::from(range.end()),
                                &change.text,
                            );
                        }
                    }
                }
                drop(signatures);
                drop(analysis);
                vfs.set_overlay(&path, Some(text));
                drop(vfs);
                self.open_files.insert(file_id, p.text_document.version);
            }
            notif::DidCloseTextDocument::METHOD => {
                let Some(p) = params::<notif::DidCloseTextDocument>(notification) else {
                    return;
                };
                let Ok(path) = proto::url_to_path(&p.text_document.uri) else {
                    return;
                };
                let file_id = self.vfs_mut().set_overlay(&path, None);
                self.open_files.remove(&file_id);
                self.signatures
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&file_id);
                self.semantic_tokens.remove(&p.text_document.uri);
                if !self.config.pull_diagnostics {
                    self.notify::<notif::PublishDiagnostics>(lsp_types::PublishDiagnosticsParams {
                        uri: p.text_document.uri,
                        diagnostics: Vec::new(),
                        version: None,
                    });
                }
            }
            notif::DidSaveTextDocument::METHOD => {
                let Some(p) = params::<notif::DidSaveTextDocument>(notification) else {
                    return;
                };
                if let Ok(path) = proto::url_to_path(&p.text_document.uri) {
                    if is_build_file(&path) {
                        self.start_project_load();
                    }
                }
            }
            notif::DidChangeWatchedFiles::METHOD => {
                let Some(p) = params::<notif::DidChangeWatchedFiles>(notification) else {
                    return;
                };
                let paths = p
                    .changes
                    .iter()
                    .filter_map(|event| proto::url_to_path(&event.uri).ok())
                    .collect();
                self.on_paths_changed(paths);
            }
            notif::DidChangeWorkspaceFolders::METHOD => {
                let Some(p) = params::<notif::DidChangeWorkspaceFolders>(notification) else {
                    return;
                };
                let removed: Vec<PathBuf> = p
                    .event
                    .removed
                    .iter()
                    .filter_map(|f| proto::url_to_path(&f.uri).ok())
                    .collect();
                self.roots.retain(|root| !removed.contains(root));
                for folder in &p.event.added {
                    if let Ok(path) = proto::url_to_path(&folder.uri) {
                        if !self.roots.contains(&path) {
                            self.roots.push(path);
                        }
                    }
                }
                self.start_project_load();
            }
            notif::DidChangeConfiguration::METHOD => {
                let Some(p) = params::<notif::DidChangeConfiguration>(notification) else {
                    return;
                };
                let mut config = (*self.config).clone();
                let reload = config.update(p.settings);
                self.config = Arc::new(config);
                if reload {
                    self.start_project_load();
                }
            }
            _ => {}
        }
    }

    /// 磁盘上的文件变了（来自客户端的通知或自带的监听器）。以磁盘的当前状态为准，
    /// 不依赖事件类型：监听器的事件可能合并、乱序。
    fn on_paths_changed(&mut self, paths: Vec<PathBuf>) {
        let mut reload = false;
        for path in paths {
            if is_build_file(&path) || self.build_files.contains(&path) {
                reload = true;
            } else if path.extension().is_some_and(|ext| ext == "java")
                || vfs::is_resource_file(&path)
            {
                let contents = if path.is_file() {
                    vfs::read_file(&path, self.source_encodings.for_path(&path))
                } else {
                    None
                };
                // 从未见过、现在也不存在的文件不必登记
                if contents.is_none() && self.vfs_mut().file_id(&path).is_none() {
                    continue;
                }
                self.vfs_mut().set_disk_contents(&path, contents);
            }
        }
        if reload {
            self.start_project_load();
        }
    }

    /// 客户端不能替我们监听文件时，自己监听工作区和各源码根。
    fn start_fallback_watcher(&mut self) {
        use notify::Watcher;

        if self.config.dynamic_watchers {
            return;
        }
        let tx = self.task_tx.clone();
        let handler = move |event: notify::Result<notify::Event>| {
            let Ok(event) = event else { return };
            if matches!(event.kind, notify::EventKind::Access(_)) {
                return;
            }
            let paths: Vec<PathBuf> = event
                .paths
                .into_iter()
                .filter(|path| {
                    let in_git = path.components().any(|c| c.as_os_str() == ".git");
                    !in_git
                        && (is_build_file(path)
                            || vfs::is_resource_file(path)
                            || path.extension().is_some_and(|ext| ext == "java"))
                })
                .collect();
            if !paths.is_empty() {
                let _ = tx.send(Task::FilesChanged(paths));
            }
        };
        let mut watcher = match notify::recommended_watcher(handler) {
            Ok(watcher) => watcher,
            Err(err) => {
                tracing::warn!("无法启动文件监听: {err}");
                return;
            }
        };
        let mut roots = self.roots.clone();
        for source_root in self.project.all_source_roots() {
            if !self.roots.iter().any(|root| source_root.starts_with(root)) {
                roots.push(source_root);
            }
        }
        for root in roots {
            if let Err(err) = watcher.watch(&root, notify::RecursiveMode::Recursive) {
                tracing::warn!("无法监听 {}: {err}", root.display());
            }
        }
        // 替换掉旧的监听器（工程重新导入后源码根可能变了）
        self.watcher = Some(watcher);
    }

    fn vfs_mut(&self) -> std::sync::RwLockWriteGuard<'_, Vfs> {
        self.vfs.write().unwrap_or_else(|e| e.into_inner())
    }

    // ------------------------------------------------------------------
    // 变更
    // ------------------------------------------------------------------

    fn file_kind(&self, path: &Path) -> FileKind {
        if self.libraries.is_library_path(path) {
            return FileKind::Library;
        }
        match self.project.source_root_of(path) {
            Some((module, root)) => FileKind::Workspace {
                module: Some(module),
                root: root.kind,
            },
            None => FileKind::Workspace {
                module: None,
                root: SourceRootKind::Main,
            },
        }
    }

    /// 把 VFS 里累积的文件变更整理成数据库变更。Spring 配置文件有变化时，第二项是资源文件的新全集。
    fn take_file_changes(&self) -> (Vec<(FileId, Option<FileChange>)>, Option<Resources>) {
        let mut vfs = self.vfs_mut();
        let changes = vfs.take_changes();
        let (configs, changes): (Vec<_>, Vec<_>) = changes
            .into_iter()
            .partition(|changed| vfs::is_resource_file(vfs.file_path(changed.file_id)));
        let configs_changed = !configs.is_empty();
        let files = changes
            .into_iter()
            // 编辑器里打开的其他文件（客户端把 YAML 等文件类型也交给了服务器）不进数据库
            .filter(|changed| {
                vfs.file_path(changed.file_id)
                    .extension()
                    .is_some_and(|ext| ext == "java")
            })
            .map(|changed| {
                let change = match changed.kind {
                    ChangeKind::Create(text) | ChangeKind::Modify(text) => {
                        let path = vfs.file_path(changed.file_id);
                        let kind = self.file_kind(path);
                        Some(FileChange::at_path(&path.to_string_lossy(), text, kind))
                    }
                    ChangeKind::Delete => None,
                };
                (changed.file_id, change)
            })
            .collect();
        drop(vfs);
        let resources = configs_changed.then(|| self.all_resources());
        (files, resources)
    }

    /// VFS 里全部 Spring 配置文件和配置元数据。
    fn all_resources(&self) -> Resources {
        let vfs = self.vfs.read().unwrap_or_else(|e| e.into_inner());
        let list = vfs
            .iter()
            .filter(|(_, path)| vfs::is_resource_file(path))
            .filter_map(|(file_id, path)| {
                Some(base_db::ResourceFile {
                    file_id,
                    name: path
                        .file_name()
                        .map(|n| n.to_string_lossy().into())
                        .unwrap_or_default(),
                    dir: base_db::file_dir(&path.to_string_lossy()),
                    module: self.module_containing(path),
                    text: vfs.file_contents(file_id)?.clone(),
                })
            })
            .collect();
        Arc::new(list)
    }

    /// 路径所在的模块：模块根是它的最长前缀的那个。
    fn module_containing(&self, path: &Path) -> Option<base_db::ModuleId> {
        self.project
            .modules
            .iter()
            .enumerate()
            .filter(|(_, module)| path.starts_with(&module.root))
            .max_by_key(|(_, module)| module.root.components().count())
            .map(|(i, _)| base_db::ModuleId(i as u32))
    }

    fn process_changes(&mut self) {
        self.apply_vfs_changes(true);
    }

    /// 修订版分析用的变更：应用文件内容，但不发布诊断（快照里的错误不属于编辑器）。
    fn process_changes_quiet(&mut self) {
        self.apply_vfs_changes(false);
    }

    fn apply_vfs_changes(&mut self, diagnostics: bool) {
        let (files, resources) = self.take_file_changes();
        if files.is_empty() && resources.is_none() {
            return;
        }
        self.host.apply_change(Change {
            files,
            resources,
            ..Change::default()
        });
        if diagnostics {
            self.push_diagnostics();
        }
    }

    /// 修订版文本的分析：临时把该文件在数据库里的文本换成 revision 的内容，
    /// 分析完再换回来。不改变磁盘、编辑器文档，也不发布诊断。
    fn answer_revision_request(&mut self, request: Request) {
        let id = request.id;
        let definition = request.method == lsp_ext::RevisionDefinition::METHOD;
        let params = match serde_json::from_value::<lsp_ext::RevisionParams>(request.params) {
            Ok(params) => params,
            Err(err) => {
                self.respond(Response::new_err(
                    id,
                    ErrorCode::InvalidParams as i32,
                    err.to_string(),
                ));
                return;
            }
        };
        if definition {
            let result = self.with_revision_text(params, |snap, file_id, doc, position| {
                let found = snap.analysis.goto_definition(FilePosition {
                    file_id,
                    offset: doc.offset(position),
                })?;
                handlers::goto_response(snap, doc, found)
            });
            let response = match result {
                Ok(value) => Response::new_ok(id, value),
                Err(err) => Response::new_err(id, ErrorCode::InternalError as i32, err.to_string()),
            };
            self.respond(response);
        } else {
            let result = self.with_revision_text(params, |snap, file_id, doc, _| {
                let ranges = snap.analysis.highlight(file_id, None)?;
                Ok(Some(lsp_types::SemanticTokens {
                    result_id: None,
                    data: proto::semantic_tokens(doc, ranges),
                }))
            });
            let response = match result {
                Ok(value) => Response::new_ok(id, value),
                Err(err) => Response::new_err(id, ErrorCode::InternalError as i32, err.to_string()),
            };
            self.respond(response);
        }
    }

    fn with_revision_text<T>(
        &mut self,
        params: lsp_ext::RevisionParams,
        analyze: impl FnOnce(&Snapshot, FileId, &Doc, lsp_types::Position) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let path = proto::url_to_path(&params.path)?;
        let position = params.position;
        let Some(file_id) = self
            .vfs
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .file_id(&path)
        else {
            anyhow::bail!(base_db::tr!(
                "不是工作区里的文件",
                "not a file of the workspace"
            ));
        };
        let previous = {
            let vfs = self.vfs.read().unwrap_or_else(|e| e.into_inner());
            if vfs.is_overlaid(file_id) {
                vfs.file_contents(file_id).map(|text| text.to_string())
            } else {
                None
            }
        };
        self.vfs_mut().set_overlay(&path, Some(params.text));
        self.process_changes_quiet();
        let result = (|| {
            let snap = self.snapshot();
            let doc = snap.doc(file_id)?;
            analyze(&snap, file_id, &doc, position)
        })();
        self.vfs_mut().set_overlay(&path, previous);
        self.process_changes_quiet();
        result
    }

    /// push 模型：为所有打开的文件重新计算诊断。
    fn push_diagnostics(&mut self) {
        if self.config.pull_diagnostics {
            return;
        }
        let open: Vec<(FileId, i32)> = self.open_files.iter().map(|(id, v)| (*id, *v)).collect();
        for (file_id, version) in open {
            let snap = self.snapshot();
            let tx = self.task_tx.clone();
            self.pool.spawn(Tier::Normal, move || {
                // 被取消时什么都不发：打断它的那次编辑会触发下一轮
                if let Ok(Some((url, diagnostics))) = handlers::compute_diagnostics(&snap, file_id)
                {
                    let _ = tx.send(Task::Diagnostics {
                        url,
                        version: Some(version),
                        diagnostics,
                    });
                }
            });
        }
    }

    /// 语义结果整体变了（工程重新导入、索引完成）：让客户端重新取。
    fn refresh_client(&mut self) {
        self.push_diagnostics();
        if self.config.pull_diagnostics && self.config.diagnostic_refresh {
            self.request::<lsp_types::request::WorkspaceDiagnosticRefresh>((), |_, _| {});
        }
        if self.config.semantic_tokens_refresh {
            self.request::<lsp_types::request::SemanticTokensRefresh>((), |_, _| {});
        }
    }

    // ------------------------------------------------------------------
    // 工程导入与索引
    // ------------------------------------------------------------------

    /// 在独立线程上导入工程（可能要跑 mvn / gradle，耗时不可控）。
    fn start_project_load(&mut self) {
        if self.loading {
            self.reload_requested = true;
            return;
        }
        self.loading = true;
        self.begin_progress(LOAD_TOKEN, base_db::trs!("导入工程", "Importing project"));

        let roots = self.roots.clone();
        let options = self.config.load_options();
        let tx = self.task_tx.clone();
        let spawned = std::thread::Builder::new()
            .name("ja-project-load".to_owned())
            .spawn(move || {
                let project = project_model::load_projects(&roots, &options);
                let class_path = project_model::build_class_path(&project.graph);
                let encodings = vfs::SourceEncodings::new(project.graph.source_root_encodings());
                let files = vfs::load_java_files(&project.graph.all_source_roots(), &encodings);
                let module_roots: Vec<PathBuf> = project
                    .graph
                    .modules
                    .iter()
                    .map(|m| m.root.clone())
                    .collect();
                let mut configs = vfs::load_spring_configs(&module_roots);
                configs.extend(vfs::load_dir_configs(
                    &module_roots,
                    &project.graph.all_source_roots(),
                ));
                let _ = tx.send(Task::ProjectLoaded(Box::new(ProjectLoad {
                    project,
                    class_path,
                    files,
                    configs,
                })));
            });
        if let Err(err) = spawned {
            self.loading = false;
            self.end_progress(LOAD_TOKEN, None);
            self.show_message(
                lsp_types::MessageType::ERROR,
                base_db::tr!(
                    "无法启动工程导入: {err}",
                    "Failed to start the project import: {err}"
                ),
            );
        }
    }

    /// 新建的空 `.java` 文件：填上 package 声明和同名的类（`workspace/applyEdit`）。
    /// 工程还没导入完时先记下来，导入完再填（那时才知道源码根）。
    fn fill_new_file(&mut self, path: PathBuf) {
        let enabled = self.config.apply_edit && self.config.user.new_file_template.unwrap_or(true);
        if !enabled
            || path.extension().is_none_or(|e| e != "java")
            || self.skeleton_done.contains(&path)
        {
            return;
        }
        // 磁盘上已经有内容的文件（比如被清空了还没保存）不动
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > 0) {
            return;
        }
        if self.loading && self.project.modules.is_empty() {
            if !self.pending_skeletons.contains(&path) {
                self.pending_skeletons.push(path);
            }
            return;
        }
        // 文件仍然开着并且还是空的
        let still_empty = {
            let vfs = self.vfs.read().unwrap_or_else(|e| e.into_inner());
            vfs.file_id(&path).is_some_and(|id| {
                self.open_files.contains_key(&id)
                    && vfs.file_contents(id).is_some_and(|t| t.trim().is_empty())
            })
        };
        if !still_empty {
            return;
        }
        let Some(package) = handlers::package_for_new_file(&path, &self.project.all_source_roots())
        else {
            return;
        };
        let Some(text) = handlers::new_file_skeleton(&path, &package) else {
            return;
        };
        let Ok(uri) = Url::from_file_path(&path) else {
            return;
        };
        self.skeleton_done.insert(path);
        let start = lsp_types::Position::new(0, 0);
        let edit = lsp_types::TextEdit {
            range: lsp_types::Range::new(start, start),
            new_text: text,
        };
        let mut changes = std::collections::HashMap::new();
        changes.insert(uri, vec![edit]);
        self.request::<lsp_types::request::ApplyWorkspaceEdit>(
            lsp_types::ApplyWorkspaceEditParams {
                label: Some(base_db::trs!("新建 Java 文件", "New Java file").to_owned()),
                edit: lsp_types::WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                },
            },
            |_, _| {},
        );
    }

    /// 后台下载跳转时发现缺少的源码包（一次一个，按请求顺序）。
    fn download_wanted_sources(&mut self) {
        self.source_downloads
            .extend(self.libraries.take_wanted_sources());
        if self.downloading_sources || self.source_downloads.is_empty() {
            return;
        }
        let jar = self.source_downloads.remove(0);
        let root = self
            .roots
            .first()
            .cloned()
            .unwrap_or_else(|| PathBuf::from("."));
        let tx = self.task_tx.clone();
        self.downloading_sources = true;
        let spawned = std::thread::Builder::new()
            .name("download-sources".to_owned())
            .spawn(move || {
                let sources = project_model::download_sources(&jar, &root);
                let _ = tx.send(Task::SourcesDownloaded { jar, sources });
            });
        if spawned.is_err() {
            self.downloading_sources = false;
        }
    }

    /// 源码包下载好了：记进工程模型（悬停的文档、参数名随之更新），
    /// 已经生成的桩文件就地换成源码。
    fn use_downloaded_sources(&mut self, jar: &Path, sources: PathBuf) {
        let mut graph = (*self.project).clone();
        let mut changed = false;
        for library in &mut graph.libraries {
            if library.jar == jar && library.sources.is_none() {
                library.sources = Some(sources.clone());
                changed = true;
            }
        }
        if changed {
            self.project = Arc::new(graph);
            self.host.apply_change(Change {
                project: Some(self.project.clone()),
                ..Default::default()
            });
        }
        self.libraries.clear();
        let replaced = self.libraries.replace_stubs(jar, &sources);
        tracing::info!(
            "已下载 {} 的源码包，替换了 {} 个桩文件",
            jar.display(),
            replaced.len()
        );
        // 开着的桩文件：磁盘内容变了，按文件变更处理（客户端也会重新加载）
        if !replaced.is_empty() {
            self.on_paths_changed(replaced);
        }
    }

    fn on_project_loaded(&mut self, load: ProjectLoad) {
        self.loading = false;
        let ProjectLoad {
            project,
            class_path,
            files,
            configs,
        } = load;
        for message in &project.messages {
            self.show_message(lsp_types::MessageType::WARNING, message.clone());
        }
        tracing::info!(
            "工程导入完成: {:?}, {} 个模块, {} 个依赖, {} 个源文件, {} 个二进制类",
            project.kind,
            project.graph.modules.len(),
            project.graph.libraries.len(),
            files.len(),
            class_path.len(),
        );
        self.source_encodings = vfs::SourceEncodings::new(project.graph.source_root_encodings());
        self.project = Arc::new(project.graph);
        self.build_files = project.build_files;
        self.libraries.clear();

        let kinds: Vec<(FileId, FileKind)> = {
            let mut vfs = self.vfs_mut();
            for file in files.into_iter().chain(configs) {
                vfs.set_disk_contents(&file.path, Some(file.text));
            }
            vfs.iter()
                .filter(|(_, path)| !vfs::is_resource_file(path))
                .map(|(id, path)| (id, self.file_kind(path)))
                .collect()
        };
        let (files, _) = self.take_file_changes();
        let change = Change {
            files,
            kinds,
            project: Some(self.project.clone()),
            class_path: Some(Arc::new(class_path)),
            // 模块编号可能变了：资源文件总是整体重建
            resources: Some(self.all_resources()),
        };
        self.host.apply_change(change);
        self.end_progress(LOAD_TOKEN, None);
        // 导入期间存起来的请求；客户端已经取消了的不再处理
        for request in std::mem::take(&mut self.deferred_requests) {
            if !self.req_queue.incoming.is_completed(&request.id) {
                self.dispatch(request);
            }
        }
        for path in std::mem::take(&mut self.pending_skeletons) {
            self.fill_new_file(path);
        }

        if self.reload_requested {
            self.reload_requested = false;
            self.start_project_load();
        }
        self.start_fallback_watcher();
        self.start_priming();
        self.refresh_client();
    }

    /// 后台预热：解析全部源文件、建立符号索引。
    fn start_priming(&mut self) {
        if self.priming {
            return;
        }
        self.priming = true;
        self.begin_progress(INDEX_TOKEN, base_db::trs!("建立索引", "Indexing"));
        let analysis = self.host.analysis();
        let tx = self.task_tx.clone();
        self.pool.spawn(Tier::Background, move || {
            let start = Instant::now();
            let task = match analysis.prime_caches() {
                Ok((files, symbols)) => Task::Primed {
                    files,
                    symbols,
                    millis: start.elapsed().as_millis(),
                },
                Err(_) => Task::PrimeCancelled,
            };
            let _ = tx.send(task);
        });
    }

    fn on_task(&mut self, task: Task) {
        match task {
            Task::Response(response) => self.respond(response),
            Task::Diagnostics {
                url,
                version,
                diagnostics,
            } => {
                // 过期的结果（文件在计算期间又被改了）直接丢弃
                let current = proto::url_to_path(&url)
                    .ok()
                    .and_then(|path| self.vfs.read().ok()?.file_id(&path))
                    .and_then(|id| self.open_files.get(&id).copied());
                if version.is_some() && current != version {
                    return;
                }
                self.notify::<lsp_types::notification::PublishDiagnostics>(
                    lsp_types::PublishDiagnosticsParams {
                        uri: url,
                        diagnostics,
                        version,
                    },
                );
            }
            Task::ProjectLoaded(load) => self.on_project_loaded(*load),
            Task::SourcesDownloaded { jar, sources } => {
                self.downloading_sources = false;
                if let Some(sources) = sources {
                    self.use_downloaded_sources(&jar, sources);
                }
                self.download_wanted_sources();
            }
            Task::Primed {
                files,
                symbols,
                millis,
            } => {
                self.priming = false;
                tracing::info!("索引完成: {files} 个文件, {symbols} 个符号, {millis} ms");
                self.end_progress(
                    INDEX_TOKEN,
                    Some(base_db::tr!(
                        "{files} 个文件, {symbols} 个符号",
                        "{files} files, {symbols} symbols"
                    )),
                );
            }
            Task::FilesChanged(paths) => self.on_paths_changed(paths),
            Task::PrimeCancelled => {
                // 已解析的部分留在缓存里，重来的代价只是剩余的文件
                self.priming = false;
                self.end_progress(INDEX_TOKEN, None);
                self.start_priming();
            }
        }
    }
}

/// 按方法名把请求路由到处理函数，并安排到线程池上执行。
struct Dispatcher<'a> {
    state: &'a mut GlobalState,
    request: Option<Request>,
}

impl Dispatcher<'_> {
    fn on<R>(
        &mut self,
        tier: Tier,
        handler: fn(Snapshot, R::Params) -> anyhow::Result<R::Result>,
    ) -> &mut Self
    where
        R: lsp_types::request::Request + 'static,
        R::Params: serde::de::DeserializeOwned + Send + 'static,
        R::Result: serde::Serialize,
    {
        let Some(request) = self.request.take_if(|request| request.method == R::METHOD) else {
            return self;
        };
        let id = request.id;
        let params: R::Params = match serde_json::from_value(request.params) {
            Ok(params) => params,
            Err(err) => {
                self.state.respond(Response::new_err(
                    id,
                    ErrorCode::InvalidParams as i32,
                    err.to_string(),
                ));
                return self;
            }
        };

        let snap = self.state.snapshot();
        let tx = self.state.task_tx.clone();
        self.state.pool.spawn(tier, move || {
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(snap, params)));
            let response = match outcome {
                Ok(Ok(result)) => match serde_json::to_value(result) {
                    Ok(value) => Response::new_ok(id, value),
                    Err(err) => {
                        Response::new_err(id, ErrorCode::InternalError as i32, err.to_string())
                    }
                },
                // 查询被新的编辑取消：让客户端重发
                Ok(Err(err)) if err.is::<ide::Cancelled>() => Response::new_err(
                    id,
                    ErrorCode::ContentModified as i32,
                    "content modified".to_owned(),
                ),
                // 操作本身不成立（重命名冲突等）：消息原样交给用户
                Ok(Err(err)) if err.is::<handlers::UserError>() => {
                    Response::new_err(id, ErrorCode::RequestFailed as i32, err.to_string())
                }
                Ok(Err(err)) => {
                    Response::new_err(id, ErrorCode::InternalError as i32, format!("{err:#}"))
                }
                Err(_) => Response::new_err(
                    id,
                    ErrorCode::InternalError as i32,
                    base_db::tr!(
                        "处理请求时发生内部错误",
                        "Internal error while handling the request"
                    ),
                ),
            };
            let _ = tx.send(Task::Response(response));
        });
        self
    }

    fn finish(&mut self) {
        if let Some(request) = self.request.take() {
            self.state.respond(Response::new_err(
                request.id,
                ErrorCode::MethodNotFound as i32,
                base_db::tr!(
                    "不支持的请求: {}",
                    "Unsupported request: {}",
                    request.method
                ),
            ));
        }
    }
}
