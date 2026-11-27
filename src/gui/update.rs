mod feed;
mod install;

use super::BridgeApp;
use super::components::{BtnVariant, Theme};
use anyhow::{Context as _, Result};
use feed::PreparedUpdate;
use gpui::{AnyElement, Context, IntoElement, Timer};
use install::{InstallAction, InstallLocation};
use std::time::Duration;

pub(super) use install::startup;

const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);
const VERSION: &str = concat!("当前版本 v", env!("CARGO_PKG_VERSION"));
type UpdateClick = fn(&mut BridgeApp, &mut Context<BridgeApp>);

enum Status {
    Idle,
    Checking,
    Current,
    Ready(PreparedUpdate),
    Waiting(PreparedUpdate, InstallAction),
    Installing,
    Failed(String),
}

pub(super) struct Updates {
    location: InstallLocation,
    key: [u8; 32],
    status: Status,
}

impl Updates {
    pub(super) fn for_current_install() -> Option<Self> {
        Some(Self {
            key: feed::public_key()?,
            location: InstallLocation::current()?,
            status: Status::Idle,
        })
    }
}

impl BridgeApp {
    pub(super) fn initialize_updates(
        &mut self,
        startup: &install::Startup,
        cx: &mut Context<Self>,
    ) {
        if startup.installed_update() {
            self.set_toast(concat!("已更新到 v", env!("CARGO_PKG_VERSION")), false);
        }
        let mut first_check = Duration::from_secs(3);
        if let Some(error) = startup.take_failure() {
            if let Some(updates) = &mut self.updates {
                updates.status = Status::Failed(error.clone());
            }
            self.set_toast(format!("更新失败，{error}"), true);
            first_check = CHECK_INTERVAL;
        }
        if startup.resume_gateway {
            self.start_gateway();
        }
        if self.updates.is_none() {
            return;
        }
        self.rt.spawn_blocking(feed::purge_finished_downloads);
        cx.spawn(async move |this, cx| {
            Timer::after(first_check).await;
            while this.update(cx, |app, cx| app.check_for_updates(cx)).is_ok() {
                Timer::after(CHECK_INTERVAL).await;
            }
        })
        .detach();
    }

    fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        let Some(updates) = &mut self.updates else {
            return;
        };
        if !matches!(
            updates.status,
            Status::Idle | Status::Current | Status::Failed(_)
        ) {
            return;
        }
        updates.status = Status::Checking;
        let key = updates.key;
        let task = self.rt.spawn(async move {
            let _ = tokio::task::spawn_blocking(feed::purge_finished_downloads).await;
            feed::download_latest(key).await
        });
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = task.await.context("检查更新失败").and_then(|result| result);
            let _ = this.update(cx, |app, cx| {
                let status = match result {
                    Ok(Some(prepared)) => {
                        app.set_toast(format!("v{} 已下载，可重启更新", prepared.version), false);
                        Status::Ready(prepared)
                    }
                    Ok(None) => Status::Current,
                    Err(error) => Status::Failed(error.to_string()),
                };
                if let Some(updates) = &mut app.updates {
                    updates.status = status;
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn install_update_on_quit(&mut self, cx: &mut Context<Self>) -> bool {
        self.queue_update(InstallAction::Exit, cx)
    }

    fn queue_update(&mut self, action: InstallAction, cx: &mut Context<Self>) -> bool {
        let Some(updates) = &mut self.updates else {
            return false;
        };
        let previous = std::mem::replace(&mut updates.status, Status::Idle);
        updates.status = match previous {
            Status::Ready(prepared) | Status::Waiting(prepared, _) => {
                Status::Waiting(prepared, action)
            }
            Status::Installing => {
                updates.status = Status::Installing;
                return true;
            }
            other => {
                updates.status = other;
                return false;
            }
        };
        self.start_waiting_update(cx);
        if self.update_is_waiting() {
            self.set_toast("当前请求结束后安装更新", false);
        }
        cx.notify();
        true
    }

    fn update_is_waiting(&self) -> bool {
        self.updates
            .as_ref()
            .is_some_and(|updates| matches!(updates.status, Status::Waiting(..)))
    }

    fn cancel_waiting_update(&mut self, cx: &mut Context<Self>) {
        if let Some(updates) = &mut self.updates
            && matches!(updates.status, Status::Waiting(..))
            && let Status::Waiting(prepared, _) =
                std::mem::replace(&mut updates.status, Status::Idle)
        {
            updates.status = Status::Ready(prepared);
            cx.notify();
        }
    }

    pub(super) fn start_waiting_update(&mut self, cx: &mut Context<Self>) {
        if !self.update_is_waiting()
            || self.shared.lock().gateway_busy
            || !self.pool.requests_idle()
        {
            return;
        }
        let Some(updates) = &mut self.updates else {
            return;
        };
        let Status::Waiting(prepared, action) =
            std::mem::replace(&mut updates.status, Status::Installing)
        else {
            return;
        };
        let was_running = {
            let mut shared = self.shared.lock();
            shared.gateway_busy = true;
            shared.server_running
        };
        let location = updates.location.clone();
        let server = self.server.clone();
        let pool = self.pool.clone();
        let task = self.rt.spawn(async move {
            let server = server.lock().take();
            if let Some(server) = server {
                server.stop_and_wait().await;
            }
            // A disconnected stream can still be waiting for Quick's stop acknowledgement.
            pool.wait_for_request_idle().await;
            tokio::task::spawn_blocking(move || location.hand_off(prepared, action, was_running))
                .await
                .context("准备安装更新失败")?
        });
        self.bump_ui();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result: Result<()> = task.await.context("更新任务失败").and_then(|result| result);
            let _ = this.update(cx, |app, cx| match result {
                Ok(()) => cx.quit(),
                Err(error) => {
                    let error = error.to_string();
                    if action == InstallAction::Exit {
                        install::record_failure(&error);
                        cx.quit();
                        return;
                    }
                    {
                        let mut shared = app.shared.lock();
                        shared.gateway_busy = false;
                        shared.server_running = false;
                    }
                    if let Some(updates) = &mut app.updates {
                        updates.status = Status::Failed(error.clone());
                    }
                    if was_running {
                        app.start_gateway();
                    }
                    app.set_toast(format!("更新失败，{error}"), true);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn software_update_row(&self, t: Theme, cx: &mut Context<Self>) -> AnyElement {
        let (detail, label, variant, action): (_, _, _, UpdateClick) =
            match self.updates.as_ref().map(|updates| &updates.status) {
                None => {
                    return Self::setting_row(t, "版本", VERSION, gpui::div().into_any_element())
                        .into_any_element();
                }
                Some(status @ (Status::Checking | Status::Installing)) => {
                    let detail = if matches!(status, Status::Checking) {
                        "正在检查并下载更新"
                    } else {
                        "正在安装更新"
                    };
                    return Self::setting_row(
                        t,
                        "软件更新",
                        detail,
                        Self::skeleton_bar(t, "update-loading", Some(84.0), 12.0, 6.0),
                    )
                    .into_any_element();
                }
                Some(Status::Idle) => (
                    VERSION.to_owned(),
                    "检查更新",
                    BtnVariant::Secondary,
                    Self::check_for_updates,
                ),
                Some(Status::Current) => (
                    format!("{VERSION} · 已是最新版本"),
                    "检查更新",
                    BtnVariant::Secondary,
                    Self::check_for_updates,
                ),
                Some(Status::Ready(prepared)) => (
                    format!("v{} 已下载，退出时自动安装", prepared.version),
                    "重启更新",
                    BtnVariant::Primary,
                    |app, cx| {
                        app.queue_update(InstallAction::Restart, cx);
                    },
                ),
                Some(Status::Waiting(_, action)) => (
                    if *action == InstallAction::Restart {
                        "当前请求结束后重启更新"
                    } else {
                        "当前请求结束后退出并更新"
                    }
                    .to_owned(),
                    "取消",
                    BtnVariant::Secondary,
                    Self::cancel_waiting_update,
                ),
                Some(Status::Failed(error)) => (
                    error.clone(),
                    "重试",
                    BtnVariant::Secondary,
                    Self::check_for_updates,
                ),
            };
        Self::setting_row(
            t,
            "软件更新",
            detail,
            Self::button(
                t,
                "software-update",
                None,
                false,
                label,
                variant,
                move |app, _, cx| {
                    action(app, cx);
                },
                cx,
            ),
        )
        .into_any_element()
    }
}
