use super::feed::{self, ARCHIVE, FINISHED, LEASE, MANIFEST, PreparedUpdate};
use anyhow::{Context, Result, bail, ensure};
use semver::Version;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(test)]
mod tests;

const APPLY: &str = "--quick-bridge-apply-update";
const REOPEN: &str = "--quick-bridge-after-update";
const READY: &str = "launch-ready";
const FAILED: &str = "last-install-error";
#[cfg(target_os = "macos")]
const HELPER_NAME: &str = "quick-bridge-update";
#[cfg(target_os = "windows")]
const HELPER_NAME: &str = "quick-bridge-update.exe";
#[cfg(target_os = "windows")]
const GUI_EXE: &str = "Quick Bridge.exe";
#[cfg(target_os = "windows")]
const CLI_EXE: &str = "aws-quick-bridge-rs.exe";

// The replacement process acquires this lease after the original process exits.
static HANDOFF: Mutex<Option<File>> = Mutex::new(None);

pub(super) fn record_failure(message: &str) {
    let _ = fs::write(feed::cache_dir().join(FAILED), message);
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum InstallAction {
    Exit,
    Restart,
}

#[derive(Clone)]
pub(super) struct InstallLocation {
    executable: PathBuf,
    #[cfg(target_os = "macos")]
    bundle: PathBuf,
}

impl InstallLocation {
    pub(super) fn current() -> Option<Self> {
        Self::from_executable(std::env::current_exe().ok()?).ok()
    }

    fn from_executable(executable: PathBuf) -> Result<Self> {
        ensure!(
            executable.is_absolute() && fs::symlink_metadata(&executable)?.is_file(),
            "无法定位 Quick Bridge 安装文件"
        );
        #[cfg(target_os = "macos")]
        {
            let macos = executable.parent().context("应用缺少 MacOS 目录")?;
            let contents = macos.parent().context("应用缺少 Contents 目录")?;
            let bundle = contents.parent().context("应用缺少安装目录")?;
            ensure!(
                executable.file_name() == Some(OsStr::new("QuickBridge"))
                    && macos.file_name() == Some(OsStr::new("MacOS"))
                    && contents.file_name() == Some(OsStr::new("Contents"))
                    && bundle.extension() == Some(OsStr::new("app"))
                    && fs::symlink_metadata(bundle)?.is_dir(),
                "仅正式的 Quick Bridge 应用包可执行更新"
            );
            Ok(Self {
                bundle: bundle.to_path_buf(),
                executable,
            })
        }
        #[cfg(target_os = "windows")]
        {
            ensure!(
                executable
                    .file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|name| name.eq_ignore_ascii_case(GUI_EXE)),
                "仅正式的 Quick Bridge 便携程序可执行更新"
            );
            Ok(Self { executable })
        }
    }

    fn staging_dir(&self) -> Result<tempfile::TempDir> {
        #[cfg(target_os = "macos")]
        let parent = self.bundle.parent();
        #[cfg(target_os = "windows")]
        let parent = self.executable.parent();
        let parent = parent.context("无法定位应用安装目录")?;
        tempfile::Builder::new()
            .prefix(".quick-bridge-update-")
            .tempdir_in(parent)
            .with_context(|| format!("无法在应用所在目录准备更新 {}", parent.display()))
    }

    pub(super) fn hand_off(
        &self,
        prepared: PreparedUpdate,
        action: InstallAction,
        resume_gateway: bool,
    ) -> Result<()> {
        let mut handoff = HANDOFF
            .lock()
            .map_err(|_| anyhow::anyhow!("无法交接更新安装"))?;
        ensure!(handoff.is_none(), "更新已交给安装程序");
        let helper = prepared.directory.path().join(HELPER_NAME);
        fs::copy(&self.executable, &helper).context("无法准备更新安装程序")?;
        let mut command = background_command(helper);
        let home = std::path::absolute(crate::config::home_dir())?;
        command
            .current_dir(prepared.directory.path())
            .env("AWS_QUICK_BRIDGE_HOME", home)
            .arg(APPLY)
            .arg(&self.executable)
            .arg(if action == InstallAction::Restart {
                "restart"
            } else {
                "exit"
            })
            .arg(if resume_gateway { "running" } else { "stopped" })
            .spawn()
            .context("无法启动更新安装程序")?;
        *handoff = Some(prepared.lease);
        let _ = prepared.directory.keep();
        Ok(())
    }
}

#[derive(Default)]
pub(in crate::gui) struct Startup {
    pub(super) resume_gateway: bool,
    launch_job: Option<PathBuf>,
}

impl Startup {
    pub(in crate::gui) fn acknowledge(&self) -> Result<()> {
        if let Some(job) = &self.launch_job {
            File::create_new(job.join(READY)).context("无法确认新版本已启动")?;
        }
        Ok(())
    }

