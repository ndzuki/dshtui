//! Theme/keymap config mirror (REQ-007 D-42; pure model).
//!
//! Palette semantic roles + keymap overrides are local TUI config (NOT read
//! from the remote ui-theme settings namespace — ADR-010 / D-42). The config
//! file remains the source of truth; these types model the parsed result.

use std::collections::BTreeMap;

/// Semantic palette roles used by the UI (role → color string or name).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThemeKeymapConfig {
    /// Selected built-in theme ("dark" | "light").
    pub theme: String,
    /// Role-name → color override (parsed tolerantly; invalid → role default
    /// + warning, AC-007-21 semantics).
    pub palette: BTreeMap<String, String>,
    /// Per-mode command → key-sequence overrides (Step 5).
    pub keymap: BTreeMap<String, BTreeMap<String, String>>,
    /// Draft persistence switches (ADR-010).
    pub drafts_enabled: bool,
    pub drafts_clear: bool,
}

impl ThemeKeymapConfig {
    /// Effective palette for a role: explicit override first, else built-in
    /// default table, else None (the renderer keeps its hardcoded default).
    pub fn color_for<'a>(
        &'a self,
        role: &str,
        builtin: &'a BTreeMap<String, String>,
    ) -> Option<&'a String> {
        self.palette.get(role).or_else(|| builtin.get(role))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin() -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("normal".into(), "#888888".into());
        m
    }

    #[test]
    fn override_wins_over_builtin() {
        let mut cfg = ThemeKeymapConfig {
            theme: "dark".into(),
            ..Default::default()
        };
        cfg.palette.insert("normal".into(), "#ffffff".into());
        assert_eq!(
            cfg.color_for("normal", &builtin()).map(String::as_str),
            Some("#ffffff")
        );
    }

    #[test]
    fn missing_role_falls_back_to_builtin_or_none() {
        let cfg = ThemeKeymapConfig::default();
        assert_eq!(
            cfg.color_for("normal", &builtin()).map(String::as_str),
            Some("#888888")
        );
        assert_eq!(cfg.color_for("unknown", &builtin()), None);
    }

    #[test]
    fn defaults_no_clear_empty_maps() {
        let cfg = ThemeKeymapConfig::default();
        assert!(!cfg.drafts_clear);
        assert!(cfg.palette.is_empty());
        assert!(cfg.keymap.is_empty());
    }
}
