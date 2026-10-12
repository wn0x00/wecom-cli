/// HTTP/reqwest-specific helpers that build on top of the core [`super::Fs`]
/// sandbox.
use std::path::Path;

use futures::StreamExt;
use tokio::io::AsyncWriteExt;

use super::sandbox;
use crate::{Error, Result};

// ── Multipart upload ────────────────────────────────────────

impl super::Fs {
    /// Resolve a path, TOCTOU-safe open for reading, and wrap into a
    /// [`reqwest::multipart::Part`] ready for upload.
    pub async fn open_as_multipart_part(
        &self,
        file_path: &str,
    ) -> Result<reqwest::multipart::Part> {
        let resolved = self.resolve_readable_or_suggest(file_path).await?;
        let (resolved, std_file) = sandbox::open_file(&resolved, self.readable_dirs())?;

        let file = tokio::fs::File::from_std(std_file);
        let len = file.metadata().await.map(|m| m.len()).ok();

        let file_name = resolved
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "upload".to_string());

        let part = match len {
            Some(len) => reqwest::multipart::Part::stream_with_length(file, len),
            None => reqwest::multipart::Part::stream(file),
        }
        .file_name(file_name);

        Ok(part)
    }

    /// [yingdao-fork] 与 [`Self::open_as_multipart_part`] 等价，但额外返回已通过
    /// 沙箱校验的绝对路径——iPaaS 文件桥需要路径去上传对象存储。
    ///
    /// 两处都调用 `sandbox::open_file`，因此安全语义完全一致；
    /// 这里复用 `resolve_readable_or_suggest`，错误提示也与上游相同。
    pub async fn open_as_multipart_field(
        &self,
        file_path: &str,
    ) -> Result<wecom_transport::OpenedFilePart> {
        let resolved = self.resolve_readable_or_suggest(file_path).await?;
        let (resolved, std_file) = sandbox::open_file(&resolved, self.readable_dirs())?;

        let file = tokio::fs::File::from_std(std_file);
        let len = file.metadata().await.map(|m| m.len()).ok();

        let file_name = resolved
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "upload".to_string());

        let part = match len {
            Some(len) => reqwest::multipart::Part::stream_with_length(file, len),
            None => reqwest::multipart::Part::stream(file),
        }
        .file_name(file_name);

        Ok(wecom_transport::OpenedFilePart { part, path: resolved })
    }
}

// ── Stream download ─────────────────────────────────────────

/// Stream a [`wecom_transport::HttpResponse`] body into `file`, then `sync_all`.
///
/// This is a pure I/O helper with no sandbox checks — the caller is
/// responsible for ensuring that `file` was opened through a
/// sandbox-checked path (e.g. via [`super::Fs::create_file`]).
pub async fn stream_to_file(
    file: &mut tokio::fs::File,
    display_path: &Path,
    response: wecom_transport::HttpResponse,
) -> Result<()> {
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(Error::Transport)?;
        file.write_all(&chunk)
            .await
            .map_err(|e| Error::io(format!("Failed to write to {}", display_path.display()), e))?;
    }

    file.sync_all()
        .await
        .map_err(|e| Error::io(format!("Failed to sync {}", display_path.display()), e))?;

    Ok(())
}

// ── Content-Disposition ─────────────────────────────────────

/// Extract a filename from the `Content-Disposition` header of an HTTP header map.
pub fn content_disposition_filename(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let header = headers.get("content-disposition")?;
    let value = header.to_str().ok()?;
    super::sanitize::content_disposition_filename(value)
}

