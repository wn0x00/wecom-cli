//! OpenCode iPaaS proxy backend for JSON WeCom API requests.

use std::borrow::Cow;
use std::future::Future;
use std::pin::Pin;

use base64::Engine;
use serde::Deserialize;
use wecom_transport::{
    Endpoint, EndpointHttpExt, HttpEndpoint, HttpRequestBody, HttpRequestPayload,
    HttpTransportBackend, RequestOptions, ResponseEnvelope, TransportBackend, TransportResponse,
};

use super::envelope::NestedRes;
use crate::env;

const PROXY_PATH: &str = "/ipass-proxy/wecom";

#[derive(Debug, Deserialize)]
struct IpassProxyResponse {
    #[allow(dead_code)]
    status: u16,
    #[allow(dead_code)]
    headers: serde_json::Value,
    #[serde(rename = "bodyBase64")]
    body_base64: String,
}

/// Decodes the raw HTTP response returned by the iPaaS connector, then lets
/// the existing WeCom gateway envelope validate its business result.
#[derive(Debug, Clone, Copy, Default)]
struct IpassProxyRes;

impl ResponseEnvelope for IpassProxyRes {
    fn decode(
        &self,
        url: &str,
        body: serde_json::Value,
    ) -> Result<wecom_transport::backend::protocol::ApiResponse, wecom_transport::Error> {
        let response: IpassProxyResponse =
            serde_json::from_value(body).map_err(|error| wecom_transport::Error::Parse {
                message: format!("Parse iPaaS proxy response failed for {url}: {error:#}"),
                endpoint: url.to_string(),
                body: Box::new(serde_json::Value::Null),
                source: Some(error),
            })?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&response.body_base64)
            .map_err(|error| wecom_transport::Error::Other(Box::new(error)))?;
        let response_body =
            serde_json::from_slice(&bytes).map_err(|error| wecom_transport::Error::Parse {
                message: format!("Parse iPaaS proxy response body failed for {url}: {error:#}"),
                endpoint: url.to_string(),
                body: Box::new(serde_json::Value::String(
                    String::from_utf8_lossy(&bytes).into_owned(),
                )),
                source: Some(error),
            })?;
        NestedRes.decode(url, response_body)
    }

    fn name(&self) -> &'static str {
        "ipass-proxy"
    }
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
            http: HttpTransportBackend::default().with_base_url(adapter_url.trim_end_matches('/')),
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
            let body = match payload.build().await? {
                HttpRequestBody::Json(value) => {
                    endpoint.req_envelope().encode(value.as_ref().clone())
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
            headers
                .entry("content-type".to_string())
                .or_insert_with(|| serde_json::Value::String("application/json".to_string()));

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
            let proxy_endpoint = Endpoint::new()
                .with(HttpEndpoint::new(PROXY_PATH).with_res_envelope(IpassProxyRes));
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

    use serde_json::json;
    use wecom::PayloadStringReq;
    use wecom_transport::{
        Endpoint, HttpEndpoint, HttpRequestPayload, RequestOptions, TransportBackend,
    };
    use wiremock::matchers::{header, method, path};
    use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

    use super::IpassProxyBackend;

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
}
