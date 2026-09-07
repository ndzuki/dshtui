//! settings/* endpoint wrappers (REQ-007 V0.4; official read 0.1.2-rc.1 from
//! `@deepseek-ai/dsh-api-settings-controller/typert.remote-client`).
//!
//! Wire knowledge (centralized here per Notes/03):
//! - `settings/describe()` is zero-arg;
//! - `settings/mutate(ns, ops, expectedRevision?)` — namespace-scoped path ops;
//! - `settings/update(ns, patch, expectedRevision?)` / `replace(ns, section,
//!   expectedRevision?)` — whole-namespace patch/replace with CAS;
//! - `expectedRevision` is a per-namespace monotonic CAS; omit = last-write
//!   wins (used by read-only describe flows). Mutations return the updated
//!   `SettingsNamespaceView`.
//!
//! The dshtui settings panel only writes a whitelisted set of scalar keys
//! (mirroring the web UI); secret values are never displayed.

use serde_json::Value;

use super::envelope::ClientError;
use super::types::{
    SettingsDescribeValue, SettingsNamespaceView, SettingsPathOpView,
};
use super::unary;

/// `settings/describe` — namespaces, schemas, current values and revisions.
pub async fn describe(
    http: &reqwest::Client,
    base: &str,
) -> Result<SettingsDescribeValue, ClientError> {
    let value = unary(http, base, "settings/describe", serde_json::json!({})).await?;
    serde_json::from_value(value)
        .map_err(|e| ClientError::Protocol(format!("settings/describe 响应形状异常: {e}")))
}

/// `settings/mutate` — apply scoped path ops with an optional expectedRevision
/// CAS. Omitted expected_revision = last-write-wins.
pub async fn mutate(
    http: &reqwest::Client,
    base: &str,
    ns: &str,
    ops: &[SettingsPathOpView],
    expected_revision: Option<u64>,
) -> Result<SettingsNamespaceView, ClientError> {
    let args = json_with_expected(
        "settings/mutate",
        ns,
        expected_revision,
        serde_json::json!({ "ops": ops }),
    );
    let value = unary(http, base, "settings/mutate", args).await?;
    parse_namespace("settings/mutate", value)
}

/// `settings/update` — patch one namespace (partial update of scalar keys).
pub async fn update(
    http: &reqwest::Client,
    base: &str,
    ns: &str,
    patch: Value,
    expected_revision: Option<u64>,
) -> Result<SettingsNamespaceView, ClientError> {
    let args = json_with_expected(
        "settings/update",
        ns,
        expected_revision,
        serde_json::json!({ "patch": patch }),
    );
    let value = unary(http, base, "settings/update", args).await?;
    parse_namespace("settings/update", value)
}

/// `settings/replace` — replace one namespace section wholesale.
#[allow(dead_code)] // kept symmetric with update; UI edits go through update.
pub async fn replace(
    http: &reqwest::Client,
    base: &str,
    ns: &str,
    section: Value,
    expected_revision: Option<u64>,
) -> Result<SettingsNamespaceView, ClientError> {
    let args = json_with_expected(
        "settings/replace",
        ns,
        expected_revision,
        serde_json::json!({ "section": section }),
    );
    let value = unary(http, base, "settings/replace", args).await?;
    parse_namespace("settings/replace", value)
}

/// Shared args builder: `{ns, ...extra}` plus optional `expectedRevision`.
fn json_with_expected(
    _method: &str,
    ns: &str,
    expected_revision: Option<u64>,
    mut extra: Value,
) -> Value {
    extra["ns"] = serde_json::json!(ns);
    if let Some(rev) = expected_revision {
        extra["expectedRevision"] = serde_json::json!(rev);
    }
    extra
}

/// Tolerant namespace-view parser: mutations may return the view directly or
/// nested under `{namespace: {...}}`.
fn parse_namespace(method: &str, value: Value) -> Result<SettingsNamespaceView, ClientError> {
    let inner = value
        .get("namespace")
        .or_else(|| value.get("view"))
        .cloned()
        .unwrap_or(value);
    serde_json::from_value(inner)
        .map_err(|e| ClientError::Protocol(format!("{method} 响应形状异常: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_view_parses_revision_and_applies() {
        let v: SettingsNamespaceView = serde_json::from_value(serde_json::json!({
            "ns": "ui-theme",
            "schema": {"type": "object"},
            "value": {"preference": "dark"},
            "applies": "restart",
            "secrets": [],
            "revision": 7
        }))
        .unwrap();
        assert_eq!(v.ns, "ui-theme");
        assert_eq!(v.value["preference"], "dark");
        assert_eq!(v.applies, "restart");
        assert_eq!(v.revision, 7);
    }

    #[test]
    fn path_op_serializes_set_unset() {
        let op = SettingsPathOpView {
            path: "preference".into(),
            op_kind: "set".into(),
            value: Some(serde_json::json!("dark")),
        };
        let v = serde_json::to_value(&op).unwrap();
        assert_eq!(v["path"], "preference");
        assert_eq!(v["op"], "set");
        assert_eq!(v["value"], "dark");

        let un = SettingsPathOpView {
            path: "fontSize".into(),
            op_kind: "unset".into(),
            value: None,
        };
        let v = serde_json::to_value(&un).unwrap();
        assert_eq!(v["op"], "unset");
        assert!(v.get("value").is_none(), "unset 不带上送值");
    }

    #[test]
    fn args_builder_includes_ns_and_optional_revision() {
        let args = json_with_expected(
            "settings/mutate",
            "locale",
            Some(3),
            serde_json::json!({"ops": []}),
        );
        assert_eq!(args["ns"], "locale");
        assert_eq!(args["expectedRevision"], 3);
        let args = json_with_expected("settings/mutate", "locale", None, serde_json::json!({"ops": []}));
        assert!(args.get("expectedRevision").is_none(), "None 不上送");
    }
}
