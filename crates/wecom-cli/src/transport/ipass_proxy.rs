//! OpenCode iPaaS proxy backend for WeCom API requests.
//!
//! [yingdao-fork] 在代理链路上新增文件桥：
//!
//! - **响应侧**：在 HttpClient 层拆掉 OC 传输外壳 `{status, headers, bodyBase64}`。
//!   遇到 `__ipaas_file` envelope 时先把 OSS 里的真实字节取回来，再交给通用
//!   `pipeline_execute`——这样二进制落盘、`206 Range` 续传、JSON 三条路全部自动复用。
//!   （原先这一步在 `IpassProxyRes`/ResponseEnvelope 层做，但 envelope 只能返回
//!   `ApiResponse`，装不下二进制，所以下沉一层。）
//! - **请求侧**：`HttpRequestBody::Multipart` 的文件先上传 OSS，转发时只带
//!   `{url, filename, content_type}` 描述符，由网关侧重建真实 multipart。

use std::borrow::Cow;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use base64::Engine;
use bytes::Bytes;
use serde::Deserialize;
use wecom_transport::{
    ByteStream, Endpoint, EndpointHttpExt, HttpClient, HttpEndpoint, HttpRequest, HttpRequestBody,
    HttpRequestPayload, HttpResponse, HttpTransportBackend, MultipartBody, MultipartField,
    RequestOptions, TransportBackend, TransportResponse, basename_of,
};

use super::envelope::NestedRes;
use crate::env;

const PROXY_PATH: &str = "/ipass-proxy/wecom";
/// OC 侧为 CLI 提供 OSS 上传签名的端点（与 lark-cli 共用）。
const OSS_UPLOAD_PATH: &str = "/oss/upload";
/// OSS 对象来源标记，便于在平台侧区分 wecom 的中转文件。
const FILE_ORIGIN_SCENE: &str = "wecom_cli_ipass_upload";
const IPASS_SESSION_HEADER: &str = "x-ipass-session-id";

/// OC `/oc_adapter/ipass-proxy/wecom` 的传输外壳：`{status, headers, bodyBase64}`。
#[derive(Debug, Deserialize)]
struct IpassProxyResponse {
    /// 内层（真实企业微信网关）状态码。
    status: u16,
    /// 内层响应头，对象形式。
    headers: serde_json::Value,
    #[serde(rename = "bodyBase64")]
    body_base64: String,
}

/// `__ipaas_file` envelope：网关把二进制转存 OSS 后回给 CLI 的取件信息。
#[derive(Debug, Deserialize)]
struct FileEnvelope {
    #[serde(rename = "__ipaas_file")]
    file: FileSpec,
}

#[derive(Debug, Deserialize)]
struct FileSpec {
    oss_url: String,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    filename: Option<String>,
}

/// OC `/oss/upload` 的响应外壳。
#[derive(Debug, Deserialize)]
struct OssUploadEnvelope {
    code: i64,
    #[serde(default)]
    msg: Option<String>,
    #[serde(default)]
    data: Option<OssUploadData>,
}

#[derive(Debug, Deserialize)]
struct OssUploadData {
    #[serde(rename = "uploadUrl")]
    upload_url: String,
    #[serde(rename = "readUrl")]
    read_url: String,
}

/// [yingdao-fork] OC 传输外壳的拆包点。
///
/// 把 OC 回的 `{status, headers, bodyBase64}` 还原成真正的 HTTP 响应，
/// 让下游 `pipeline_execute` 按内层 content-type 自行判定 JSON / 二进制。
#[derive(Debug)]
struct OcAdapterClient {
    inner: reqwest::Client,
}

impl OcAdapterClient {
    fn new() -> Self {
        Self {
            inner: reqwest::Client::new(),
        }
    }
}