// ══════════════════════════════════════════════════════════════
//  Tests
// ══════════════════════════════════════════════════════════════

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    //! ## 模块摘要：reqwest_ext（Reqwest 扩展：文件上传与响应头解析）
    //!
    //! ### 关键接口
    //! - [open_as_multipart_part] — 在沙箱内打开文件用于 multipart 上传
    //! - [content_disposition_filename] — 从响应头的 Content-Disposition 中提取文件名
    //!
    //! ### 关键分支与异常路径
    //! - 文件在沙箱内 → 成功打开
    //! - 文件在沙箱外 → 返回 Err
    //! - 文件不存在 → 返回 Err
    //! - 有 filename*（RFC 5987）→ 优先使用并正确解码
    //! - 无引号 filename → 仍能提取
    //! - 无 Content-Disposition 头 → 返回 None
    //!
    //! ### 上下游交互
    //! - 上游：[directive::UploadMultipart] 调用 [open_as_multipart_part]
    //! - 下游：依赖 [Fs::open_file] 做沙箱校验

    use std::fs as stdfs;

    use tempfile::TempDir;

    use super::super::Fs;
    use super::*;

    // ── open_as_multipart_part ──

    /// P0：[open_as_multipart_part] 成功打开沙盒内文件
    /// 条件：文件位于可读写根目录下
    /// 断言：返回 Ok，Part 可正常创建
    #[tokio::test]
    async fn open_as_multipart_part_success() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("upload.bin");
        stdfs::write(&file, "binary data").unwrap();

        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let part = fs.open_as_multipart_part(file.to_str().unwrap()).await;
        assert!(part.is_ok());
    }

    /// P1：[open_as_multipart_part] open_as_multipart_part 拒绝沙盒外文件
    /// 条件：文件位于 forbidden 目录（不在 allowed roots 内）
    /// 断言：返回 Err
    #[tokio::test]
    async fn open_as_multipart_part_rejects_outside_roots() {
        let allowed = TempDir::new().unwrap();
        let forbidden = TempDir::new().unwrap();
        let file = forbidden.path().join("secret.bin");
        stdfs::write(&file, "secret").unwrap();

        let fs = Fs::new_with_permissions(
            allowed.path(),
            Some(&[allowed.path()]),
            Some(&[allowed.path()]),
        );
        let result = fs.open_as_multipart_part(file.to_str().unwrap()).await;
        assert!(result.is_err());
    }

    /// P1：[open_as_multipart_part] open_as_multipart_part 对不存在的文件返回错误
    /// 条件：请求打开的文件在沙盒内不存在
    /// 断言：返回 Err
    #[tokio::test]
    async fn open_as_multipart_part_nonexistent_file() {
        let tmp = TempDir::new().unwrap();
        let fs = Fs::new_with_permissions(tmp.path(), Some(&[tmp.path()]), Some(&[tmp.path()]));
        let result = fs
            .open_as_multipart_part(tmp.path().join("no-such-file").to_str().unwrap())
            .await;
        assert!(result.is_err());
    }

    /// P1：[open_as_multipart_part] 拼写错误路径通过模糊纠正后成功打开
    /// 条件：文件实际路径为 /root/sub/data.txt，输入路径拼写为 /rrot/sb/data.txt
    /// 断言：模糊纠错成功，返回 Ok(Part)
    #[tokio::test]
    async fn open_as_multipart_part_fuzzy_correction() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        let sub = workspace.join("sub");
        stdfs::create_dir_all(&sub).unwrap();
        stdfs::write(sub.join("data.txt"), "fuzzy upload test").unwrap();

        let fs = Fs::new_with_permissions(
            &workspace,
            Some(&[workspace.as_path()]),
            Some(&[tmp.path()]),
        );
        // Typo path: "wrkspace" (1-char deletion) + "sb" (1-char substitution)
        let typo_path = tmp.path().join("wrkspace/sb/data.txt");
        let result = fs.open_as_multipart_part(typo_path.to_str().unwrap()).await;
        assert!(
            result.is_ok(),
            "fuzzy correction should succeed for typo path"
        );
    }

    // ── content_disposition_filename ──

    /// P0：[content_disposition_filename] 简单带引号 filename 正确提取
    /// 条件：响应头 "attachment; filename=\"report.pdf\""
    /// 断言：返回 Some("report.pdf")
    #[test]
    fn content_disposition_filename_simple() {
        let response = http::Response::builder()
            .status(200)
            .header("content-disposition", "attachment; filename=\"report.pdf\"")
            .body(Vec::<u8>::new())
            .unwrap();
        let response = reqwest::Response::from(response);
        assert_eq!(
            content_disposition_filename(response.headers()).as_deref(),
            Some("report.pdf")
        );
    }

    /// P1：[content_disposition_filename] filename*（RFC 5987）优先于 filename 被使用
    /// 条件：同时包含 filename="fallback.pdf" 和 filename*=中文文件名
    /// 断言：返回 filename* 解析出的中文名
    #[test]
    fn content_disposition_filename_star_takes_precedence() {
        let response = http::Response::builder()
            .status(200)
            .header(
                "content-disposition",
                "attachment; filename=\"fallback.pdf\"; filename*=UTF-8''%E6%96%87%E4%BB%B6.pdf",
            )
            .body(Vec::<u8>::new())
            .unwrap();
        let response = reqwest::Response::from(response);
        assert_eq!(
            content_disposition_filename(response.headers()).as_deref(),
            Some("文件.pdf")
        );
    }

    /// P1：[content_disposition_filename] 无引号的 filename 也能正确提取
    /// 条件：响应头 "attachment; filename=data.csv"（无引号）
    /// 断言：返回 Some("data.csv")
    #[test]
    fn content_disposition_filename_unquoted() {
        let response = http::Response::builder()
            .status(200)
            .header("content-disposition", "attachment; filename=data.csv")
            .body(Vec::<u8>::new())
            .unwrap();
        let response = reqwest::Response::from(response);
        assert_eq!(
            content_disposition_filename(response.headers()).as_deref(),
            Some("data.csv")
        );
    }

    /// P2：[content_disposition_filename] 无 Content-Disposition 头时返回 None
    /// 条件：响应不含任何 Content-Disposition 头
    /// 断言：返回 None
    #[test]
    fn content_disposition_no_header_returns_none() {
        let response = http::Response::builder()
            .status(200)
            .body(Vec::<u8>::new())
            .unwrap();
        let response = reqwest::Response::from(response);
        assert!(content_disposition_filename(response.headers()).is_none());
    }

    /// P2：[content_disposition_filename] Content-Disposition 无 filename 参数时返回 None
    /// 条件：响应头仅为 "inline"（无 filename/filename*）
    /// 断言：返回 None
    #[test]
    fn content_disposition_no_filename_param_returns_none() {
        let response = http::Response::builder()
            .status(200)
            .header("content-disposition", "inline")
            .body(Vec::<u8>::new())
            .unwrap();
        let response = reqwest::Response::from(response);
        assert!(content_disposition_filename(response.headers()).is_none());
    }

    /// P2：[content_disposition_filename] 空字符串 filename 返回 None
    /// 条件：响应头 filename=""
    /// 断言：返回 None
    #[test]
    fn content_disposition_empty_filename_returns_none() {
        let response = http::Response::builder()
            .status(200)
            .header("content-disposition", "attachment; filename=\"\"")
            .body(Vec::<u8>::new())
            .unwrap();
        let response = reqwest::Response::from(response);
        assert!(content_disposition_filename(response.headers()).is_none());
    }

    /// P1：[content_disposition_filename] 仅有 filename*（无 filename）时正确解析
    /// 条件：响应头仅含 "filename*=UTF-8''hello%20world.txt"
    /// 断言：返回 Some("hello world.txt")
    #[test]
    fn content_disposition_only_filename_star() {
        let response = http::Response::builder()
            .status(200)
            .header(
                "content-disposition",
                "attachment; filename*=UTF-8''hello%20world.txt",
            )
            .body(Vec::<u8>::new())
            .unwrap();
        let response = reqwest::Response::from(response);
        assert_eq!(
            content_disposition_filename(response.headers()).as_deref(),
            Some("hello world.txt")
        );
    }

    /// P2：不含等号的段被静默忽略 [[content_disposition_filename]]
    /// 条件：响应头中包含 "noequals" 段和正常的 filename 段
    /// 断言：仍正确解析出 filename="ok.txt"
    #[test]
    fn content_disposition_segment_without_equals() {
        // A segment with no '=' should be silently ignored.
        let response = http::Response::builder()
            .status(200)
            .header(
                "content-disposition",
                "attachment; noequals; filename=\"ok.txt\"",
            )
            .body(Vec::<u8>::new())
            .unwrap();
        let response = reqwest::Response::from(response);
        assert_eq!(
            content_disposition_filename(response.headers()).as_deref(),
            Some("ok.txt")
        );
    }
}
