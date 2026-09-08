//! PROTOTYPE (throwaway) — Step 5 keymap-override gate (REQ-007 AC-007-21).
//!
//! Question: can per-mode `[keymap]` overrides be layered over the existing
//! hardcoded KeyDecoder WITHOUT rewriting every decode branch, so that:
//!   - no override present  → decode is byte-identical to today (existing ~54
//!     keymap tests stay green unchanged);
//!   - `move_down="x"`      → x now MoveDown AND j (the old default) no longer
//!     MoveDown ("原键失绑" — no post-decode remap trickery needed);
//!   - `quit="none"`        → q unbound;
//!   - illegal keyspec/unknown command/mode → warning + fall back to default
//!     (never crash).
//!
//! This file is a SELF-CONTAINED mirror of the merge/shadow algorithm only —
//! it does not touch the production `KeyDecoder`. PASS here + existing suite
//! green = the real Step 5 can wire the same algorithm behind `with_keymap`.

use std::collections::BTreeMap;

// ---- minimal mirror of the production command set (normal mode subset) ----
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmd {
    MoveDown,
    MoveUp,
    GotoBottom,
    OpenPicker,
    InsertMode,
    OpenHelp,
    Quit,
    StopRunning,
    CollapseProject,
    ExpandProject,
    OpenSelected,
    OpenModelCatalog,
    OpenCommandPalette,
}

/// Canonical name used by `[keymap]` config.
fn cmd_name(c: Cmd) -> &'static str {
    match c {
        Cmd::MoveDown => "move_down",
        Cmd::MoveUp => "move_up",
        Cmd::GotoBottom => "goto_bottom",
        Cmd::OpenPicker => "open_picker",
        Cmd::InsertMode => "insert_mode",
        Cmd::OpenHelp => "open_help",
        Cmd::Quit => "quit",
        Cmd::StopRunning => "stop_running",
        Cmd::CollapseProject => "collapse_project",
        Cmd::ExpandProject => "expand_project",
        Cmd::OpenSelected => "open_selected",
        Cmd::OpenModelCatalog => "open_model_catalog",
        Cmd::OpenCommandPalette => "open_command_palette",
    }
}

fn cmd_from_name(name: &str) -> Option<Cmd> {
    [
        Cmd::MoveDown,
        Cmd::MoveUp,
        Cmd::GotoBottom,
        Cmd::OpenPicker,
        Cmd::InsertMode,
        Cmd::OpenHelp,
        Cmd::Quit,
        Cmd::StopRunning,
        Cmd::CollapseProject,
        Cmd::ExpandProject,
        Cmd::OpenSelected,
        Cmd::OpenModelCatalog,
        Cmd::OpenCommandPalette,
    ]
    .into_iter()
    .find(|c| cmd_name(*c) == name)
}

/// Normal-mode DEFAULT single-key table (transcribed from `KeyDecoder::normal`).
fn default_normal() -> Vec<(char, Cmd)> {
    vec![
        ('j', Cmd::MoveDown),
        ('k', Cmd::MoveUp),
        ('G', Cmd::GotoBottom),
        ('f', Cmd::OpenPicker),
        ('i', Cmd::InsertMode),
        ('?', Cmd::OpenHelp),
        ('q', Cmd::Quit),
        ('s', Cmd::StopRunning),
        ('h', Cmd::CollapseProject),
        ('l', Cmd::ExpandProject),
        ('o', Cmd::OpenSelected),
        ('M', Cmd::OpenModelCatalog),
        (':', Cmd::OpenCommandPalette),
    ]
}

// ---- prototype merge: effective table + shadow set + warnings ----
struct ProtoKeymap {
    /// effective: char → command (defaults + overrides merged).
    effective: BTreeMap<char, Cmd>,
    /// pristine defaults: char → command (to detect "this char's command was
    /// moved/unbound" → shadow the old default key).
    defaults: BTreeMap<char, Cmd>,
    warnings: Vec<String>,
}

impl ProtoKeymap {
    /// Build from default table + config `{command: keyspec}` for normal mode.
    fn build(cfg: &BTreeMap<String, String>) -> Self {
        let defaults: BTreeMap<char, Cmd> = default_normal().into_iter().collect();
        let mut effective = defaults.clone();
        let mut warnings = Vec::new();
        for (name, keyspec) in cfg {
            let Some(cmd) = cmd_from_name(name) else {
                warnings.push(format!("未知 command: {name}（保留默认）"));
                continue;
            };
            // Find this command's default key.
            let default_key = defaults.iter().find_map(|(k, c)| (*c == cmd).then_some(*k));
            // `none` = unbind.
            if keyspec == "none" {
                if let Some(k) = default_key {
                    effective.remove(&k);
                }
                continue;
            }
            // Single plain-char keyspec only (prototype scope).
            let mut cs = keyspec.chars();
            let (Some(c), None) = (cs.next(), cs.next()) else {
                warnings.push(format!(
                    "{name}={keyspec:?} 非法键序列（原型仅支持单字符或 none，保留默认）"
                ));
                continue;
            };
            if !c.is_ascii_alphanumeric() && !":/?[]GHJKL".contains(c) {
                warnings.push(format!("{name}={keyspec:?} 非法键序列（保留默认）"));
                continue;
            }
            if c == 'g' {
                warnings.push(format!(
                    "{name}={keyspec:?}: g 为前缀键不可重绑（保留默认）"
                ));
                continue;
            }
            // Remove old default key of the command (原键失绑).
            if let Some(k) = default_key {
                effective.remove(&k);
            }
            // Later write wins on conflict; warn if a different default is displaced.
            if let Some(displaced) = defaults.get(&c) {
                if *displaced != cmd {
                    warnings.push(format!(
                        "{}={} 位移默认绑定 {}（{}）",
                        name,
                        c,
                        cmd_name(*displaced),
                        c
                    ));
                }
            }
            effective.insert(c, cmd);
        }
        ProtoKeymap {
            effective,
            defaults,
            warnings,
        }
    }

