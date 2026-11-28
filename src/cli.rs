//! 命令行接口（供无头或终端场景与 GPUI 桌面端并行使用）。

use crate::config::{
    DEFAULT_HOST, DEFAULT_PORT, THINKING_EFFORTS, load_config, remove_account, remove_all_accounts,
    rotate_api_key, save_config,
};
use crate::gateway::ccswitch::{
    APPS, CLAUDE_DESKTOP_APP, detect_lan_ip, endpoint_for, import_link,
};
use crate::gateway::start_server;
use crate::login::start_browser_sign_in;
use crate::pool::AccountPool;
use crate::qbs;
use crate::upstream::{TurnInput, UpstreamSession, chat};
use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::io::BufRead;
use tokio::sync::watch;

#[derive(Parser, Debug)]
#[command(
    name = "aws-quick-bridge-rs",
    version,
    about = "Amazon Quick (QuickWork) bridge in Rust with multi-account pool, Anthropic/OpenAI gateway, CC Switch & GPUI"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// 启动 GPUI 桌面控制中心（不传子命令时的默认行为）
    Gui,
    /// 在终端前台启动本地 HTTP 网关
    Serve {
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        port: Option<u16>,
    },
    /// 打开浏览器登录 Amazon Quick 账号并加入账号池
    Login {
        #[arg(long)]
        no_open: bool,
    },
    /// 查看网关状态、API Key、全部账号与额度
    Status,
    /// 列出或管理多账号池中的账号
    Accounts {
        /// 按邮箱 / account_id / key 设置主账号
        #[arg(long)]
        primary: Option<String>,
        /// 按邮箱 / account_id / key 移除账号
        #[arg(long)]
        remove: Option<String>,
    },
    /// 发送极简探测请求并输出上游 resolved_model_id（如 us.anthropic.claude-opus-5-5）
    Probe {
        #[arg(default_value = "smart")]
        models: Vec<String>,
        /// 指定思考档位，支持逗号分隔。默认沿用网关设置。
        #[arg(
            long,
            value_name = "LEVEL",
            value_delimiter = ',',
            value_parser = clap::builder::PossibleValuesParser::new(
                std::iter::once("auto").chain(THINKING_EFFORTS.iter().copied()),
            ),
        )]
        thinking_effort: Vec<String>,
    },
    /// 输出或直接打开 CC Switch 导入深链
    Ccswitch {
        #[arg(long)]
        open: bool,
        /// 指定客户端（如 claude-desktop），可用逗号分隔
        #[arg(long)]
        app: Option<String>,
        #[arg(long)]
        url: Option<String>,
        /// 导入预设模型（Claude Desktop 自动发现网关提供的模型）
        #[arg(long, default_value = "smart")]
        model: String,
        #[arg(long)]
        lan: bool,
    },
    /// 查看或重置本地网关 API Key
    Key {
        #[arg(long)]
        new: bool,
    },
    /// 移除本地保存的账号凭据（不传参数时清除全部账号）
    Logout { account: Option<String> },
}

fn display_base(host: &str, port: u16) -> String {
    let h = if host == "0.0.0.0" || host == "::" {
        "127.0.0.1"
    } else {
        host
    };
    format!("http://{h}:{port}")
}

