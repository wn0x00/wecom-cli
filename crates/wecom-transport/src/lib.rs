//! Transport layer for the WeChat Work client — HTTP (reqwest).
//!
//! # Key types
//!
//! - [`Transport`] — unified handle holding `Arc<dyn TransportBackend>`. Accepts any
//!   [`TransportBackend`](traits::TransportBackend) via `From<T>` or `Transport::new()`.
//! - [`TransportBackend`](traits::TransportBackend) — open trait for custom transport backends.
//! - [`Endpoint`] — unified addressing carrying HTTP info for one call.
//! - [`TransportRequest`] — builder-style request handle, `IntoFuture`-driven.
//!
//! # Implementing a custom backend
//!
//! Building blocks for custom [`TransportBackend`](traits::TransportBackend)
//! implementations (protocol types, long-task polling, resumable download,
//! request envelope) live under [`backend`] — kept out of the crate root so
//! the top level stays focused on the consumer API.
//!
//! # Minimal example
//!
//! ```ignore
//! let backend = wecom_transport::HttpTransportBackend::default();
//! let transport = wecom_transport::Transport::from(backend)
//!     .with_header("Authorization", "Bearer x")?;
//! let endpoint = Endpoint::new("https://api.example.com", "/cgi-bin/action");
//! let result = transport.invoke(&endpoint, &serde_json::json!({})).await?;
//! ```

pub mod backend;
mod builder;
mod common;
mod dispatch;
mod http;
mod http_client;
mod macros;
mod polling;
pub mod telemetry;
pub mod traits;
mod transport;

pub use builder::TransportBuilder;
pub use common::error::*;
pub use common::{
    CatalogKey, Endpoint, EndpointCatalog, EndpointExt, Extension, Extensions, IntoCowEndpoint,
    IntoHeaderName, IntoHeaderValue, MaskedHeaders, PollEndpoint, RequestOptions, WireOptions,
};
pub use dispatch::{ExecuteOutput, PollCallback, PollEvent, TransportRequest};
pub use http::{
    EndpointHttpExt, GatewayRes, HttpEndpoint, HttpTransportBackend, PassthroughReq,
    RequestEnvelope, ResponseEnvelope,
};
// [yingdao-fork] 末尾三项（MultipartBody / MultipartField / OpenFileFn /
// OpenedFilePart / basename_of）为文件桥新增，详见 http_client/multipart_body.rs。
pub use http_client::{
    ByteStream, ContentRange, HttpClient, HttpRequest, HttpRequestBody, HttpRequestPayload,
    HttpResponse, IntoHttpRequestPayload, MultipartBody, MultipartField, OpenFileFn,
    OpenedFilePart, basename_of,
};
pub use traits::{TransportBackend, TransportResponse};
pub use transport::Transport;

pub(crate) type Result<T> = std::result::Result<T, Error>;
