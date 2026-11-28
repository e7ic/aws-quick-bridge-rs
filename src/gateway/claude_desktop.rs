//! Claude Desktop 本地接入配置和模型发现。

use crate::config::{CLAUDE_DESKTOP_AUTO_ID, CLAUDE_ROLE_MODES, model_display_name};
use chrono::Utc;
use serde_json::{Value, json};
use std::collections::HashSet;

pub(super) const BASE_PATH: &str = "/claude-desktop";

pub(crate) fn endpoint(base: &str) -> String {
    format!("{}{BASE_PATH}", base.trim_end_matches('/'))
}

fn model_id(tier: Option<&str>) -> String {
    match tier {
        Some(tier) => format!("claude-{tier}-bridge"),
        None => CLAUDE_DESKTOP_AUTO_ID.to_string(),
    }
}

fn model_routes(default_model: &str) -> impl Iterator<Item = (Option<&'static str>, &'static str)> {
    let preferred = if default_model == "auto" {
        (None, "auto")
    } else {
        let &(tier, mode) = CLAUDE_ROLE_MODES
            .iter()
            .find(|&&(_, mode)| mode == default_model)
            .unwrap_or(&CLAUDE_ROLE_MODES[0]);
        (Some(tier), mode)
    };
    std::iter::once(preferred).chain(
        CLAUDE_ROLE_MODES
            .iter()
            .filter(move |&&(_, mode)| mode != preferred.1)
            .map(|&(tier, mode)| (Some(tier), mode)),
    )
}

#[cfg(feature = "gui")]
pub(crate) fn native_configuration(endpoint: &str, api_key: &str) -> Value {
    json!({
        "inferenceProvider": "gateway",
        "inferenceCredentialKind": "static",
        "inferenceGatewayBaseUrl": endpoint,
        "inferenceGatewayApiKey": api_key,
        "inferenceGatewayAuthScheme": "bearer",
        "modelDiscoveryEnabled": true,
        "modelPrefer1mContext": true,
    })
}

#[cfg(feature = "gui")]
pub(crate) fn save_native_configuration(
    path: &std::path::Path,
    endpoint: &str,
    api_key: &str,
) -> anyhow::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("无法确定保存位置"))?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer_pretty(file.as_file_mut(), &native_configuration(endpoint, api_key))?;
    file.persist(path)?;
    Ok(())
}

pub(super) fn list_models(default_model: &str, confirmed_1m_modes: &HashSet<&str>) -> Vec<Value> {
    let created_at = Utc::now().to_rfc3339();
    model_routes(default_model)
        .map(|(tier, mode)| {
            let mut entry = json!({
                "type": "model",
                "id": model_id(tier),
                "display_name": model_display_name(mode),
                "created_at": created_at,
            });
            if let Some(tier) = tier {
                entry["anthropic_family_tier"] = json!(tier);
                entry["is_family_default"] = json!(true);
                if confirmed_1m_modes.contains(mode) {
                    entry["supports_1m"] = json!(true);
                }
            }
            entry
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::resolve_quick_model;

    #[test]
    fn resolves_desktop_ids_and_tier_aliases_to_quick_modes() {
        for (model, &(_, mode)) in list_models("smart", &HashSet::new())
            .iter()
            .zip(CLAUDE_ROLE_MODES)
        {
            assert_eq!(
                resolve_quick_model(model["id"].as_str().unwrap()),
                Some(mode)
            );
            assert_eq!(
                resolve_quick_model(model["anthropic_family_tier"].as_str().unwrap()),
                Some(mode)
            );
        }
        assert_eq!(resolve_quick_model(CLAUDE_DESKTOP_AUTO_ID), Some("auto"));
        assert_eq!(resolve_quick_model("claude-opus-5"), Some("smart"));
        assert_eq!(resolve_quick_model("claude-opus-quick-fast"), None);
        assert_eq!(resolve_quick_model("claude-opus-unknown"), None);
        assert_eq!(resolve_quick_model(""), None);
    }

    #[test]
    fn discovery_lists_the_default_first_and_marks_only_confirmed_fixed_modes() {
        for (mode, id) in [
            ("smart", "claude-opus-bridge"),
            ("balanced", "claude-sonnet-bridge"),
            ("fast", "claude-haiku-bridge"),
            ("auto", CLAUDE_DESKTOP_AUTO_ID),
        ] {
            let models = list_models(mode, &HashSet::from(["balanced", "auto"]));
            assert_eq!(models.len(), if mode == "auto" { 4 } else { 3 });
            assert_eq!(models[0]["id"], id);
            assert_eq!(models[0]["display_name"], model_display_name(mode));
            for model in models {
                assert_eq!(
                    model.get("supports_1m"),
                    (model["id"] == "claude-sonnet-bridge").then_some(&Value::Bool(true))
                );
            }
        }
    }

    #[cfg(feature = "gui")]
    #[test]
    fn native_config_prefers_1m_with_gateway_discovery() {
        let endpoint = "http://127.0.0.1:48765/claude-desktop";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop.json");
        save_native_configuration(&path, endpoint, "test-key").unwrap();
        let config: Value = serde_json::from_reader(std::fs::File::open(&path).unwrap()).unwrap();
        assert_eq!(config["inferenceGatewayBaseUrl"], endpoint);
        assert_eq!(config["inferenceGatewayApiKey"], "test-key");
        assert_eq!(config["inferenceGatewayAuthScheme"], "bearer");
        assert_eq!(config["modelDiscoveryEnabled"], true);
        assert_eq!(config["modelPrefer1mContext"], true);
        assert!(config.get("inferenceModels").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}