    pub(super) fn installed_update(&self) -> bool {
        self.launch_job.is_some()
    }

    pub(super) fn take_failure(&self) -> Option<String> {
        let path = feed::cache_dir().join(FAILED);
        let message = fs::read_to_string(&path).ok()?;
        let _ = fs::remove_file(path);
        Some(message)
    }
}

/// An installer invocation exits before constructing GPUI or opening the account store.
pub(in crate::gui) fn startup() -> Result<Option<Startup>> {
    let mut args = std::env::args_os().skip(1);
    match args.next().as_deref().and_then(OsStr::to_str) {
        Some(APPLY) => {
            let executable = args.next().context("缺少原安装文件路径")?.into();
            let action = match args.next().as_deref().and_then(OsStr::to_str) {
                Some("exit") => InstallAction::Exit,
                Some("restart") => InstallAction::Restart,
                _ => bail!("无效的更新安装方式"),
            };
            let resume = args.next().as_deref() == Some(OsStr::new("running"));
            run_helper(executable, action, resume)?;
            Ok(None)
        }
        Some(REOPEN) => {
            let resume_gateway = args.next().as_deref() == Some(OsStr::new("running"));
            let launch_job = args.next().map(PathBuf::from);
            if let Some(job) = &launch_job {
                ensure!(feed::is_job_path(job), "无效的更新状态目录");
            }
            Ok(Some(Startup {
                resume_gateway,
                launch_job,
            }))
        }
        _ => Ok(Some(Startup::default())),
    }
}

fn background_command(executable: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(executable);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt as _;
        // CREATE_NO_WINDOW keeps the installer and archive extraction in the background.
        command.creation_flags(0x0800_0000);
    }
    command
}

fn acquire_parent_lease(job: &Path) -> Result<File> {
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .open(job.join(LEASE))?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match lease.try_lock() {
            Ok(()) => return Ok(lease),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(TryLockError::WouldBlock) => bail!("等待 Quick Bridge 退出超时"),
            Err(error) => return Err(error.into()),
        }
    }
}

fn run_helper(original: PathBuf, action: InstallAction, resume_gateway: bool) -> Result<()> {
    let helper = std::env::current_exe()?;
    let job = helper.parent().context("无法定位更新缓存")?;
    ensure!(
        helper.file_name() == Some(OsStr::new(HELPER_NAME)) && feed::is_job_path(job),
        "无效的更新安装程序路径"
    );
    let lease = acquire_parent_lease(job)?;
    let install_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(feed::cache_dir().join("apply.lock"))?;
    install_lock.lock().context("无法锁定更新安装")?;
    let result = (|| {
        let location = InstallLocation::from_executable(original.clone())?;
        let key = feed::public_key().context("缺少更新签名公钥")?;
        let version = feed::verify_download(job, &key)?;
        ensure!(
            feed::file_digest(&helper)? == feed::file_digest(&original)?,
            "安装文件已变化，请重新检查更新"
        );
        apply_package(&location, &job.join(ARCHIVE), &version, || {
            if action == InstallAction::Restart {
                relaunch(&original, resume_gateway, Some(job))?;
            }
            Ok(())
        })
    })();
    if let Err(error) = &result {
        record_failure(&format!("{error:#}"));
        if action == InstallAction::Restart && original.is_file() {
            let _ = relaunch(&original, resume_gateway, None);
        }
    }
    let _ = fs::remove_file(job.join(ARCHIVE));
    let _ = fs::remove_file(job.join(MANIFEST));
    let _ = File::create(job.join(FINISHED));
    drop(lease);
    // A later update check removes the helper on Windows after its executable is unlocked.
    #[cfg(target_os = "macos")]
    let _ = fs::remove_dir_all(job);
    result
}

fn relaunch(executable: &Path, resume_gateway: bool, job: Option<&Path>) -> Result<()> {
    let mut command = background_command(executable);
    command.current_dir(executable.parent().context("无法定位启动目录")?);
    command
        .arg(REOPEN)
        .arg(if resume_gateway { "running" } else { "stopped" });
    if let Some(job) = job {
        command.arg(job);
    }
    let mut child = command.spawn().context("更新后启动 Quick Bridge 失败")?;
    let Some(job) = job else {
        return Ok(());
    };
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if job.join(READY).is_file() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            bail!("更新后 Quick Bridge 未能启动（{status}）");
        }
        if Instant::now() >= deadline && !job.join(READY).is_file() {
            child.kill().context("无法停止启动超时的新版本")?;
            let _ = child.wait();
            bail!("等待新版本启动超时");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(target_os = "macos")]
