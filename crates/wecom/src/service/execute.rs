use indexmap::IndexMap;
use tracing::field::Empty;
use wecom_transport::{HttpRequestPayload, TransportResponse};

use super::{MethodHandle, RunOptions, output, schema_util};
use crate::{CliRunOutput, Error, Result, directive, fs, schema};

// ─── Main execute function ──────────────────────────────────────

/// Execute a method call and handle its response.
///
/// Covers request preparation, directives, media uploads, pagination,
/// and output routing (file / stdout, binary vs JSON).
///
/// ## Tracing
/// Opens a `service.execute` span with `action` and `paged` fields.
/// Downstream directive, media-upload, and transport events are nested
/// under this span for per-call correlation. Errors are recorded at WARN level.
#[tracing::instrument(
    level = "info",
    name = "service.execute",
    skip_all,
    fields(action = %method.path().join("."), paged = Empty),
)]
pub(super) async fn execute_and_output(
    method: &MethodHandle<'_>,
    mut options: RunOptions<'_>,
) -> Result<()> {
    let client = method.client;

    let output = options.run.get_output();

    let transport_backend = client.transport().name();
    tracing::info!(transport = transport_backend, "executing method");

    // 收集请求 directives
    let request_schema = method.request_schema();
    let (directives, multipart) = if let Some(schema) = &request_schema {
        let directives =
            directive::collect_directives(&method.service_schema.schemas, schema, &options.payload);
        let multipart = directive::check_has_octet_stream(schema);
        tracing::debug!(
            directives_count = directives.len(),
            multipart,
            "request directives collected",
        );
        (directives, multipart)
    } else {
        (vec![], false)
    };

    let fs = options.run.fs();

    // 预留 / 校验 output 路径（含沙箱根纠错）
    let mut output_file = match options.output_path.as_deref() {
        Some(path) => {
            let corrected = fs.resolve_writable_or_suggest(path).await?;
            Some(output::create_output_file(fs, &corrected).await?)
        }
        None => None,
    };

    // 校验 output_dir 合法性（不做路径解析）
    if let Some(path) = &options.output_dir {
        fs.resolve_dir_writable_or_suggest(path)
            .await
            .inspect_err(|e| tracing::error!(error = %e, "sandbox check dir writable failed"))?;
    }

    // 处理 media upload
    if !directives.is_empty() {
        tracing::debug!(
            directives_count = directives.len(),
            "processing media upload directives",
        );
        directive::process_media_upload(
            client,
            fs,
            &mut options.payload,
            &directives,
            options.run.get_options(),
        )
        .await?;
    }

    // 检查 form-data 上传文件总大小不超过 100 MB
    check_multipart_body_size(fs, &directives, 100 * 1024 * 1024).await?;

    // ── 第一页请求 ──────────────────────────────────────────

    let on_extra_data = options.run.get_on_extra_data();

    let payload = if multipart {
        // [yingdao-fork] 结构化 multipart（延迟物化）：每次发送/重放时重新打开文件。
        // 直连模式由 `reqwest_send` 物化为 `reqwest::multipart::Form`；
        // iPaaS 代理模式则把文件改道 OSS，只转发描述符。
        let fs = fs.clone();
        let payload = options.payload.clone();
        let file_fields = directive::multipart_file_fields(&directives);
        HttpRequestPayload::multipart(move || {
            directive::build_multipart_body(&fs, &payload, &file_fields)
        })
    } else {
        // JSON 直值：构造时一次 Arc，重放零拷贝。
        HttpRequestPayload::json(options.payload.clone())
    };

    let request = client
        .transport()
        .invoke(method.endpoint(), payload)
        .with_options(options.run.get_options().clone());

    let mut data = match request.execute().await? {
        TransportResponse::Json(output) => {
            tracing::debug!(extra_count = output.extra.len(), "JSON response received");
            if let Some(cb) = on_extra_data
                && !output.extra.is_empty()
            {
                cb(&output.extra);
            }
            output.result
        }
        TransportResponse::Binary(response) => {
            tracing::info!(
                content_length = response.content_length(),
                "binary response received, writing to output"
            );
            // 二进制响应：不分页，二进制流落盘
            let result = output::handle_binary_output(
                &options,
                response,
                output_file,
                &method.method_path_segments,
            )
            .await?;

            output.print(&serde_json::to_string_pretty(&result).unwrap_or_default());
            return Ok(());
        }
    };

    // ── JSON 响应后续处理 ───────────────────────────────────

    let response_schema =
        schema_util::resolve_schema_ref(&method.service_schema.schemas, &method.schema.response);

    process_response_directives(
        &options,
        &mut data,
        &method.service_schema.schemas,
        &response_schema,
    )
    .await?;

    // 单页模式：直接输出
    if multipart || options.page_count.is_none() {
        tracing::info!(
            reason = if multipart {
                "multipart"
            } else {
                "no page count"
            },
            "single page mode, outputting result",
        );
        let result = output::handle_json_output(
            fs,
            data,
            output_file,
            options.output_dir.as_deref(),
            &method.method_path_segments,
        )
        .await?;

        output.print(&serde_json::to_string_pretty(&result).unwrap_or_default());
        return Ok(());
    }

    // 分页模式：NDJSON 追加写入
    tracing::Span::current().record("paged", true);
    tracing::info!(max_pages = options.page_count, "entering paged mode",);

    // 确定写入目标：--output > --output-dir 下创建文件 > stdout
    if output_file.is_none()
        && let Some(dir) = &options.output_dir
    {
        let name = format!("{}.ndjson", method.method_path_segments.join("_"));
        output_file = Some(output::create_output_file_unique(fs, dir.join(name)).await?);
    }

    // 输出
    write_page(output, &data, &mut output_file)?;

    // 翻页循环（从第 2 页开始）
    let mut total_pages: u32 = 1;
    for (n, _) in (1..options.page_count.unwrap_or(1)).enumerate() {
        let Some(next_cursor) = extract_next_cursor(&data) else {
            tracing::info!(
                total_pages = total_pages,
                "no next cursor, pagination stopped early",
            );
            break;
        };

        tracing::info!(page = %n + 2, cursor = %next_cursor, "Fetching page");

        // 页间延迟
        tokio::time::sleep(std::time::Duration::from_millis(options.page_delay_ms)).await;

        if let Some(obj) = options.payload.as_object_mut() {
            obj.insert("cursor".to_string(), serde_json::Value::String(next_cursor));
        }

        let page_output = client
            .transport()
            .invoke(method.endpoint(), &options.payload)
            .with_options(options.run.get_options().clone())
            .execute()
            .await?
            .into_json()?;
        if let Some(cb) = on_extra_data
            && !page_output.extra.is_empty()
        {
            cb(&page_output.extra);
        }
        data = page_output.result;

        process_response_directives(
            &options,
            &mut data,
            &method.service_schema.schemas,
            &response_schema,
        )
        .await?;
        write_page(output, &data, &mut output_file)?;
        total_pages += 1;
    }

    tracing::info!(total_pages = total_pages, "pagination complete");

    // 分页完成，如果写入了文件则打印路径信息
    if let Some(output_file) = &mut output_file {
        output
            .print(&serde_json::to_string_pretty(&output_file.result_ndjson()).unwrap_or_default());
    }

    Ok(())
}