impl HttpClient for OcAdapterClient {
    fn send<'a>(
        &'a self,
        req: HttpRequest<'a>,
    ) -> Pin<
        Box<
            dyn Future<Output = std::result::Result<HttpResponse, wecom_transport::Error>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            // `reqwest::Client` 自身实现了 HttpClient，这里复用它现成的发送链路。
            // `reqwest_request` 只读 endpoint / payload / options，不会回读
            // `req.http_client`，因此不存在递归。
            let response = self.inner.send(req).await?;
            unwrap_proxy_response(response).await
        })
    }

    fn name(&self) -> &'static str {
        "ipass-proxy"
    }
}

/// 把 OC 传输外壳还原成「真正的」企业微信 HTTP 响应。
async fn unwrap_proxy_response(
    response: HttpResponse,
) -> std::result::Result<HttpResponse, wecom_transport::Error> {
    let endpoint = response.endpoint().to_string();
    let envelope: IpassProxyResponse = response.json().await?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&envelope.body_base64)
        .map_err(|error| wecom_transport::Error::Other(Box::new(error)))?;

    // 二进制响应：网关已转存 OSS，先把真实字节取回来，再交给通用 pipeline。
    if let Some(spec) = decode_file_envelope(&bytes) {
        let file_bytes = fetch_oss_file(&spec).await?;
        return Ok(HttpResponse::new(
            endpoint,
            envelope.status,
            file_envelope_headers(&spec),
            bytes_stream(file_bytes),
        ));
    }

    Ok(HttpResponse::new(
        endpoint,
        envelope.status,
        header_map(&envelope.headers),
        bytes_stream(bytes),
    ))
}

fn decode_file_envelope(bytes: &[u8]) -> Option<FileSpec> {
    let envelope: FileEnvelope = serde_json::from_slice(bytes).ok()?;
    (!envelope.file.oss_url.trim().is_empty()).then_some(envelope.file)
}

fn file_envelope_headers(spec: &FileSpec) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    let content_type = spec
        .content_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("application/octet-stream");
    if let Ok(value) = reqwest::header::HeaderValue::from_str(content_type) {
        headers.insert(reqwest::header::CONTENT_TYPE, value);
    }
    if let Some(name) = spec
        .filename
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if let Ok(value) =
            reqwest::header::HeaderValue::from_str(&format!("attachment; filename=\"{name}\""))
        {
            headers.insert(reqwest::header::CONTENT_DISPOSITION, value);
        }
    }
    headers
}

async fn fetch_oss_file(spec: &FileSpec) -> std::result::Result<Vec<u8>, wecom_transport::Error> {
    let response = reqwest::Client::new()
        .get(&spec.oss_url)
        .send()
        .await
        .map_err(|error| wecom_transport::Error::Other(Box::new(error)))?;
    let status = response.status();
    if !status.is_success() {
        return Err(wecom_transport::Error::Http {
            message: format!("下载中转文件失败: HTTP {status}"),
            endpoint: spec.oss_url.clone(),
            status: status.as_u16(),
        });
    }
    response
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|error| wecom_transport::Error::Other(Box::new(error)))
}

fn header_map(raw: &serde_json::Value) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    let Some(object) = raw.as_object() else {
        return headers;
    };
    for (name, value) in object {
        let Some(text) = value.as_str() else {
            continue;
        };
        let Ok(name) = reqwest::header::HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(value) = reqwest::header::HeaderValue::from_str(text) else {
            continue;
        };
        headers.insert(name, value);
    }
    headers
}

fn bytes_stream(bytes: Vec<u8>) -> ByteStream {
    Box::pin(futures_util::stream::once(async move {
        Ok(Bytes::from(bytes))
    }))
}

#[derive(Clone, Debug)]
pub(crate) struct IpassProxyBackend {
    http: HttpTransportBackend,
    session_id: Option<String>,
    call_id: Option<String>,
    message_id: Option<String>,
}

