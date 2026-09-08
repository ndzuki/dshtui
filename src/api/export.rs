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
//!   fidelity); the page-rebuild JSONL fallback (D-46 hard contract) applies
//!   only when the official route is unavailable/fails: 404/5xx/transport →
//!   `session/page` 全量 records 重建（`[验证]` 行格式待 REQ-008 live 冒烟，
//!   records/关键事件数口径为可测验收）；
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

/// D-46 page 重建兜底：`session/page` 单页预算（records 数）。向后翻页直至
/// `hasMore=false`。
const REBUILD_PAGE_SIZE: usize = 500;
/// 防死循环硬上界（极端会话分页数；超过即中止，不留半成品）。
const REBUILD_MAX_PAGES: u64 = 1_000_000;

/// D-46: 官方导出路由不可用时（404/5xx/transport）的 **page 全量重建**——
/// 循环 `session/page` 收集该会话全量 raw records（from newest 向后翻页，
/// beforeSeq 独占上界推进，hasMore=false 停止），逐行写 header + record JSON
/// 到目标目录 tmp 文件，完成后原子 rename。
///
/// 返回 `(records 数, final_path)`。任何页错误/取消 → 删除 tmp、不留半成品。
/// 行格式 `[验证]`（REQ-008 live 冒烟锁定字节级契约）；本函数可测口径 =
/// records 行数对账 + tmp+rename 落盘 + 取消清理。
#[allow(clippy::too_many_arguments)]
pub async fn rebuild_export_jsonl<F, C>(
    http: &reqwest::Client,
    base: &str,
    session_id: &str,
    address: &super::types::SessionAddress,
    through_seq: super::types::SessionSeq,
    path: &Path,
    on_progress: &mut F,
    is_cancelled: C,
) -> Result<ExportReceipt, ClientError>
where
    F: FnMut(u64) + Send,
    C: Fn() -> bool + Send,
{
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let tmp_name = format!(
        "{}.rebuild-{}",
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
    // 独立的 async 行写入（复用同一文件句柄）。
    async fn write_line(out: &mut tokio::fs::File, line: &str) -> Result<u64, ClientError> {
        let b = line.as_bytes();
        out.write_all(b)
            .await
            .map_err(|e| ClientError::Transport(format!("导出重建写入失败: {e}")))?;
        Ok(b.len() as u64)
    }
    let run: Result<u64, ClientError> = async {
        let mut bytes: u64 = 0;
        // header 行（会话 meta）。
        let header = crate::model::export::rebuild_header_line(session_id);
        bytes += write_line(&mut out, &header).await?;
        let mut collected: u64 = 0;
        let mut before_seq: Option<super::types::SessionSeq> = None;
        let mut pages: u64 = 0;
        loop {
            if is_cancelled() {
                return Err(ClientError::Transport("导出已取消".into()));
            }
            if pages >= REBUILD_MAX_PAGES {
                return Err(ClientError::Transport(
                    "导出重建页数超上限，中止（防死循环）".into(),
                ));
            }
            let page = super::session::page_raw(
                http,
                base,
                address,
                through_seq,
                before_seq,
                REBUILD_PAGE_SIZE,
            )
            .await?;
            pages += 1;
            for rec in &page.records {
                let line = crate::model::export::rebuild_record_line(rec);
                bytes += write_line(&mut out, &line).await?;
            }
            collected += page.records.len() as u64;
            on_progress(collected);
            // 游标推进（防死循环：无 seq 页/无推进/空页 → 停）。
            let (next_before, stop) = crate::model::export::rebuild_next_cursor(
                before_seq.map(|s| s.0),
                &page.records,
                page.has_more,
                pages,
                REBUILD_MAX_PAGES,
            );
            before_seq = next_before.map(super::types::SessionSeq);
            if stop {
                break;
            }
        }
        out.flush()
            .await
            .map_err(|e| ClientError::Transport(format!("导出重建 flush 失败: {e}")))?;
        Ok(bytes)
    }
    .await;
    drop(out);
    let bytes = match run {
        Ok(bytes) => bytes,
        Err(e) => {
            // 取消/页错误/断流：删除临时文件，目标路径不留半成品（D-46）。
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(e);
        }
    };
    tokio::fs::rename(&tmp_path, path)
        .await
        .map_err(|e| ClientError::Transport(format!("导出落盘 rename 失败: {e}")))?;
    Ok(ExportReceipt {
        bytes,
        final_path: path.to_path_buf(),
    })
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
        // 结构化状态（D-46 兜底分类）：401/403 权限 → 不降级；404/5xx
        // （官方路由不可用）→ 上层可转 page 重建兜底。
        return Err(ClientError::HttpStatus {
            status: resp.status().as_u16(),
            url,
        });
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
