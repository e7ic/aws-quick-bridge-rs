//! CC Switch 一键导入深链生成与本机局域网 IP 探测。

use anyhow::{Result, bail};
use std::net::{IpAddr, Ipv4Addr, UdpSocket};

pub const CLAUDE_DESKTOP_APP: &str = "claude-desktop";

#[derive(Debug, Clone, Copy)]
pub struct AppSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub api_format: &'static str,
    pub v1: bool,
}

pub const APPS: &[AppSpec] = &[
    AppSpec {
        key: "claude",
        label: "Claude Code",
        api_format: "anthropic",
        v1: false,
    },
    AppSpec {
        key: CLAUDE_DESKTOP_APP,
        label: "Claude Desktop",
        api_format: "anthropic",
        v1: false,
    },
    AppSpec {
        key: "codex",
        label: "Codex",
        api_format: "openai_responses",
        v1: true,
    },
    AppSpec {
        key: "opencode",
        label: "OpenCode",
        api_format: "openai_chat",
        v1: true,
    },
];

pub fn find_app(app: &str) -> Option<AppSpec> {
    APPS.iter().copied().find(|a| a.key == app.trim())
}

pub fn endpoint_for(app: &str, base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    match find_app(app) {
        Some(spec) if spec.key == CLAUDE_DESKTOP_APP => {
            format!("{trimmed}{}", super::claude_desktop::BASE_PATH)
        }
        Some(spec) if spec.v1 => format!("{trimmed}/v1"),
        _ => trimmed.to_string(),
    }
}

pub fn import_link(
    app: &str,
    base: &str,
    api_key: &str,
    model: &str,
    name: &str,
) -> Result<String> {
    let Some(spec) = find_app(app) else {
        bail!("unknown app {app:?}. Use claude, claude-desktop, codex, or opencode");
    };
    let ep = endpoint_for(spec.key, base);
    let mut pairs: Vec<(&str, String)> = vec![
        ("resource", "provider".to_string()),
        ("app", spec.key.to_string()),
        ("name", name.to_string()),
        ("endpoint", ep),
        ("apiKey", api_key.to_string()),
    ];
    if spec.key == CLAUDE_DESKTOP_APP {
        // An Anthropic provider without model overrides uses direct model discovery.
        pairs.push(("enabled", "true".to_string()));
    } else {
        pairs.push(("model", model.to_string()));
    }
    if spec.key == "claude" {
        pairs.push(("haikuModel", "fast".to_string()));
        pairs.push(("sonnetModel", model.to_string()));
        pairs.push(("opusModel", model.to_string()));
    }
    pairs.extend([
        ("apiFormat", spec.api_format.to_string()),
        ("icon", "aws".to_string()),
        (
            "notes",
            "Amazon Quick via aws-quick-bridge-rs; keep the bridge gateway running.".to_string(),
        ),
    ]);

    let query = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    Ok(format!("ccswitch://v1/import?{query}"))
}

/// 优先查询本地多播路由的源地址。UDP connect 只选择路由，不发送报文。
pub fn detect_lan_ip() -> Option<String> {
    [
        (Ipv4Addr::new(224, 0, 0, 251), 5353),
        (Ipv4Addr::new(8, 8, 8, 8), 80),
    ]
    .into_iter()
    .find_map(|destination| {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
        socket.connect(destination).ok()?;
        let IpAddr::V4(ip) = socket.local_addr().ok()?.ip() else {
            return None;
        };
        usable_lan_address(ip).then(|| ip.to_string())
    })
}

fn usable_lan_address(ip: Ipv4Addr) -> bool {
    !ip.is_loopback()
        && !ip.is_unspecified()
        && !ip.is_link_local()
        && !matches!(ip.octets(), [198, 18 | 19, ..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_out_proxy_and_loopback_source_addresses() {
        assert!(!usable_lan_address(Ipv4Addr::new(198, 18, 0, 1)));
        assert!(!usable_lan_address(Ipv4Addr::LOCALHOST));
        assert!(usable_lan_address(Ipv4Addr::new(172, 18, 0, 9)));
    }
}