impl IpassProxyBackend {
    pub(crate) fn from_env() -> Option<Self> {
        let adapter_url = std::env::var(env::OC_ADAPTER_URL)
            .ok()
            .filter(|value| !value.is_empty())?;
        Some(Self::new(
            adapter_url,
            std::env::var(env::IPASS_SESSION_ID)
                .ok()
                .filter(|value| !value.is_empty()),
            std::env::var(env::IPASS_ACTIVE_CALL_ID)
                .ok()
                .filter(|value| !value.is_empty()),
            std::env::var(env::IPASS_ACTIVE_MESSAGE_ID)
                .ok()
                .filter(|value| !value.is_empty()),
        ))
    }

    pub(crate) fn new(
        adapter_url: String,
        session_id: Option<String>,
        call_id: Option<String>,
        message_id: Option<String>,
    ) -> Self {
        Self {
            // [yingdao-fork] 换上会在 HttpClient 层拆 OC 外壳的客户端；
            // base_url 仍指向 OC adapter。
            http: HttpTransportBackend::default()
                .with_http_client(OcAdapterClient::new())
                .with_base_url(adapter_url.trim_end_matches('/')),
            session_id,
            call_id,
            message_id,
        }
    }

    fn proxy_options(
        &self,
        source: &RequestOptions,
    ) -> Result<RequestOptions, wecom_transport::Error> {
        let session_id = self.session_id.as_deref().ok_or_else(|| {
            wecom_transport::Error::Config(format!(
                "{} is required when {} is set",
                env::IPASS_SESSION_ID,
                env::OC_ADAPTER_URL
            ))
        })?;

        let mut options = RequestOptions::default();
        options.wire.timeout = source.wire.timeout;
        options.wire.headers.insert(
            "x-ipass-session-id",
            session_id.parse().map_err(|error| {
                wecom_transport::Error::Config(format!(
                    "invalid {0}: {error}",
                    env::IPASS_SESSION_ID
                ))
            })?,
        );
        if let Some(call_id) = &self.call_id {
            options.wire.headers.insert(
                "x-ipass-call-id",
                call_id.parse().map_err(|error| {
                    wecom_transport::Error::Config(format!(
                        "invalid {0}: {error}",
                        env::IPASS_ACTIVE_CALL_ID
                    ))
                })?,
            );
        }
        if let Some(message_id) = &self.message_id {
            options.wire.headers.insert(
                "x-ipass-message-id",
                message_id.parse().map_err(|error| {
                    wecom_transport::Error::Config(format!(
                        "invalid {0}: {error}",
                        env::IPASS_ACTIVE_MESSAGE_ID
                    ))
                })?,
            );
        }
        Ok(options)
    }

    /// [yingdao-fork] 把结构化 multipart 转成网关可消费的 JSON 描述符。
    ///
    /// 文件字段先经 OC `/oss/upload` 取签名并 PUT 到对象存储，描述符里只保留
    /// `{url, filename, content_type}`；网关据此重建真实 multipart。
    async fn build_file_bridge_body(
        &self,
        body: MultipartBody,
    ) -> std::result::Result<serde_json::Value, wecom_transport::Error> {
        let session_id = self.session_id.as_deref().ok_or_else(|| {
            wecom_transport::Error::Config(format!(
                "{} is required when {} is set",
                env::IPASS_SESSION_ID,
                env::OC_ADAPTER_URL
            ))
        })?;

        let mut descriptor = serde_json::Map::new();
        for field in body.fields {
            match field {
                MultipartField::Text { name, value } => {
                    descriptor.insert(name, serde_json::Value::String(value));
                }
                MultipartField::File { name, open } => {
                    let opened = open().await?;
                    let filename = basename_of(&opened.path);
                    let read_url = self
                        .upload_file_to_oss(&opened.path, &filename, session_id)
                        .await?;
                    descriptor.insert(
                        name,
                        serde_json::json!({
                            "url": read_url,
                            "filename": filename,
                            // 上游 `open_as_multipart_part` 不设置 Part 的 mime，
                            // reqwest 会回落到 octet-stream；这里保持一致。
                            "content_type": "application/octet-stream",
                        }),
                    );
                }
            }
        }
        Ok(serde_json::Value::Object(descriptor))
    }

