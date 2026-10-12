//! [yingdao-fork] 结构化 multipart 载荷（iPaaS 文件桥用）。
//!
//! 上游只有 [`HttpRequestBody::Form`](super::HttpRequestBody)，而
//! `reqwest::multipart::Form` 是不透明的（`Part` 无法取回文件字节），
//! `wecom-cli` 的 iPaaS 代理后端就无法把文件改道对象存储。
//!
//! 因此这里引入一个可枚举的结构化表示，同一个载荷支持两种物化方向：
//!
//! - **直连模式**（[`reqwest_send`](super::reqwest_send)）：
//!   经 [`into_reqwest_form`] 组装成 `reqwest::multipart::Form`，行为与
//!   上游 `HttpRequestPayload::form` 完全一致（同样是延迟物化、可重放）。
//! - **代理模式**（`wecom-cli` 的 `ipass_proxy`）：
//!   读取 [`MultipartField::File`] 的 [`OpenedFilePart::path`]（已经过沙箱校验），
//!   把文件上传到 OSS，只把 `{url, filename}` 描述符通过网关转发。
//!
//! 每次物化都会重新调用 `open`，保留上游「工厂幂等、每次发送恰好物化一次」的语义。
//!
//! 注意：这里**不携带 MIME 类型**。上游 `Fs::open_as_multipart_part` 也没有设置
//! `Part` 的 mime，reqwest 会回落到 `application/octet-stream`；保持这一现状可以
//! 确保直连模式行为零变化。若将来要按扩展名推断类型，需先确认真实企业微信网关的接受范围。

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use crate::Error;

/// 一次物化的文件片段：reqwest 表单片段 + 已经过沙箱校验的绝对路径。
#[derive(Debug)]
pub struct OpenedFilePart {
    /// 供直连模式使用的 reqwest 表单片段。
    pub part: reqwest::multipart::Part,
    /// 供代理模式使用的绝对路径（已通过 `Fs` 的沙箱校验）。
    pub path: PathBuf,
}

/// 文件片段工厂：每次发送 / 重放都重新解析并打开文件。
pub type OpenFileFn =
    dyn Fn() -> Pin<Box<dyn Future<Output = Result<OpenedFilePart, Error>> + Send>> + Send + Sync;

/// 结构化 multipart 的一个字段。
pub enum MultipartField {
    /// 普通文本字段。
    Text { name: String, value: String },
    /// 文件字段。
    ///
    /// 文件名不从调用方传入，而是由 [`OpenedFilePart::path`] 的 basename 推导
    /// ——与上游 `Fs::open_as_multipart_part` 的行为完全一致（它也是用解析后的
    /// 路径 basename 设置 `Part::file_name`）。
    File {
        /// multipart 字段名（即网关侧的表单键）。
        name: String,
        /// 延迟物化入口，可被多次调用。
        open: Arc<OpenFileFn>,
    },
}

impl std::fmt::Debug for MultipartField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text { name, value } => f
                .debug_struct("Text")
                .field("name", name)
                .field("value", value)
                .finish(),
            // 工厂闭包不参与 Debug 输出，避免打印无意义的内存地址。
            Self::File { name, .. } => f.debug_struct("File").field("name", name).finish(),
        }
    }
}

/// 从已解析路径推导 multipart 文件名（与上游行为一致）。
pub fn basename_of(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "upload".to_string())
}

/// 结构化 multipart 载荷。
#[derive(Debug)]
pub struct MultipartBody {
    pub fields: Vec<MultipartField>,
}

/// 把结构化载荷物化为 `reqwest::multipart::Form`（直连模式）。
pub(crate) async fn into_reqwest_form(body: MultipartBody) -> Result<reqwest::multipart::Form, Error> {
    let mut form = reqwest::multipart::Form::new();
    for field in body.fields {
        match field {
            MultipartField::Text { name, value } => {
                form = form.text(name, value);
            }
            MultipartField::File { name, open } => {
                // `Part` 的 file_name 已由 open 工厂按解析后的 basename 设置
                // （见 [`MultipartField::File`] 的说明）。
                let opened = open().await?;
                form = form.part(name, opened.part);
            }
        }
    }
    Ok(form)
}
