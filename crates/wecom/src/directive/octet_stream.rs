use std::collections::HashSet;

use super::Directive;
use crate::{fs, json_path, schema};

/// 递归遍历 schema，检查是否存在 `x-wecom-octet-stream: true` 的字段。
pub fn check_has_octet_stream(schema: &schema::JsonSchema) -> bool {
    if schema.directives.octet_stream.is_some() {
        return true;
    }

    for child in schema.properties.values() {
        if check_has_octet_stream(child.as_ref()) {
            return true;
        }
    }

    for child in &schema.one_of {
        if check_has_octet_stream(child.as_ref()) {
            return true;
        }
    }

    if let Some(items) = &schema.items
        && check_has_octet_stream(items.as_ref())
    {
        return true;
    }

    false
}

/// 提取 multipart 上传的文件字段名集合（供构建表单与再次构建闭包复用）。
pub fn multipart_file_fields(directives: &[Directive<'_>]) -> HashSet<String> {
    directives
        .iter()
        .filter_map(|d| match d {
            Directive::UploadMultipart { path, .. } => Some(json_path::segments_to_path(path)),
            _ => None,
        })
        .collect()
}

/// [yingdao-fork] 构造可重放的 `MultipartField::File` 打开工厂。
///
/// 每次物化都会重新走沙箱校验并打开文件，与上游 `HttpRequestPayload::form`
/// 的延迟物化语义一致。
pub fn multipart_file_opener(
    fs: &fs::Fs,
    file_path: &str,
) -> std::sync::Arc<wecom_transport::OpenFileFn> {
    let fs = fs.clone();
    let file_path = file_path.to_string();
    std::sync::Arc::new(move || {
        let fs = fs.clone();
        let file_path = file_path.clone();
        Box::pin(async move {
            fs.open_as_multipart_field(&file_path)
                .await
                .inspect_err(
                    |e| tracing::error!(error = %e, "open file for multipart upload failed"),
                )
                .map_err(crate::util::to_transport_error)
        })
    })
}

/// [yingdao-fork] 按 directives 将 payload 扁平化为**结构化** multipart 载荷。
///
/// 与上游 `build_multipart_form` 的唯一差别是返回 [`wecom_transport::MultipartBody`]
/// 而不是 `reqwest::multipart::Form`：后者不透明，iPaaS 代理后端拿不到文件路径，
/// 无法把文件改道对象存储。直连模式由 `reqwest_send` 物化成等价的 Form，行为不变。
pub fn build_multipart_body(
    fs: &fs::Fs,
    payload: &serde_json::Value,
    file_fields: &HashSet<String>,
) -> wecom_transport::MultipartBody {
    let mut fields = Vec::new();

    for (name, value) in json_path::flatten_value(payload) {
        if file_fields.contains(name.as_str()) {
            fields.push(wecom_transport::MultipartField::File {
                name,
                open: multipart_file_opener(fs, &value),
            });
        } else {
            fields.push(wecom_transport::MultipartField::Text { name, value });
        }
    }

    wecom_transport::MultipartBody { fields }
}

#[cfg(test)]
mod tests {
    //! ## 模块摘要：octet_stream（Octet-Stream 检测与 multipart 构建）
    //!
    //! ### 关键接口
    //! - [check_has_octet_stream] — 递归检查 schema 中是否存在 x-wecom-octet-stream: true 标记
    //! - [build_multipart_body] — 根据 directives 将 payload 扁平化为结构化 multipart 载荷
    //!
    //! ### 关键分支与异常路径
    //! - check_has_octet_stream：顶层标记、properties 嵌套、oneOf 嵌套、items 嵌套四种检测路径；均无则返回 false
    //! - build_multipart_body：文件字段用 multipart_file_opener，普通字段用 text
    //!
    //! ### 上下游交互
    //! - 上游：HTTP 请求构建层调用本模块判断是否需要 multipart 及构建表单
    //! - 下游：依赖 [schema::JsonSchema]（指令标记）、[json_path::flatten_value]（扁平化）、[fs::Fs]（文件读取）

    use std::sync::Arc;

    use super::*;

    fn make_schema() -> schema::JsonSchema {
        schema::JsonSchema::default()
    }

    fn with_octet_stream() -> schema::JsonSchema {
        schema::JsonSchema {
            directives: schema::JsonSchemaWecomDirectives {
                octet_stream: Some(schema::WecomBoolValue::default()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// P1：空 schema 无 octet_stream 标记时返回 false
    /// 条件：使用默认空 schema
    /// 断言：check_has_octet_stream 返回 false
    #[test]
    fn empty_schema_is_false() {
        assert!(!check_has_octet_stream(&make_schema()));
    }

    /// P0：顶层 schema 直接标记 octet_stream 时返回 true
    /// 条件：schema 的 directives.octet_stream.is_some()
    /// 断言：check_has_octet_stream 返回 true
    #[test]
    fn direct_octet_stream() {
        assert!(check_has_octet_stream(&with_octet_stream()));
    }

    /// P0：[check_has_octet_stream] 嵌套在 properties 中的 octet_stream 标记被检测到
    /// 条件：schema 的 property 子 schema 标记了 octet_stream
    /// 断言：check_has_octet_stream 返回 true
    #[test]
    fn octet_stream_in_property() {
        let mut s = make_schema();
        s.properties
            .insert("data".into(), Arc::new(with_octet_stream()));
        assert!(check_has_octet_stream(&s));
    }

    /// P1：[check_has_octet_stream] 嵌套在 oneOf 中的 octet_stream 标记被检测到
    /// 条件：schema 的 oneOf 子 schema 标记了 octet_stream
    /// 断言：check_has_octet_stream 返回 true
    #[test]
    fn octet_stream_in_one_of() {
        let mut s = make_schema();
        s.one_of.push(Arc::new(with_octet_stream()));
        assert!(check_has_octet_stream(&s));
    }

    /// P1：[check_has_octet_stream] 嵌套在 items 中的 octet_stream 标记被检测到
    /// 条件：schema 的 items 子 schema 标记了 octet_stream
    /// 断言：check_has_octet_stream 返回 true
    #[test]
    fn octet_stream_in_items() {
        let mut s = make_schema();
        s.items = Some(Arc::new(with_octet_stream()));
        assert!(check_has_octet_stream(&s));
    }

    /// P1：[check_has_octet_stream] 深层嵌套属性中无 octet_stream 标记时返回 false
    /// 条件：schema 含有嵌套属性但均未标记 octet_stream
    /// 断言：check_has_octet_stream 返回 false
    #[test]
    fn deep_nested_property_not_found() {
        let inner = make_schema();
        let mut s = make_schema();
        s.properties.insert("nested".into(), Arc::new(inner));
        assert!(!check_has_octet_stream(&s));
    }

    // ── build_multipart_body ──

    /// P0：[build_multipart_body] 普通文本字段构建为 Text 字段
    /// 条件：payload 含 key=value 文本字段，无文件字段集合
    /// 断言：返回的载荷含一个 Text 字段
    #[test]
    fn build_multipart_body_text_only() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = crate::fs::Fs::new(tmp.path());
        let payload = serde_json::json!({"name": "test.txt"});
        let file_fields = HashSet::new();
        let body = build_multipart_body(&fs, &payload, &file_fields);
        assert_eq!(body.fields.len(), 1);
        assert!(matches!(
            body.fields[0],
            wecom_transport::MultipartField::Text { .. }
        ));
    }

    /// P0：[multipart_file_fields] 提取 UploadMultipart 指令中的文件字段名
    /// 条件：directives 含两个 UploadMultipart（path 分别为 ["file"] 与 ["nested","data"]）
    /// 断言：返回集合含 "file" 与 "nested.data"
    #[test]
    fn multipart_file_fields_extracts_paths() {
        use crate::json_path::PathSegment;

        let directives = vec![
            Directive::UploadMultipart {
                path: vec![PathSegment::Key("file".into())],
                file_path: "/tmp/a.bin".into(),
            },
            Directive::UploadMultipart {
                path: vec![
                    PathSegment::Key("nested".into()),
                    PathSegment::Key("data".into()),
                ],
                file_path: "/tmp/b.bin".into(),
            },
        ];
        let fields = multipart_file_fields(&directives);
        assert!(fields.contains("file"));
        assert!(fields.contains("nested.data"));
    }
}
