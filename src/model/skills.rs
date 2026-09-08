//! Skills catalog state (REQ-007 FR-007-03 half; pure model, read-only).
//!
//! Skills and slash commands are two independent registries (wire fact). The
//! catalog panel shows entries + copy-reference; invocation goes through the
//! existing `/` command entry (commands/execute), never a fake execute.

use crate::api::types::SkillEntry;

/// Skills catalog panel state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SkillsCatalogState {
    pub visible: bool,
    pub items: Vec<SkillEntry>,
    pub selected: usize,
    pub query: String,
    pub loading: bool,
    pub last_error_code: Option<String>,
}

impl SkillsCatalogState {
    pub fn open(&mut self) {
        self.visible = true;
        self.loading = true;
        self.last_error_code = None;
    }

    pub fn close(&mut self) {
        *self = Self::default();
    }

    pub fn set_items(&mut self, items: Vec<SkillEntry>) {
        self.items = items;
        self.loading = false;
        if !self.items.is_empty() {
            self.selected = self.selected.min(self.items.len() - 1);
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        self.selected =
            (self.selected as isize + delta).clamp(0, self.items.len() as isize - 1) as usize;
    }

    /// Local prefix filter over the fetched catalog (no server round trip).
    pub fn filtered(&self) -> Vec<&SkillEntry> {
        let q = self.query.trim().to_lowercase();
        self.items
            .iter()
            .filter(|e| {
                q.is_empty()
                    || e.name.to_lowercase().contains(&q)
                    || e.description.to_lowercase().contains(&q)
            })
            .collect()
    }

    /// Reference text copied for an entry (slash name).
    pub fn copy_ref(&self) -> Option<String> {
        self.filtered()
            .get(self.selected)
            .map(|e| format!("/{}", e.name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, desc: &str) -> SkillEntry {
        SkillEntry {
            name: name.into(),
            description: desc.into(),
            when_to_use: None,
            model_invocable: true,
        }
    }

    #[test]
    fn filter_matches_name_and_description() {
        let mut s = SkillsCatalogState::default();
        s.set_items(vec![
            entry("bash", "执行 shell 命令"),
            entry("research", "调研查证"),
        ]);
        assert_eq!(s.filtered().len(), 2);
        s.query = "research".into();
        assert_eq!(s.filtered().len(), 1);
        assert_eq!(s.filtered()[0].name, "research");
        s.query = "查证".into();
        assert_eq!(s.filtered()[0].name, "research");
        s.query = "zzz".into();
        assert!(s.filtered().is_empty());
    }

    #[test]
    fn copy_ref_produces_slash_name() {
        let mut s = SkillsCatalogState::default();
        s.set_items(vec![entry("bash", "x")]);
        assert_eq!(s.copy_ref().as_deref(), Some("/bash"));
    }
}
