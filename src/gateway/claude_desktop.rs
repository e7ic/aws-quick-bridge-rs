//! Claude Desktop 模型发现。

use crate::config::{CLAUDE_ROLE_MODES, model_display_name};
use chrono::Utc;
use serde_json::{Value, json};

pub(super) const BASE_PATH: &str = "/claude-desktop";

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
            json!({
                "name": model,
                "labelOverride": model_display_name(model),
            })
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

pub(super) fn list_models() -> Vec<Value> {
    let created_at = Utc::now().to_rfc3339();
    CLAUDE_ROLE_MODES
        .iter()
        .map(|&(role, mode)| {
            json!({
                "type": "model",
                "id": format!("claude-{role}-bridge"),
                "display_name": model_display_name(mode),
                "created_at": created_at,
                "anthropic_family_tier": role,
                "is_family_default": true,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::resolve_quick_model;

    #[test]
    fn resolves_desktop_ids_and_tier_aliases_to_quick_modes() {
        for (model, &(_, mode)) in list_models().iter().zip(CLAUDE_ROLE_MODES) {
            assert_eq!(
                resolve_quick_model(model["id"].as_str().unwrap()),
                Some(mode)
            );
            assert_eq!(
                resolve_quick_model(model["anthropic_family_tier"].as_str().unwrap()),
                Some(mode)
            );
        }
        assert_eq!(resolve_quick_model("claude-opus-5"), Some("smart"));
        assert_eq!(resolve_quick_model("claude-opus-quick-fast"), None);
        assert_eq!(resolve_quick_model("claude-opus-unknown"), None);
        assert_eq!(resolve_quick_model(""), None);
    }

    #[cfg(feature = "gui")]
    #[test]
    fn native_config_uses_bridge_modes_and_selected_default() {
        let endpoint = "http://127.0.0.1:48765/claude-desktop";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop.json");
        save_native_configuration(&path, endpoint, "test-key", "balanced").unwrap();
        let config: Value = serde_json::from_reader(std::fs::File::open(&path).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(config["inferenceGatewayBaseUrl"], endpoint);
        assert_eq!(config["inferenceGatewayApiKey"], "test-key");
        assert_eq!(config["inferenceGatewayAuthScheme"], "bearer");
        assert_eq!(
            config["inferenceModels"][0]["labelOverride"],
            "Quick Balanced"
        );
        for model in config["inferenceModels"].as_array().unwrap() {
            let name = model["name"].as_str().unwrap();
            assert_eq!(resolve_quick_model(name), Some(name));
        }
    }
}
