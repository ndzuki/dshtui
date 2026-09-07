//! Settings panel state (REQ-007 FR-007-03 half; pure model).
//!
//! The describe tree is read-only; only a whitelisted set of scalar keys is
//! editable (mirroring the web UI key set). Secret values never leave the
//! `set/unset` presence state (安全边界).

/// One editable settings row (flattened from a namespace describe).
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsRow {
    /// Fully-qualified key e.g. `locale.preference`.
    pub key: String,
    pub namespace: String,
    pub value_display: String,
    /// Whether this row carries a user override (base vs user).
    pub user_set: bool,
    pub secret: bool,
    /// namespace revision (CAS token for the whole-namespace write).
    pub revision: u64,
}

/// Settings panel state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SettingsPanelState {
    pub visible: bool,
    /// Describe tree flatten (read-only view rows).
    pub rows: Vec<SettingsRow>,
    pub selected: usize,
    /// Writable namespaces list (for edit permission gating).
    pub writable: bool,
    /// Edit sub-stage: key being edited + input buffer.
    pub edit_key: Option<String>,
    pub edit_buffer: String,
    /// Risk confirm (whitelist-external key / destructive).
    pub risk_confirm: Option<String>,
    pub loading: bool,
    pub last_error_code: Option<String>,
}

impl SettingsPanelState {
    pub fn open(&mut self) {
        self.visible = true;
        self.loading = true;
        self.last_error_code = None;
    }

    pub fn close(&mut self) {
        *self = Self::default();
    }

    pub fn set_rows(&mut self, rows: Vec<SettingsRow>, writable: bool) {
        self.rows = rows;
        self.writable = writable;
        self.loading = false;
        if !self.rows.is_empty() {
            self.selected = self.selected.min(self.rows.len() - 1);
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        self.selected =
            (self.selected as isize + delta).clamp(0, self.rows.len() as isize - 1) as usize;
    }
}

/// Flatten a namespace `value` object into rows for the whitelisted keys.
/// `whitelist` maps `ns.key` → true. Non-whitelisted scalars are shown
/// read-only but never offered for editing.
pub fn flatten_namespace_rows(
    ns: &str,
    value: &serde_json::Value,
    user: Option<&serde_json::Value>,
    secrets: &[String],
    revision: u64,
    whitelist: &[&str],
) -> Vec<SettingsRow> {
    let mut out = Vec::new();
    let Some(obj) = value.as_object() else {
        return out;
    };
    let user_obj = user.and_then(|u| u.as_object());
    for (k, v) in obj {
        if v.is_object() || v.is_array() {
            continue; // 摘要层只展示标量；嵌套对象不展开
        }
        let key = format!("{ns}.{k}");
        let is_secret = secrets.iter().any(|s| s == k || s == &key);
        let display = if is_secret {
            "••• (set)".to_string()
        } else {
            match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Bool(b) => b.to_string(),
                serde_json::Value::Number(n) => n.to_string(),
                _ => String::new(),
            }
        };
        out.push(SettingsRow {
            key: key.clone(),
            namespace: ns.to_string(),
            value_display: display,
            user_set: user_obj.map(|u| u.contains_key(k)).unwrap_or(false),
            secret: is_secret,
            revision,
        });
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    out.retain(|r| whitelist.contains(&r.key.as_str()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flatten_selects_whitelisted_scalars_and_marks_secrets() {
        let value = json!({
            "preference": "dark",
            "fontSize": 14,
            "secretToken": "never",
            "nested": {"x": 1}
        });
        let rows = flatten_namespace_rows(
            "ui-theme",
            &value,
            Some(&json!({"preference": "dark"})),
            &["secretToken".into()],
            4,
            &["ui-theme.preference", "ui-theme.fontSize"],
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].key, "ui-theme.fontSize");
        assert_eq!(rows[0].value_display, "14");
        assert_eq!(rows[1].key, "ui-theme.preference");
        assert!(rows[1].user_set);
        assert!(!rows[1].secret);
        // secret key 不进白名单编辑、展示掩码（在此 rows 只含白名单，故需单独验证）
        let rows = flatten_namespace_rows(
            "credentials",
            &json!({"token": "abc"}),
            None,
            &["token".into()],
            1,
            &["credentials.token"],
        );
        assert_eq!(rows[0].value_display, "••• (set)");
        assert!(rows[0].secret);
    }

    #[test]
    fn empty_and_non_object_value_yield_no_rows() {
        assert!(flatten_namespace_rows("ui-theme", &json!({}), None, &[], 0, &[]).is_empty());
        assert!(flatten_namespace_rows("ui-theme", &json!("nope"), None, &[], 0, &[]).is_empty());
    }

    #[test]
    fn selection_clamped() {
        let mut s = SettingsPanelState::default();
        s.open();
        s.set_rows(
            flatten_namespace_rows("a", &json!({"x": 1}), None, &[], 1, &["a.x"]),
            true,
        );
        assert_eq!(s.rows.len(), 1);
        assert!(!s.loading);
        s.move_selection(9);
        assert_eq!(s.selected, 0, "clamp 到 len-1");
    }
}
