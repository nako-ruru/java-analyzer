//! LSP 之外的扩展请求。

use lsp_types::Url;
use serde::{Deserialize, Serialize};

/// `java-analyzer/classFile`：按 class 文件的位置（jar / jmod / 目录 + 条目名）或类的二进制名，
/// 找到可以打开的源码文件。依赖库和 JDK 的类物化成磁盘上的文件（和跳转定义一样），
/// 工作区里的类返回它的源文件。无法物化时结果为 `null`。
pub enum ClassFile {}

impl lsp_types::request::Request for ClassFile {
    type Params = ClassFileParams;
    type Result = Option<ClassFileResult>;
    const METHOD: &'static str = "java-analyzer/classFile";
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassFileParams {
    /// jar、jmod、JDK 的 `lib/modules` 或 class 文件目录（路径或 `file:` URI）
    #[serde(default)]
    pub archive: Option<String>,
    /// 容器里的条目，例如 `com/google/common/collect/ImmutableList.class`
    #[serde(default)]
    pub entry: Option<String>,
    /// 类的二进制名，例如 `java.util.ArrayList$Itr`
    #[serde(default)]
    pub binary_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ClassFileResult {
    pub uri: Url,
}

/// `java-analyzer/revisionDefinition`：`text` 里 `position` 处的定义，按
/// `path` 这个文件的 Git 修订版本来分析。磁盘上的文件和编辑器里的文档都不变。
pub enum RevisionDefinition {}

impl lsp_types::request::Request for RevisionDefinition {
    type Params = RevisionParams;
    type Result = Option<lsp_types::GotoDefinitionResponse>;
    const METHOD: &'static str = "java-analyzer/revisionDefinition";
}

/// `java-analyzer/revisionSemanticTokens`：`text` 自己的语义着色，坐标是这份文本的。
pub enum RevisionSemanticTokens {}

impl lsp_types::request::Request for RevisionSemanticTokens {
    type Params = RevisionParams;
    type Result = Option<lsp_types::SemanticTokens>;
    const METHOD: &'static str = "java-analyzer/revisionSemanticTokens";
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RevisionParams {
    /// 文件本身的位置（工作区里的 `file:` URI）。
    pub path: Url,
    /// 修订版本的内容。
    pub text: String,
    /// `revisionDefinition` 要查的位置，坐标属于 `text`。
    pub position: lsp_types::Position,
}
