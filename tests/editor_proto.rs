//! PROTOTYPE (throwaway) — Step 7 `:edit` raw-mode suspend gate (REQ-007
//! AC-007-25).
//!
//! Question: can `:edit` suspend the TUI, run a foreground `$EDITOR`, and
//! refill the composer WITHOUT a new terminal-state-management framework?
//!
//! This environment has no real TTY, so raw-mode leave/reenter cannot be
//! driven directly; the prototype validates the deterministic core that the
//! real implementation will ship with:
//!   1. `run_editor_blocking(tmp, editor)` — spawn a foreground editor on a
//!      temp file (same-filesystem, in the target dir per
//!      `uncategorized/TASK-002-pitfall`), map exit status;
//!   2. fake `$EDITOR` (bash: writes the file, exits 0) → file content
//!      replaced, exit 0;
//!   3. missing editor / non-zero exit → readable typed error AND the
//!      ExternalEditState machine lands in a safe recoverable state
//!      (recovery path: a good editor run after a failure succeeds);
//!   4. ExternalEditState transitions already unit-tested in Step 2 — this
//!      proves the suspend→Editing→exited_ok→RefillPending integration with
//!      the real process helper.
//!
//! Raw-mode leave/reenter itself is a thin pair of crossterm calls that is
//! the exact inverse of `TerminalSession::enter()` (which `Drop` already
//! performs) — no new framework required → PASS if 1–4 hold.

use std::io::Write;
use std::process::{Command, Stdio};

/// Prototype of the production editor-run helper.
fn run_editor_blocking(tmp: &std::path::Path, editor: &str) -> Result<(), String> {
    // 前台子进程继承 stdio：编辑器可交互（raw mode 已由调用方释放）。
    let status = Command::new(editor)
        .arg(tmp)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| format!("无法启动编辑器 {editor}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "编辑器 {editor} 异常退出（{status}），已安全返回 composer"
        ))
    }
}

/// 写一个假 editor 脚本：把 $1 文件替换为给定内容，退出码由参数控制。
fn write_fake_editor(dir: &std::path::Path, name: &str, exit_code: i32) -> std::path::PathBuf {
    let script = dir.join(name);
    let mut f = std::fs::File::create(&script).unwrap();
    writeln!(
        f,
        "#!/bin/bash\nprintf 'EDITED-BODY' > \"$1\"\nexit {exit_code}\n"
    )
    .unwrap();
    drop(f);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&script).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&script, perm).unwrap();
    }
    script
}

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "dshtui-edit-proto-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn proto_fake_editor_writes_file_and_exits_ok() {
    let dir = tmp_dir("ok");
    let editor = write_fake_editor(&dir, "fake-editor.sh", 0);
    let tmp = dir.join("draft.md");
    std::fs::write(&tmp, "ORIGINAL").unwrap();
    run_editor_blocking(&tmp, editor.to_str().unwrap()).unwrap();
    assert_eq!(
        std::fs::read_to_string(&tmp).unwrap(),
        "EDITED-BODY",
        "假编辑器替换文件内容"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn proto_missing_editor_is_readable_error() {
    let dir = tmp_dir("missing");
    let tmp = dir.join("draft.md");
    std::fs::write(&tmp, "ORIGINAL").unwrap();
    let err = run_editor_blocking(&tmp, "/nonexistent/editor-xyz").unwrap_err();
    assert!(err.contains("无法启动编辑器"), "可读错误：err={err}");
    // 恢复路径：修正 editor 后必须能成功（不被旧失败污染）。
    let editor = write_fake_editor(&dir, "good.sh", 0);
    run_editor_blocking(&tmp, editor.to_str().unwrap()).unwrap();
    assert_eq!(std::fs::read_to_string(&tmp).unwrap(), "EDITED-BODY");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn proto_nonzero_exit_is_error_and_file_untouched_is_safe() {
    let dir = tmp_dir("fail");
    let editor = write_fake_editor(&dir, "bad.sh", 3);
    let tmp = dir.join("draft.md");
    std::fs::write(&tmp, "ORIGINAL").unwrap();
    let err = run_editor_blocking(&tmp, editor.to_str().unwrap()).unwrap_err();
    assert!(
        err.contains("异常退出") && err.contains("安全返回 composer"),
        "可读错误：err={err}"
    );
    // 失败后文件可能被部分写（编辑器行为），composer 保留原 draft 是上层
    // ExternalEditState.cancel/fail 语义；这里断言模型安全回 Inactive。
    let mut state = dshtui::model::ExternalEditState::default();
    assert!(state.suspend("原草稿", tmp.to_string_lossy().into_owned(), None));
    state.fail(err.clone());
    assert_eq!(
        state.phase,
        dshtui::model::external_edit::ExternalEditPhase::Inactive
    );
    assert!(state.last_error.as_deref().unwrap().contains("异常退出"));
    assert!(!state.is_active(), "失败后安全回 composer");
    // 恢复路径：失败后重新 suspend 成功。
    assert!(state.suspend("原草稿", tmp.to_string_lossy().into_owned(), None));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn proto_suspend_exit_ok_refill_integration() {
    let dir = tmp_dir("refill");
    let editor = write_fake_editor(&dir, "ok.sh", 0);
    let tmp = dir.join("draft.md");
    std::fs::write(&tmp, "ORIGINAL").unwrap();

    let mut state = dshtui::model::ExternalEditState::default();
    // 1) 挂起：raw 释放（调用方）+ 状态置 Editing。
    assert!(state.suspend(
        "正在编辑的草稿",
        tmp.to_string_lossy().into_owned(),
        Some(editor.to_string_lossy().into_owned())
    ));
    assert_eq!(
        state.phase,
        dshtui::model::external_edit::ExternalEditPhase::Editing
    );
    // 2) 子进程运行（可交互写）。
    run_editor_blocking(&tmp, state.editor.as_deref().unwrap()).unwrap();
    // 3) 退出后回填 composer。
    state.mark_exited_ok();
    assert_eq!(
        state.phase,
        dshtui::model::external_edit::ExternalEditPhase::RefillPending
    );
    let edited = std::fs::read_to_string(&tmp).unwrap();
    assert_eq!(edited, "EDITED-BODY");
    state.settle();
    assert_eq!(
        state.phase,
        dshtui::model::external_edit::ExternalEditPhase::Inactive
    );
    let _ = std::fs::remove_dir_all(&dir);
}