fn apply_package(
    location: &InstallLocation,
    archive: &Path,
    version: &Version,
    after_install: impl FnOnce() -> Result<()>,
) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    fn exchange(left: &Path, right: &Path) -> Result<()> {
        let left = CString::new(left.as_os_str().as_bytes())?;
        let right = CString::new(right.as_os_str().as_bytes())?;
        // SAFETY: The two C strings live throughout the call and point to sibling-volume paths.
        let result = unsafe { libc::renamex_np(left.as_ptr(), right.as_ptr(), libc::RENAME_SWAP) };
        if result != 0 {
            return Err(std::io::Error::last_os_error()).context("替换应用包失败");
        }
        Ok(())
    }

    fn plist_value(bundle: &Path, key: &str) -> Result<String> {
        let output = Command::new("/usr/bin/plutil")
            .args(["-extract", key, "raw", "-o", "-"])
            .arg(bundle.join("Contents/Info.plist"))
            .output()?;
        ensure!(output.status.success(), "无法读取应用包信息 {key}");
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }

    let staging = location.staging_dir()?;
    let status = background_command("/usr/bin/ditto")
        .args(["-x", "-k"])
        .arg(archive)
        .arg(staging.path())
        .status()
        .context("无法解压 macOS 更新包")?;
    ensure!(status.success(), "解压 macOS 更新包失败");
    let bundle = staging.path().join("Quick Bridge.app");
    let _ = InstallLocation::from_executable(bundle.join("Contents/MacOS/QuickBridge"))?;
    ensure!(
        plist_value(&bundle, "CFBundleIdentifier")? == "org.quickbridge.desktop"
            && plist_value(&bundle, "CFBundleShortVersionString")? == version.to_string()
            && plist_value(&location.bundle, "CFBundleIdentifier")? == "org.quickbridge.desktop",
        "更新包的应用标识或版本不符"
    );
    let status = background_command("/usr/bin/codesign")
        .args(["--verify", "--strict", "--deep"])
        .arg(&bundle)
        .status()?;
    ensure!(status.success(), "macOS 更新包的应用签名无效");

    exchange(&location.bundle, &bundle)?;
    if let Err(error) = after_install() {
        if let Err(rollback) = exchange(&location.bundle, &bundle) {
            let backup = staging.keep();
            bail!(
                "{error:#}。恢复旧版失败：{rollback:#}。备份位于 {}",
                backup.display()
            );
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn apply_package(
    location: &InstallLocation,
    archive: &Path,
    _version: &Version,
    after_install: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let staging = location.staging_dir()?;
    let mut archive =
        zip::ZipArchive::new(File::open(archive)?).context("无法读取 Windows 更新包")?;
    let cli = location.executable.with_file_name(CLI_EXE);
    let backup = staging.path().join("backup");
    fs::create_dir(&backup)?;

    let mut files = Vec::with_capacity(2);
    for (name, installed) in [
        (CLI_EXE, cli.as_path()),
        (GUI_EXE, location.executable.as_path()),
    ] {
        let index = archive
            .index_for_name(&format!("Quick Bridge/{name}"))
            .or_else(|| archive.index_for_name(&format!(r"Quick Bridge\{name}")))
            .with_context(|| format!("Windows 更新包缺少 {name}"))?;
        let mut entry = archive.by_index(index)?;
        ensure!(entry.is_file(), "Windows 更新包中的 {name} 不是程序文件");
        if name == CLI_EXE && !installed.try_exists()? {
            continue;
        }
        ensure!(
            fs::symlink_metadata(installed)?.is_file(),
            "无法识别已安装的 {name}"
        );
        let update = staging.path().join(name);
        std::io::copy(&mut entry, &mut File::create_new(&update)?)
            .with_context(|| format!("无法解压 {name}"))?;
        let backup = backup.join(name);
        fs::copy(installed, &backup).with_context(|| format!("备份 {name} 失败"))?;
        files.push((update, installed, backup));
    }
    let mut replaced = 0;
    let result = (|| {
        // Replace the GUI last so a failed CLI replacement leaves its recovery entry available.
        for (update, installed, _) in &files {
            fs::rename(update, installed)
                .with_context(|| format!("替换更新失败 {}", installed.display()))?;
            replaced += 1;
        }
        after_install()
    })();
    if let Err(error) = result {
        let mut rollback_failure = None;
        for (_, installed, backup) in files[..replaced].iter().rev() {
            if let Err(rollback) = fs::rename(backup, installed) {
                rollback_failure = Some(rollback);
            }
        }
        if let Some(rollback) = rollback_failure {
            let backup = staging.keep();
            bail!(
                "{error:#}。恢复旧版失败：{rollback}。备份位于 {}",
                backup.display()
            );
        }
        return Err(error);
    }
    Ok(())
}
