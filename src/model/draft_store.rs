//! DraftStore — the PERSISTENT mirror of DraftRegistry (REQ-007 AC-007-22,
//! ADR-010).
//!
//! Pure data (to/from TOML); the atomic 0600 write lives in config_store
//! (Step 3). Session-keyed text table; the in-memory DraftRegistry remains
//! the reducer-facing owner and this store is its durability image.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// On-disk drafts table (`drafts.toml`), session id → text.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DraftStore {
    #[serde(default)]
    pub drafts: BTreeMap<String, String>,
}

impl DraftStore {
    pub fn get(&self, session_id: &str) -> Option<&str> {
        self.drafts.get(session_id).map(String::as_str)
    }

    /// Set (or remove when empty) one session's draft.
    pub fn set(&mut self, session_id: &str, text: &str) {
        if text.is_empty() {
            self.drafts.remove(session_id);
        } else {
            self.drafts.insert(session_id.to_string(), text.to_string());
        }
    }

    pub fn clear_all(&mut self) {
        self.drafts.clear();
    }

    pub fn len(&self) -> usize {
        self.drafts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.drafts.is_empty()
    }

    /// Serialize to a TOML document string.
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string(self)
    }

    /// Parse from a TOML document string (corrupt input → readable error so
    /// the caller can warn-and-fallback per AC-007-22).
    pub fn from_toml(input: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_toml_preserves_sessions_and_clears_empty() {
        let mut store = DraftStore::default();
        store.set("s1", "草稿一");
        store.set("s2", "");
        assert_eq!(store.len(), 1, "空文本不落盘");
        store.set("s2", "草稿二");
        let toml = store.to_toml().unwrap();
        let back = DraftStore::from_toml(&toml).unwrap();
        assert_eq!(back.get("s1"), Some("草稿一"));
        assert_eq!(back.get("s2"), Some("草稿二"));
        assert_eq!(back, store);
    }

    #[test]
    fn from_toml_corrupt_is_typed_error_not_panic() {
        assert!(DraftStore::from_toml("not [[ valid toml").is_err());
        assert!(DraftStore::from_toml("drafts = 42").is_err());
    }

    #[test]
    fn clear_all_resets_table() {
        let mut store = DraftStore::default();
        store.set("s1", "a");
        store.clear_all();
        assert!(store.is_empty());
    }
}