    /// Decode a single plain char against the override layer:
    /// - remap hit (char maps to a command DIFFERENT from its pristine default,
    ///   or char had no default) → Some(command);
    /// - shadow (char was a default key whose command was moved/unbound) → None;
    /// - otherwise → fall through to the production branch (None here).
    fn override_decode(&self, c: char) -> Option<Option<Cmd>> {
        let eff = self.effective.get(&c).copied();
        let def = self.defaults.get(&c).copied();
        match (eff, def) {
            // Char newly/remap-bound → the override layer owns it.
            (Some(cmd), Some(old)) if cmd != old => Some(Some(cmd)),
            (Some(cmd), None) => Some(Some(cmd)),
            // Char was a default key but the command moved/unbound → shadow.
            (None, Some(_)) => Some(None),
            // Unchanged default → production branch owns it (fall through).
            _ => None,
        }
    }
}

// ---- prototype assertions (the gate's PASS criteria) ----

#[test]
fn proto_no_override_is_complete_noop_fallthrough() {
    let km = ProtoKeymap::build(&BTreeMap::new());
    // All unchanged defaults must fall through (None) so the production
    // hardcoded branch keeps its byte-identical behavior.
    for (c, _cmd) in default_normal() {
        assert_eq!(km.override_decode(c), None, "默认键 {c} 必须回落生产分支");
    }
    assert!(km.warnings.is_empty());
}

#[test]
fn proto_move_down_rebind_new_key_and_old_key_unbound() {
    let mut cfg = BTreeMap::new();
    cfg.insert("move_down".to_string(), "x".to_string());
    let km = ProtoKeymap::build(&cfg);
    assert_eq!(
        km.override_decode('x'),
        Some(Some(Cmd::MoveDown)),
        "新键生效"
    );
    assert_eq!(km.override_decode('j'), Some(None), "原键 j 失绑（shadow）");
    assert!(km.warnings.is_empty());
}

#[test]
fn proto_quit_none_unbinds_old_key() {
    let mut cfg = BTreeMap::new();
    cfg.insert("quit".to_string(), "none".to_string());
    let km = ProtoKeymap::build(&cfg);
    assert_eq!(km.override_decode('q'), Some(None), "q 解绑");
}

#[test]
fn proto_jk_swap_later_wins() {
    let mut cfg = BTreeMap::new();
    cfg.insert("move_down".to_string(), "k".to_string());
    let km = ProtoKeymap::build(&cfg);
    // k now MoveDown (displaces default MoveUp), j shadowed.
    assert_eq!(km.override_decode('k'), Some(Some(Cmd::MoveDown)));
    assert_eq!(km.override_decode('j'), Some(None));
    assert!(
        km.warnings
            .iter()
            .any(|w| w.contains("位移默认绑定 move_up")),
        "位移冲突需 warn：warnings={:?}",
        km.warnings
    );
}

#[test]
fn proto_unknown_command_and_illegal_keyspec_warn_keep_default() {
    let mut cfg = BTreeMap::new();
    cfg.insert("no_such_command".to_string(), "x".to_string());
    cfg.insert("move_down".to_string(), "j k".to_string()); // 双键（原型不支持）
    cfg.insert("open_help".to_string(), "Ctrl+x".to_string());
    let km = ProtoKeymap::build(&cfg);
    assert!(
        km.warnings.iter().any(|w| w.contains("未知 command")),
        "未知 command warn: {:?}",
        km.warnings
    );
    assert!(
        km.warnings.iter().any(|w| w.contains("非法键序列")),
        "非法 keyspec warn: {:?}",
        km.warnings
    );
    // 默认保留：j still MoveDown falls through (no override took effect),
    // ? still falls through to production OpenHelp.
    assert_eq!(km.override_decode('j'), None);
    assert_eq!(km.override_decode('?'), None);
}

#[test]
fn proto_g_prefix_is_not_rebindable() {
    let mut cfg = BTreeMap::new();
    cfg.insert("move_down".to_string(), "g".to_string());
    let km = ProtoKeymap::build(&cfg);
    assert!(
        km.warnings.iter().any(|w| w.contains("g 为前缀键不可重绑")),
        "g 保留给前缀：warnings={:?}",
        km.warnings
    );
    assert_eq!(km.override_decode('g'), None, "g 仍回落前缀分支");
}
