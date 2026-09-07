//! Session export — official same-origin HTTP route (REQ-007 V0.4; read from
//! `dsh-session-log-export`).
//!
//! Wire knowledge (centralized here per Notes/03):
//! - there is NO Remote export RPC (0.1.2-rc.1 typert enumeration confirmed);
//!   the official implementation is a **web-only HTTP route**
//!   `GET/HEAD {base}/api/session.export?sessionId=X&includeDescendants=true`
//!   (same-origin auth via the in-memory cookie jar, streamed ZIP):
//!   `session.jsonl` at the root + `subagents/<id>/...` + `media/...`
//!   (DEFLATE default 6, no manifest);
//! - this download is BYTE-IDENTICAL to the official export (highest
//!   fidelity); the page-rebuild JSONL fallback is deferred ~nice-to-have
//!   (TASK-007 Step 14 scope decision — format not verifiable headless);
//! - streaming: chunks are written to a temp file in the TARGET directory and
//!   renamed on success (atomic; never whole-file in memory — RSS budget).

use std::path::Path;

use tokio::io::AsyncWriteExt;

use super::envelope::ClientError;

/// Result of a streamed export download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportReceipt {
    pub bytes: u64,
    pub final_path: std::path::PathBuf,
}

/// `GET {base}/api/session.export` — stream the official ZIP to `path`.
/// `include_descendants` defaults true (subagent logs + media). Writes to
/// `path.tmp-<pid>` first then atomically renames (same filesystem; never
/// leaves a half-written file at the destination). Convenience wrapper
/// (no progress / never cancels) — see `download_export_progress`.
pub async fn download_export(
    http: &reqwest::Client,
    base: &str,
    session_id: &str,
    path: &Path,
) -> Result<ExportReceipt, ClientError> {
    download_export_progress(http, base, session_id, path, &mut |_| {}, || false).await
}

/// Streaming variant with per-chunk progress + cooperative cancellation.
/// `on_progress` is invoked with the total bytes streamed so far; when
/// `is_cancelled()` turns true the download aborts, the tmp file is removed
/// and `ClientError::Transport("导出已取消")` is returned (AC-007-17: 中途
/// 取消/断网不产生半成品且可重试幂等). Generic callback bounds keep the
/// returned future `Send` (spawned from the main loop).
pub async fn download_export_progress<F, C>(
    http: &reqwest::Client,
    base: &str,
    session_id: &str,
    path: &Path,
    on_progress: &mut F,
    is_cancelled: C,
) -> Result<ExportReceipt, ClientError>
where
    F: FnMut(u64) + Send,
    C: Fn() -> bool + Send,
{
    let url = format!(
        "{}/api/session.export?sessionId={}&includeDescendants=true",
        base.trim_end_matches('/'),
        urlencode(session_id)
    );
    let mut resp = http
        .get(&url)
        .send()
        .await
        .map_err(|e| ClientError::Transport(format!("导出请求失败（{url}）: {e}")))?;
    if !resp.status().is_success() {
        return Err(ClientError::Http(format!(
            "session.export HTTP {}（{url}）",
            resp.status()
        )));
    }
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let tmp_name = format!(
        "{}.tmp-{}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "export".into()),
        std::process::id()
    );
    let tmp_path = match parent {
        Some(p) => {
            if let Err(e) = tokio::fs::create_dir_all(p).await {
                return Err(ClientError::Transport(format!(
                    "创建导出目录失败（{}）: {e}",
                    p.display()
                )));
            }
            p.join(&tmp_name)
        }
        None => std::path::PathBuf::from(&tmp_name),
    };
    let mut out = tokio::fs::File::create(&tmp_path).await.map_err(|e| {
        ClientError::Transport(format!(
            "创建临时导出文件失败（{}）: {e}",
            tmp_path.display()
        ))
    })?;
    let mut bytes: u64 = 0;
    let mut cancelled = false;
    let stream_result: Result<(), ClientError> = async {
        loop {
            if is_cancelled() {
                cancelled = true;
                return Err(ClientError::Transport("导出已取消".into()));
            }
            let chunk = resp
                .chunk()
                .await
                .map_err(|e| ClientError::Transport(format!("导出流读取失败: {e}")))?;
            match chunk {
                Some(c) => {
                    out.write_all(&c)
                        .await
                        .map_err(|e| ClientError::Transport(format!("导出写入失败: {e}")))?;
                    bytes += c.len() as u64;
                    on_progress(bytes);
                }
                None => break,
            }
        }
        out.flush()
            .await
            .map_err(|e| ClientError::Transport(format!("导出 flush 失败: {e}")))?;
        Ok(())
    }
    .await;
    drop(out);
    let _ = cancelled;
    if let Err(e) = stream_result {
        // 中断/取消/断网：删除临时文件，目标路径不留半成品（AC-007-17）。
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(e);
    }
    tokio::fs::rename(&tmp_path, path)
        .await
        .map_err(|e| ClientError::Transport(format!("导出落盘 rename 失败: {e}")))?;
    Ok(ExportReceipt {
        bytes,
        final_path: path.to_path_buf(),
    })
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_escapes_reserved_chars_only() {
        assert_eq!(urlencode("sess-1_abc"), "sess-1_abc");
        assert_eq!(urlencode("a b/c"), "a%20b%2Fc");
        assert_eq!(urlencode("会话"), "%E4%BC%9A%E8%AF%9D");
    }
}
