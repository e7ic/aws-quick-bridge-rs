//! Claude Desktop 模型发现。

use crate::config::{CLAUDE_DESKTOP_AUTO_ID, CLAUDE_ROLE_MODES, model_display_name};
use chrono::Utc;
use serde_json::{Value, json};

pub(super) const BASE_PATH: &str = "/claude-desktop";

fn model_id(tier: Option<&str>) -> String {
    match tier {
        Some(tier) => format!("claude-{tier}-bridge"),
        None => CLAUDE_DESKTOP_AUTO_ID.to_string(),
    }
}

#[cfg(feature = "gui")]
pub(crate) fn native_configuration(endpoint: &str, api_key: &str, default_model: &str) -> Value {
    let default_model = match default_model {
        "balanced" | "fast" | "auto" => default_model,
        _ => "smart",
    };
    let models: Vec<_> = std::iter::once(default_model)
        .chain(
            ["smart", "balanced", "fast"]
                .into_iter()
                .filter(|mode| *mode != default_model),
        )
        .map(|model| {
            let tier = CLAUDE_ROLE_MODES
                .iter()
                .find_map(|&(tier, mode)| (mode == model).then_some(tier));
            let mut entry = json!({
                "name": model_id(tier),
                "labelOverride": model_display_name(model),
            });
            if let Some(tier) = tier {
                entry["anthropicFamilyTier"] = json!(tier);
            }
            entry
        })
        .collect();
    json!({
        "inferenceProvider": "gateway",
        "inferenceCredentialKind": "static",
        "inferenceGatewayBaseUrl": endpoint,
        "inferenceGatewayApiKey": api_key,
        "inferenceGatewayAuthScheme": "bearer",
        "inferenceModels": models,
    })
}

#[cfg(feature = "gui")]
pub(crate) fn save_native_configuration(
    path: &std::path::Path,
    endpoint: &str,
    api_key: &str,
    default_model: &str,
) -> anyhow::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("无法确定保存位置"))?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer_pretty(
        file.as_file_mut(),
        &native_configuration(endpoint, api_key, default_model),
    )?;
    file.persist(path)?;
    Ok(())
}

pub(super) fn list_models(include_auto: bool) -> Vec<Value> {
    let created_at = Utc::now().to_rfc3339();
    include_auto
        .then_some((None, "auto"))
        .into_iter()
        .chain(
            CLAUDE_ROLE_MODES
                .iter()
                .map(|&(tier, mode)| (Some(tier), mode)),
        )
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
        for (model, &(_, mode)) in list_models(false).iter().zip(CLAUDE_ROLE_MODES) {
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

    #[cfg(feature = "gui")]
    #[test]
    fn native_config_uses_advertised_models_and_selected_default() {
        let endpoint = "http://127.0.0.1:48765/claude-desktop";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop.json");
        for (mode, id) in [
            ("smart", "claude-opus-bridge"),
            ("balanced", "claude-sonnet-bridge"),
            ("fast", "claude-haiku-bridge"),
            ("auto", CLAUDE_DESKTOP_AUTO_ID),
        ] {
            save_native_configuration(&path, endpoint, "test-key", mode).unwrap();
            let config: Value =
                serde_json::from_reader(std::fs::File::open(&path).unwrap()).unwrap();
            assert_eq!(config["inferenceGatewayBaseUrl"], endpoint);
            assert_eq!(config["inferenceGatewayApiKey"], "test-key");
            assert_eq!(config["inferenceGatewayAuthScheme"], "bearer");
            let configured = config["inferenceModels"].as_array().unwrap();
            let advertised = list_models(mode == "auto");
            assert_eq!(configured.len(), advertised.len());
            assert_eq!(configured[0]["name"], id);
            assert_eq!(configured[0]["labelOverride"], model_display_name(mode));

            for model in configured {
                let route = advertised
                    .iter()
                    .find(|entry| entry["id"] == model["name"])
                    .expect("every configured model must be advertised by the Desktop gateway");
                assert_eq!(model["labelOverride"], route["display_name"]);
                assert_eq!(model["anthropicFamilyTier"], route["anthropic_family_tier"]);
                assert!(model.get("behavesAs").is_none());
                assert!(model.get("supports1m").is_none());
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}
