//! Model catalog (REQ-006 FR-006-01; D-032 official read 0.1.2-rc.1).
//!
//! The official `session/modelCatalog` response is provider-grouped and has no
//! query/prefix — searching/filtering is done LOCALLY (ADR-003 nucleo). This
//! module flattens the wire catalog into selectable rows and provides the
//! fuzzy query seam (pure, no IO), mirroring `WorkspaceStore::match_sessions`
//! / `TrajectorySearchIndex` patterns.

use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Matcher, Utf32String};

use crate::api::types::{ModelCatalog, ModelCatalogModel, ModelProviderGroup, WireModelSelection};

/// One flattened selectable model row (provider group + model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCatalogItem {
    pub provider_id: String,
    pub provider_name: String,
    pub model_id: String,
    pub model_name: String,
    pub description: Option<String>,
    /// Reasoning metadata for this exact route (id list).
    pub reasoning_efforts: Vec<String>,
    pub default_effort: Option<String>,
}

/// Local catalog view (filtered list for the overlay).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CatalogIndex {
    items: Vec<ModelCatalogItem>,
}

impl CatalogIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Flatten a wire catalog into selectable rows (provider order, model
    /// order). Failures/unknown shapes are tolerated at the api layer.
    pub fn rebuild(&mut self, catalog: &ModelCatalog) {
        self.items.clear();
        for group in &catalog.groups {
            push_group(&mut self.items, group);
        }
    }

    /// All items (unfiltered).
    pub fn items(&self) -> &[ModelCatalogItem] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Fuzzy query over flattened items (nucleo, ADR-003). Empty query returns
    /// the full list; empty result means "no match" (AC-006-07 empty state).
    pub fn query(&self, query: &str) -> Vec<&ModelCatalogItem> {
        let query = query.trim();
        if query.is_empty() {
            return self.items.iter().collect();
        }
        let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
        let mut matcher = Matcher::default();
        let mut scored: Vec<(u32, &ModelCatalogItem)> = self
            .items
            .iter()
            .filter_map(|item| {
                let fields = [
                    item.model_id.as_str(),
                    item.model_name.as_str(),
                    item.provider_id.as_str(),
                    item.provider_name.as_str(),
                ];
                let best = fields
                    .iter()
                    .filter_map(|field| {
                        if field.is_empty() {
                            return None;
                        }
                        let haystack = Utf32String::from(*field);
                        pattern.score(haystack.slice(..), &mut matcher)
                    })
                    .max()?;
                Some((best, item))
            })
            .collect();
        scored.sort_by_key(|s| std::cmp::Reverse(s.0));
        scored.into_iter().map(|(_, item)| item).collect()
    }
}

fn push_group(out: &mut Vec<ModelCatalogItem>, group: &ModelProviderGroup) {
    for model in &group.models {
        push_model(out, group, model);
    }
}

fn push_model(
    out: &mut Vec<ModelCatalogItem>,
    group: &ModelProviderGroup,
    model: &ModelCatalogModel,
) {
    let reasoning_efforts = model
        .reasoning
        .as_ref()
        .map(|r| r.efforts.iter().map(|e| e.id.clone()).collect())
        .unwrap_or_default();
    let default_effort = model
        .reasoning
        .as_ref()
        .and_then(|r| r.default_effort.clone());
    out.push(ModelCatalogItem {
        provider_id: group.id.clone(),
        provider_name: group.name.clone(),
        model_id: model.id.clone(),
        model_name: model.name.clone(),
        description: model.description.clone(),
        reasoning_efforts,
        default_effort,
    });
}

impl ModelCatalogItem {
    /// Unique route display: `provider/model` (official selectModel key).
    pub fn route(&self) -> String {
        format!("{}/{}", self.provider_id, self.model_id)
    }

    /// Whether `selection` matches this item's route (for current/next marks).
    pub fn matches(&self, sel: &WireModelSelection) -> bool {
        sel.provider == self.provider_id && sel.model == self.model_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::ModelReasoning;
    use serde_json::json;

    fn catalog() -> ModelCatalog {
        serde_json::from_value(json!({
            "default": {"provider": "deepseek_official", "model": "deepseek-chat"},
            "routableProviders": ["deepseek_official"],
            "groups": [{
                "id": "deepseek_official",
                "name": "DeepSeek 官方",
                "models": [
                    {"id": "deepseek-chat", "name": "DeepSeek Chat",
                     "reasoning": {"efforts": [{"id": "low", "name": "Low"}, {"id": "high", "name": "High"}],
                                   "defaultEffort": "low"}},
                    {"id": "deepseek-reasoner", "name": "DeepSeek Reasoner",
                     "reasoning": {"efforts": [{"id": "max", "name": "Max"}]}},
                    {"id": "deepseek-v4-pro", "name": "V4 Pro"}
                ]
            }],
            "failures": []
        }))
        .unwrap()
    }

    #[test]
    fn flatten_and_query_local_nucleo_ac006_01() {
        let mut idx = CatalogIndex::new();
        idx.rebuild(&catalog());
        assert_eq!(idx.len(), 3);
        // 空查询 = 全量。
        let all = idx.query("");
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].route(), "deepseek_official/deepseek-chat");
        assert_eq!(all[0].reasoning_efforts, vec!["low", "high"]);
        assert_eq!(all[0].default_effort.as_deref(), Some("low"));
        // 搜索/筛选即时（本地 nucleo）。
        let hits = idx.query("reasoner");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].model_id, "deepseek-reasoner");
        let hits = idx.query("v4");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].model_id, "deepseek-v4-pro");
        // 无匹配 → 空（AC-006-07 空态判定依据）。
        assert!(idx.query("nope").is_empty());
    }

    #[test]
    fn matches_route_against_wire_selection() {
        let mut idx = CatalogIndex::new();
        idx.rebuild(&catalog());
        let sel = WireModelSelection {
            provider: "deepseek_official".into(),
            model: "deepseek-chat".into(),
            reasoning_effort: Some("low".into()),
        };
        assert!(idx.items()[0].matches(&sel));
        assert!(!idx.items()[1].matches(&sel));
    }

    #[test]
    fn model_reasoning_parses_from_wire() {
        let c = catalog();
        let g = &c.groups[0];
        assert_eq!(g.models[0].reasoning.as_ref().unwrap().efforts.len(), 2);
        let _ = ModelReasoning {
            efforts: vec![],
            default_effort: None,
        };
    }
}