/// 校验 form-data（`UploadMultipart`）文件总大小不超过 `limit`。
///
/// 逐个获取文件元信息并累加大小，任一文件超过 `limit` 即返回错误。
async fn check_multipart_body_size(
    fs: &fs::Fs,
    directives: &[directive::Directive<'_>],
    limit: u64,
) -> Result<()> {
    let file_paths: Vec<_> = directives
        .iter()
        .filter_map(|d| match d {
            directive::Directive::UploadMultipart { file_path, .. } => Some(file_path.as_str()),
            _ => None,
        })
        .collect();

    if file_paths.is_empty() {
        return Ok(());
    }

    let mut total_size: u64 = 0;

    for file_path in &file_paths {
        let file_size = fs.metadata(file_path).await?.len();
        total_size = total_size.saturating_add(file_size);

        if total_size > limit {
            return Err(Error::Validation(format!(
                "请求文件总大小超过限制（{:.1} MB）",
                limit as f64 / 1_048_576.0
            )));
        }
    }

    Ok(())
}

/// 输出一页 NDJSON 数据：有文件则追加写入，否则通过 output.print() 输出。
fn write_page(
    output: &CliRunOutput,
    data: &serde_json::Value,
    output_file: &mut Option<output::OutputFileInfo>,
) -> Result<()> {
    if let Some(f) = output_file {
        f.write_line(&serde_json::to_string(data).unwrap_or_default())?;
    } else {
        output.print(&data.to_string());
    }
    Ok(())
}

/// 对 JSON 响应执行 file-save 指令。
async fn process_response_directives(
    options: &RunOptions<'_>,
    data: &mut serde_json::Value,
    schemas: &IndexMap<String, schema::JsonSchema>,
    response_schema: &Option<schema::JsonSchema>,
) -> Result<()> {
    let Some(schema) = response_schema else {
        return Ok(());
    };
    for directive in &directive::collect_directives(schemas, schema, data) {
        directive::process_file_save(options, data, directive).await?;
    }
    Ok(())
}

/// Inspect a JSON response and extract the next-page cursor.
///
/// Returns `Some(cursor)` when the response carries `has_more: true` **and**
/// a non-empty `next_cursor` string; otherwise returns `None`.
fn extract_next_cursor(data: &serde_json::Value) -> Option<String> {
    let has_more = data
        .get("has_more")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    if !has_more {
        return None;
    }

    data.get("next_cursor")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    //! ## 模块摘要：execute（请求执行与输出处理）
    //!
    //! ### 关键接口
    //! - [execute_and_output] — 执行请求并处理响应（JSON/二进制、分页）
    //! - [extract_next_cursor] — 从 JSON 响应中提取下一页游标
    //! - [write_page] — 输出一页 NDJSON 数据
    //!
    //! ### 关键分支与异常路径
    //! - execute_and_output：HTTP 传输分支；JSON/二进制响应分支；单页/分页分支
    //! - extract_next_cursor：has_more 为 false、next_cursor 为空、缺少字段等分支
    //!
    //! ### 上下游交互
    //! - 上游：[service_handle::handle_service_cmd] 调用本模块执行请求
    //! - 下游：依赖 [wecom_transport::Transport]、[output] 模块、[directive] 模块

    use serde_json::json;

    use super::*;
    use crate::directive::Directive;
    use crate::json_path::PathSegment;

    // extract_next_cursor

    /// P0：[extract_next_cursor] 在 has_more=true 且有 next_cursor 时返回游标
    /// 条件：JSON 中 has_more 为 true，next_cursor 为 "abc123"
    /// 断言：返回 Some("abc123")
    #[test]
    fn extract_cursor_has_more_true_with_cursor() {
        let data = json!({"has_more": true, "next_cursor": "abc123"});
        assert_eq!(extract_next_cursor(&data), Some("abc123".into()));
    }

    /// P1：[extract_next_cursor] 在 has_more=false 时返回 None
    /// 条件：JSON 中 has_more 为 false（有 next_cursor）
    /// 断言：返回 None
    #[test]
    fn extract_cursor_has_more_false() {
        let data = json!({"has_more": false, "next_cursor": "abc123"});
        assert_eq!(extract_next_cursor(&data), None);
    }

    /// P1：[extract_next_cursor] 在 has_more=true 但 next_cursor 为空时返回 None
    /// 条件：JSON 中 has_more 为 true，next_cursor 为 ""
    /// 断言：返回 None（空游标视为无效）
    #[test]
    fn extract_cursor_has_more_true_empty_cursor() {
        let data = json!({"has_more": true, "next_cursor": ""});
        assert_eq!(extract_next_cursor(&data), None);
    }

    /// P1：[extract_next_cursor] 在缺少 has_more 字段时返回 None
    /// 条件：JSON 中只有 next_cursor，无 has_more
    /// 断言：返回 None（has_more 默认 false）
    #[test]
    fn extract_cursor_no_has_more_field() {
        let data = json!({"next_cursor": "abc"});
        assert_eq!(extract_next_cursor(&data), None);
    }

    /// P1：[extract_next_cursor] 在缺少 next_cursor 字段时返回 None
    /// 条件：JSON 中 has_more 为 true，无 next_cursor 字段
    /// 断言：返回 None
    #[test]
    fn extract_cursor_no_next_cursor_field() {
        let data = json!({"has_more": true});
        assert_eq!(extract_next_cursor(&data), None);
    }

    /// P1：[extract_next_cursor] 在空 JSON 对象时返回 None
    /// 条件：JSON 为 {}
    /// 断言：返回 None
    #[test]
    fn extract_cursor_empty_object() {
        assert_eq!(extract_next_cursor(&json!({})), None);
    }

    // ── check_multipart_body_size ──

    /// P0：[check_multipart_body_size] 无 UploadMultipart 指令时返回 Ok
    /// 条件：directives 为空或不含 UploadMultipart
    /// 断言：返回 Ok(())
    #[tokio::test]
    async fn check_multipart_body_size_empty_directives_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = crate::fs::Fs::new(tmp.path());
        let result = check_multipart_body_size(&fs, &[], 1024).await;
        assert!(result.is_ok());
    }

    /// P0：[check_multipart_body_size] 文件总大小在限制内时返回 Ok
    /// 条件：两个文件共 20 字节，limit = 1024
    /// 断言：返回 Ok(())
    #[tokio::test]
    async fn check_multipart_body_size_under_limit_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = crate::fs::Fs::new(tmp.path());
        let file_a = tmp.path().join("a.txt");
        let file_b = tmp.path().join("b.txt");
        fs.atomic_write(&file_a, b"1234567890", 0o644)
            .await
            .unwrap();
        fs.atomic_write(&file_b, b"abcdefghij", 0o644)
            .await
            .unwrap();

        let directives = vec![
            Directive::UploadMultipart {
                path: vec![PathSegment::Key("file_a".into())],
                file_path: file_a.to_string_lossy().to_string(),
            },
            Directive::UploadMultipart {
                path: vec![PathSegment::Key("file_b".into())],
                file_path: file_b.to_string_lossy().to_string(),
            },
        ];

        let result = check_multipart_body_size(&fs, &directives, 1024).await;
        assert!(result.is_ok());
    }

    /// P0：[check_multipart_body_size] 文件总大小超过限制时返回 Err
    /// 条件：两个文件共 20 字节，limit = 15
    /// 断言：返回 Err，错误信息包含大小和限制
    #[tokio::test]
    async fn check_multipart_body_size_exceeds_limit_err() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = crate::fs::Fs::new(tmp.path());
        let file_a = tmp.path().join("a.txt");
        let file_b = tmp.path().join("b.txt");
        fs.atomic_write(&file_a, b"1234567890", 0o644)
            .await
            .unwrap();
        fs.atomic_write(&file_b, b"abcdefghij", 0o644)
            .await
            .unwrap();

        let directives = vec![
            Directive::UploadMultipart {
                path: vec![PathSegment::Key("file_a".into())],
                file_path: file_a.to_string_lossy().to_string(),
            },
            Directive::UploadMultipart {
                path: vec![PathSegment::Key("file_b".into())],
                file_path: file_b.to_string_lossy().to_string(),
            },
        ];

        let result = check_multipart_body_size(&fs, &directives, 15).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("超过限制"),
            "error should mention exceeding limit"
        );
    }

    /// P1：[check_multipart_body_size] 混入 UploadMedia 指令时仅统计 UploadMultipart
    /// 条件：directives 含 1 个 UploadMedia + 1 个 UploadMultipart（10 字节），limit = 15
    /// 断言：返回 Ok（UploadMedia 不计入 form-data 大小限制）
    #[tokio::test]
    async fn check_multipart_body_size_ignores_upload_media() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = crate::fs::Fs::new(tmp.path());
        let file_a = tmp.path().join("a.txt");
        let file_b = tmp.path().join("b.txt");
        fs.atomic_write(&file_a, b"1234567890", 0o644)
            .await
            .unwrap();
        fs.atomic_write(&file_b, b"1234567890", 0o644)
            .await
            .unwrap();

        let directives = vec![
            Directive::UploadMedia {
                path: vec![PathSegment::Key("media".into())],
                file_path: file_a.to_string_lossy().to_string(),
                with_file_path: false,
            },
            Directive::UploadMultipart {
                path: vec![PathSegment::Key("form".into())],
                file_path: file_b.to_string_lossy().to_string(),
            },
        ];

        // UploadMultipart 文件 10 字节 < limit 15
        let result = check_multipart_body_size(&fs, &directives, 15).await;
        assert!(result.is_ok());
    }
}