pub async fn run_cli(cli: Cli) -> Result<()> {
    let cmd = cli.command.unwrap_or(Command::Gui);
    match cmd {
        Command::Gui => bail!("GUI should be launched from main()"),
        Command::Serve { host, port } => {
            let pool = AccountPool::load()?;
            let cfg = pool.config();
            let h = host.unwrap_or_else(|| {
                if cfg.host.is_empty() {
                    DEFAULT_HOST.to_string()
                } else {
                    cfg.host.clone()
                }
            });
            let p = port.unwrap_or(if cfg.port == 0 {
                DEFAULT_PORT
            } else {
                cfg.port
            });
            let mut srv = start_server(pool.clone(), &h, p).await?;
            let at = display_base(&h, srv.addr.port());
            let snap = pool.snapshot();
            println!("aws-quick-bridge-rs listening on http://{}", srv.addr);
            println!("  key:      {}", cfg.api_key);
            println!("  accounts: {} configured", snap.accounts.len());
            println!(
                "  Claude Code   {}   (Anthropic Messages)",
                endpoint_for("claude", &at)
            );
            println!(
                "  Claude Desktop {}   (Anthropic Messages)",
                endpoint_for(CLAUDE_DESKTOP_APP, &at)
            );
            println!(
                "  Codex         {}   (OpenAI Responses)",
                endpoint_for("codex", &at)
            );
            println!(
                "  OpenCode      {}   (OpenAI Chat Completions)",
                endpoint_for("opencode", &at)
            );
            if h == "0.0.0.0"
                && let Some(ip) = detect_lan_ip()
            {
                println!("  LAN URL:      http://{ip}:{}", srv.addr.port());
            }

            let mut rx = pool.subscribe();
            let mut printed = 0usize;
            let pool_log = pool.clone();
            tokio::spawn(async move {
                loop {
                    if rx.recv().await.is_err() {
                        break;
                    }
                    let logs = pool_log.snapshot().logs;
                    if logs.len() > printed {
                        for entry in &logs[printed..] {
                            println!(
                                "{} [{}] {}",
                                entry.timestamp.to_rfc3339(),
                                entry.api,
                                entry.message
                            );
                        }
                        printed = logs.len();
                    }
                }
            });

            tokio::signal::ctrl_c().await?;
            srv.stop();
            tokio::task::spawn_blocking(move || pool.flush_request_stats()).await?;
            Ok(())
        }
        Command::Login { no_open } => {
            let pool = AccountPool::load()?;
            let settings = pool.settings();
            let handle = start_browser_sign_in(&settings).await?;
            println!(
                "Sign in to Amazon Quick in the browser:\n\n  {}\n",
                handle.start_url
            );
            if !no_open {
                let _ = open::that(&handle.start_url);
            }

            // 支持从远程浏览器复制 localhost 回调地址后直接粘贴到标准输入完成登录。
            let _stdin_task = std::thread::spawn(|| {
                let stdin = std::io::stdin();
                for line in stdin.lock().lines().map_while(Result::ok) {
                    let trimmed = line.trim().to_string();
                    if let Ok(mut u) = url::Url::parse(&trimmed)
                        && matches!(u.host_str(), Some("localhost") | Some("127.0.0.1"))
                    {
                        let _ = u.set_host(Some("127.0.0.1"));
                        let client = reqwest::Client::builder()
                            .redirect(reqwest::redirect::Policy::none())
                            .build();
                        if let Ok(c) = client {
                            let url_str = u.to_string();
                            if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                            {
                                rt.block_on(async {
                                        if let Ok(r) = c.get(&url_str).send().await
                                            && let Some(loc) = r.headers().get("location").and_then(|v| v.to_str().ok()) {
                                                println!("\nNow open this page, and paste the address it ends on:\n\n  {loc}\n");
                                            }
                                    });
                            }
                        }
                    }
                }
            });

            let record = handle.wait().await?;
            let label = record.display_label();
            pool.add_or_update_account(record)?;
            println!("Signed in as {label}.");
            Ok(())
        }
        Command::Status => {
            let pool = AccountPool::load()?;
            let cfg = pool.config();
            let settings = pool.settings();
            let at = display_base(&cfg.host, cfg.port);
            let up = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(2))
                .build()?
                .get(format!("{at}/health"))
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            println!(
                "API:      {at}  ({})",
                if up { "running" } else { "not running" }
            );
            println!("Bind:     {}:{}", cfg.host, cfg.port);
            println!("Key:      {}", cfg.api_key);
            let snap = pool.snapshot();
            if snap.accounts.is_empty() {
                println!("Accounts: none (run `aws-quick-bridge-rs login`)");
                return Ok(());
            }
            println!("Accounts: {}", snap.accounts.len());
            for acc in snap.accounts {
                let mark = if acc.is_primary { "*" } else { " " };
                println!(
                    "  {mark} {} [{}] ({})",
                    acc.label,
                    acc.region,
                    if acc.enabled { "enabled" } else { "disabled" }
                );
                if let Ok(fresh) = pool.ensure_fresh(&acc.key, false).await {
                    match qbs::usage(&fresh, &settings).await {
                        Ok(u) => {
                            println!("      Plan: {}", u.plan);
                            for w in u.windows {
                                println!("      {}: {}", w.name, w.display);
                            }
                        }
                        Err(e) => println!("      Quota: unavailable ({})", e.message),
                    }
                }
            }
            Ok(())
        }
        Command::Accounts { primary, remove } => {
            let pool = AccountPool::load()?;
            if let Some(target) = remove {
                let snap = pool.snapshot();
                let key = snap
                    .accounts
                    .iter()
                    .find(|a| a.key == target || a.label == target)
                    .map(|a| a.key.clone())
                    .unwrap_or(target);
                pool.delete_account(&key)?;
                println!("Removed account.");
                return Ok(());
            }
            if let Some(target) = primary {
                let snap = pool.snapshot();
                let key = snap
                    .accounts
                    .iter()
                    .find(|a| a.key == target || a.label == target)
                    .map(|a| a.key.clone())
                    .ok_or_else(|| anyhow::anyhow!("account not found: {target}"))?;
                pool.set_primary(&key)?;
                println!("Primary account updated.");
                return Ok(());
            }
            let snap = pool.snapshot();
            for acc in snap.accounts {
                println!(
                    "{} {} | {} | enabled={}",
                    if acc.is_primary { "*" } else { "-" },
                    acc.label,
                    acc.region,
                    acc.enabled
                );
            }
            Ok(())
        }
        Command::Probe {
            models,
            thinking_effort,
        } => {
            let pool = AccountPool::load()?;
            let cfg = pool.settings();
            let Some(picked) = pool.pick_account("", &std::collections::HashSet::new()) else {
                bail!("not signed in; run `aws-quick-bridge-rs login`");
            };
            let auth = pool
                .ensure_fresh(&picked.key(), false)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let session = UpstreamSession::from_auth(&auth, &cfg);
            let (_tx, cancel_rx) = watch::channel(false);
            let efforts = if thinking_effort.is_empty() {
                vec![cfg.thinking_effort.as_str()]
            } else {
                thinking_effort.iter().map(String::as_str).collect()
            };
            for model in models {
                for &effort in &efforts {
                    let label = if effort.is_empty() { "auto" } else { effort };
                    let input = TurnInput {
                        prompt: "hi".to_string(),
                        model: model.clone(),
                        thread_id: String::new(),
                        title: "probe".to_string(),
                        system_prompt: String::new(),
                        thinking_effort: if effort == "auto" {
                            String::new()
                        } else {
                            effort.to_owned()
                        },
                        web_search: false,
                        resume: false,
                        attachments: Vec::new(),
                    };
                    let res = chat(&session, &cfg, &input, cancel_rx.clone(), None)
                        .await
                        .map_err(|error| {
                            anyhow::anyhow!("probe {model} (effort={label}): {}", error.message)
                        })?;
                    println!(
                        "probe {model}: resolved_model_id={} (in={}, out={}, cached={}) effort={label}",
                        if res.usage.resolved_model_id.is_empty() {
                            "(none)"
                        } else {
                            &res.usage.resolved_model_id
                        },
                        res.usage.prompt,
                        res.usage.completion,
                        res.usage.cache_read,
                    );
                }
            }
            Ok(())
        }
        Command::Ccswitch {
            open: do_open,
            app,
            url,
            model,
            lan,
        } => {
            let cfg = load_config()?;
            let base = if let Some(u) = url {
                u
            } else if lan {
                let ip = detect_lan_ip().unwrap_or_else(|| "127.0.0.1".to_string());
                format!("http://{ip}:{}", cfg.port)
            } else {
                display_base(&cfg.host, cfg.port)
            };
            let list: Vec<String> = match app {
                Some(a) => a.split(',').map(|s| s.trim().to_string()).collect(),
                None => APPS.iter().map(|a| a.key.to_string()).collect(),
            };
            for item in list {
                let link = import_link(&item, &base, &cfg.api_key, &model, "Amazon Quick")?;
                println!("{item}:\n  {link}\n");
                if do_open {
                    let _ = open::that(&link);
                }
            }
            Ok(())
        }
        Command::Key { new } => {
            if new {
                let k = rotate_api_key()?;
                println!("{k}\nRestart serve and update client keys.");
            } else {
                let cfg = load_config()?;
                println!("{}", cfg.api_key);
            }
            Ok(())
        }
        Command::Logout { account } => {
            if let Some(a) = account {
                let _ = remove_account(&a)?;
                println!("Removed account {a}.");
            } else {
                remove_all_accounts()?;
                let cfg = load_config()?;
                save_config(&cfg)?;
                println!("All accounts removed on this computer.");
            }
            Ok(())
        }
    }
}