    /// [yingdao-fork] 向 OC 申请 OSS 上传签名 → PUT 文件 → 返回可读地址。
    ///
    /// 协议与 lark-cli 的 `ipasstrans/upload.go` 一致。文件先整体读入内存
    /// （飞书侧同样如此）；改成流式需要 reqwest 的 `stream` feature 并解决
    /// 预签名 PUT 的 Content-Length 问题，暂不做。
    async fn upload_file_to_oss(
        &self,
        path: &Path,
        filename: &str,
        session_id: &str,
    ) -> std::result::Result<String, wecom_transport::Error> {
        let file_bytes = tokio::fs::read(path)
            .await
            .map_err(|error| wecom_transport::Error::Other(Box::new(error)))?;

        let client = reqwest::Client::new();
        let endpoint = format!(
            "{}{}",
            self.http.base_url().trim_end_matches('/'),
            OSS_UPLOAD_PATH
        );
        let response = client
            .post(&endpoint)
            .header(IPASS_SESSION_HEADER, session_id)
            .json(&serde_json::json!({
                "fileName": filename,
                "contentType": "application/octet-stream",
                "fileType": "file",
                "fileOriginScene": FILE_ORIGIN_SCENE,
            }))
            .send()
            .await
            .map_err(|error| wecom_transport::Error::Other(Box::new(error)))?;

        let status = response.status();
        let raw = response
            .text()
            .await
            .map_err(|error| wecom_transport::Error::Other(Box::new(error)))?;
        if !status.is_success() {
            return Err(wecom_transport::Error::Http {
                message: format!("申请 OSS 上传签名失败: HTTP {status}"),
                endpoint: OSS_UPLOAD_PATH.to_string(),
                status: status.as_u16(),
            });
        }

        let envelope: OssUploadEnvelope =
            serde_json::from_str(&raw).map_err(|error| wecom_transport::Error::Parse {
                message: format!("Parse OSS upload response failed: {error:#}"),
                endpoint: OSS_UPLOAD_PATH.to_string(),
                body: Box::new(serde_json::Value::String(raw.clone())),
                source: Some(error),
            })?;
        // OC 侧成功时可能返回 0 或 200（与 lark-cli 的判定保持一致）。
        if envelope.code != 0 && envelope.code != 200 {
            return Err(wecom_transport::Error::Api {
                message: envelope
                    .msg
                    .unwrap_or_else(|| "unknown error".to_string()),
                action: OSS_UPLOAD_PATH.to_string(),
                code: Some(envelope.code),
                body: Box::new(serde_json::Value::String(raw)),
            });
        }
        let data = envelope
            .data
            .ok_or_else(|| wecom_transport::Error::Parse {
                message: "OSS upload response is missing data".to_string(),
                endpoint: OSS_UPLOAD_PATH.to_string(),
                body: Box::new(serde_json::Value::Null),
                source: None,
            })?;
        let upload_url = data.upload_url.trim().to_string();
        let read_url = data.read_url.trim().to_string();
        if upload_url.is_empty() || read_url.is_empty() {
            return Err(wecom_transport::Error::Parse {
                message: "OSS upload response is missing uploadUrl/readUrl".to_string(),
                endpoint: OSS_UPLOAD_PATH.to_string(),
                body: Box::new(serde_json::Value::Null),
                source: None,
            });
        }

        let put = client
            .put(&upload_url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(file_bytes)
            .send()
            .await
            .map_err(|error| wecom_transport::Error::Other(Box::new(error)))?;
        let put_status = put.status();
        if !put_status.is_success() {
            return Err(wecom_transport::Error::Http {
                message: format!("上传文件到 OSS 失败: HTTP {put_status}"),
                endpoint: read_url,
                status: put_status.as_u16(),
            });
        }

        Ok(read_url)
    }
}

impl TransportBackend for IpassProxyBackend {
    fn execute<'a>(
        &'a self,
        endpoint: Cow<'a, Endpoint>,
        payload: HttpRequestPayload,
        options: RequestOptions,
    ) -> Pin<Box<dyn Future<Output = Result<TransportResponse, wecom_transport::Error>> + Send + 'a>>
    {
        Box::pin(async move {
            let (body, multipart_bridge) = match payload.build().await? {
                HttpRequestBody::Json(value) => (
                    endpoint.req_envelope().encode(value.as_ref().clone()),
                    false,
                ),
                // [yingdao-fork] 文件桥：文件先上传 OSS，转发 {url, filename} 描述符。
                HttpRequestBody::Multipart(body) => {
                    (self.build_file_bridge_body(body).await?, true)
                }
                HttpRequestBody::Form(_) => {
                    return Err(wecom_transport::Error::Config(
                        "multipart requests are not supported by the OpenCode iPaaS proxy"
                            .to_string(),
                    ));
                }
            };
            let (path, query) = endpoint
                .path()
                .split_once('?')
                .unwrap_or((endpoint.path(), ""));
            let mut headers = serde_json::Map::new();
            for (name, value) in &options.wire.headers {
                if name == reqwest::header::AUTHORIZATION || name == reqwest::header::COOKIE {
                    continue;
                }
                if let Ok(value) = value.to_str() {
                    headers.insert(
                        name.as_str().to_string(),
                        serde_json::Value::String(value.to_string()),
                    );
                }
            }
            if multipart_bridge {
                // 桥接模式下 body 是 JSON 描述符，但对外用 multipart/form-data 作为信号，
                // 让网关侧重建真实 multipart 并自行生成 boundary（所以这里不带 boundary）。
                headers.insert(
                    "content-type".to_string(),
                    serde_json::Value::String("multipart/form-data".to_string()),
                );
            } else {
                headers
                    .entry("content-type".to_string())
                    .or_insert_with(|| serde_json::Value::String("application/json".to_string()));
            }

            let request = serde_json::json!({
                "provider": "wecom",
                "method": "POST",
                "path": path,
                "query": query,
                "headers": headers,
                "bodyBase64": base64::engine::general_purpose::STANDARD
                    .encode(serde_json::to_vec(&body).map_err(|error| {
                        wecom_transport::Error::Other(Box::new(error))
                    })?),
            });
            // 外壳已由 OcAdapterClient 拆掉，这里用真实网关信封校验业务码。
            let proxy_endpoint =
                Endpoint::new().with(HttpEndpoint::new(PROXY_PATH).with_res_envelope(NestedRes));
            self.http
                .execute(
                    Cow::Owned(proxy_endpoint),
                    HttpRequestPayload::json(request),
                    self.proxy_options(&options)?,
                )
                .await
        })
    }

    fn name(&self) -> &str {
        "ipass-proxy"
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use base64::Engine;
    use serde_json::json;
    use wecom::PayloadStringReq;
    use wecom_transport::{
        Endpoint, HttpEndpoint, HttpRequestPayload, MultipartBody, MultipartField, OpenedFilePart,
        OpenFileFn, RequestOptions, TransportBackend, TransportResponse,
    };
    use wiremock::matchers::{header, method, path};
    use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

    use super::IpassProxyBackend;

    /// 测试用：构造一个只读固定字节的文件片段工厂。
    fn static_file_opener(
        path: PathBuf,
        bytes: &'static [u8],
        name: &'static str,
    ) -> Arc<OpenFileFn> {
        Arc::new(move || {
            let path = path.clone();
            Box::pin(async move {
                Ok(OpenedFilePart {
                    part: reqwest::multipart::Part::bytes(bytes.to_vec()).file_name(name),
                    path,
                })
            })
        })
    }

    struct ProxyPayload;

    impl Match for ProxyPayload {
        fn matches(&self, request: &Request) -> bool {
            let Ok(body) = serde_json::from_slice::<serde_json::Value>(&request.body) else {
                return false;
            };
            body == json!({
                "provider": "wecom",
                "method": "POST",
                "path": "/service/discovery",
                "query": "locale=zh_CN",
                "headers": {"content-type": "application/json", "x-trace": "trace-1"},
                "bodyBase64": "eyJwYXlsb2FkIjoie1wic2VydmljZVwiOlwiZG9jXCJ9In0="
            })
        }
    }

    #[tokio::test]
    async fn wraps_a_json_request_for_the_oc_proxy() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oc_adapter/ipass-proxy/wecom"))
            .and(header("x-ipass-session-id", "session-1"))
            .and(header("x-ipass-call-id", "call-1"))
            .and(header("x-ipass-message-id", "message-1"))
            .and(ProxyPayload)
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": 200,
                "headers": {"content-type": "application/json"},
                "bodyBase64": "eyJlcnJjb2RlIjowLCJlcnJtc2ciOiJvayIsInJlc3VsdHNfanNvbiI6IntcInJlc3VsdFwiOlwie1xcXCJva1xcXCI6dHJ1ZX1cIn0ifQ=="
            })))
            .expect(1)
            .mount(&server)
            .await;

        let backend = IpassProxyBackend::new(
            format!("{}/oc_adapter", server.uri()),
            Some("session-1".to_string()),
            Some("call-1".to_string()),
            Some("message-1".to_string()),
        );
        let endpoint = Endpoint::new().with(
            HttpEndpoint::new("/service/discovery?locale=zh_CN")
                .with_req_envelope(PayloadStringReq),
        );
        let mut options = RequestOptions::default();
        options
            .wire
            .headers
            .insert("x-trace", "trace-1".parse().unwrap());
        options.wire.headers.insert(
            reqwest::header::AUTHORIZATION,
            "Bearer local-token".parse().unwrap(),
        );

        let response = backend
            .execute(
                Cow::Owned(endpoint),
                HttpRequestPayload::json(json!({"service": "doc"})),
                options,
            )
            .await
            .unwrap();
        assert_eq!(response.into_result().unwrap(), json!({"ok": true}));
    }

    /// P0：[yingdao-fork] 结构化 multipart 会先传 OSS，再转发描述符。
    /// 条件：MultipartBody 含一个文本字段 + 一个文件字段；OC 签名端点、OSS PUT、代理端点均 mock。
    /// 断言：文件字节到达 OSS；转发外壳的 content-type 为 multipart/form-data；
    /// bodyBase64 解出 `{url, filename, content_type}` 描述符 + 文本字段。
    #[tokio::test]
    async fn multipart_uploads_file_to_oss_and_forwards_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("a.pdf");
        tokio::fs::write(&file_path, b"PDF-BYTES").await.unwrap();

        let server = MockServer::start().await;
        let read_url = format!("{}/read/a.pdf", server.uri());

        Mock::given(method("POST"))
            .and(path("/oc_adapter/oss/upload"))
            .and(header("x-ipass-session-id", "session-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 200,
                "data": {
                    "uploadUrl": format!("{}/put-object", server.uri()),
                    "readUrl": read_url,
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let uploaded: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&uploaded);
        Mock::given(method("PUT"))
            .and(path("/put-object"))
            .respond_with(move |req: &Request| {
                *sink.lock().unwrap() = req.body.clone();
                ResponseTemplate::new(200)
            })
            .expect(1)
            .mount(&server)
            .await;

        let forwarded: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
        let seen = Arc::clone(&forwarded);
        Mock::given(method("POST"))
            .and(path("/oc_adapter/ipass-proxy/wecom"))
            .respond_with(move |req: &Request| {
                *seen.lock().unwrap() = Some(req.body.clone());
                let inner = json!({
                    "errcode": 0,
                    "errmsg": "ok",
                    "results_json": "{\"result\":\"{}\"}",
                });
                ResponseTemplate::new(200).set_body_json(json!({
                    "status": 200,
                    "headers": {"content-type": "application/json"},
                    "bodyBase64": base64::engine::general_purpose::STANDARD
                        .encode(serde_json::to_vec(&inner).unwrap()),
                }))
            })
            .expect(1)
            .mount(&server)
            .await;

        let backend = IpassProxyBackend::new(
            format!("{}/oc_adapter", server.uri()),
            Some("session-1".to_string()),
            None,
            None,
        );
        let open = static_file_opener(file_path, b"PDF-BYTES", "a.pdf");
        let payload = HttpRequestPayload::multipart(move || MultipartBody {
            fields: vec![
                MultipartField::Text {
                    name: "type".to_string(),
                    value: "file".to_string(),
                },
                MultipartField::File {
                    name: "media".to_string(),
                    open: Arc::clone(&open),
                },
            ]
        });

        backend
            .execute(
                Cow::Owned(Endpoint::new().with(HttpEndpoint::new("/file/upload"))),
                payload,
                RequestOptions::default(),
            )
            .await
            .unwrap();

        assert_eq!(uploaded.lock().unwrap().as_slice(), b"PDF-BYTES");

        let raw = forwarded.lock().unwrap().clone().expect("proxy 未被调用");
        let outer: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(outer["path"], json!("/file/upload"));
        assert_eq!(outer["headers"]["content-type"], json!("multipart/form-data"));

        let body = base64::engine::general_purpose::STANDARD
            .decode(outer["bodyBase64"].as_str().unwrap())
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            json!({
                "type": "file",
                "media": {
                    "url": read_url,
                    "filename": "a.pdf",
                    "content_type": "application/octet-stream",
                }
            })
        );
    }

    /// P0：[yingdao-fork] `__ipaas_file` envelope 会被还原成二进制响应。
    /// 条件：OC 返回文件 envelope，OSS 上有真实字节；端点无 range_size。
    /// 断言：得到 TransportResponse::Binary，头部来自 envelope，正文来自 OSS。
    #[tokio::test]
    async fn binary_envelope_is_fetched_from_oss_as_binary_response() {
        let server = MockServer::start().await;
        let file_url = format!("{}/read/a.bin", server.uri());

        Mock::given(method("GET"))
            .and(path("/read/a.bin"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"BINARY-PAYLOAD".to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/oc_adapter/ipass-proxy/wecom"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": 200,
                "headers": {"content-type": "application/json"},
                "bodyBase64": base64::engine::general_purpose::STANDARD.encode(
                    serde_json::to_vec(&json!({
                        "__ipaas_file": {
                            "oss_url": file_url,
                            "content_type": "application/octet-stream",
                            "filename": "a.bin",
                        }
                    }))
                    .unwrap()
                ),
            })))
            .expect(1)
            .mount(&server)
            .await;

        let backend = IpassProxyBackend::new(
            format!("{}/oc_adapter", server.uri()),
            Some("session-1".to_string()),
            None,
            None,
        );
        let endpoint = Endpoint::new().with(
            HttpEndpoint::new("/media/download").with_req_envelope(PayloadStringReq),
        );

        let response = backend
            .execute(
                Cow::Owned(endpoint),
                HttpRequestPayload::json(json!({})),
                RequestOptions::default(),
            )
            .await
            .unwrap();

        match response {
            TransportResponse::Binary(resp) => {
                assert_eq!(
                    resp.headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|value| value.to_str().ok()),
                    Some("application/octet-stream")
                );
                assert_eq!(resp.text().await.unwrap(), "BINARY-PAYLOAD");
            }
            other => panic!("expected TransportResponse::Binary, got {other:?}"),
        }
    }
}
