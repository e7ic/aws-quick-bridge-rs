use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

const PROFILE_ID: &str = "6b839616-31d0-422e-bf51-2f6832a8493a";
const MODE: &str = "deploymentMode";
const APPLIED: &str = "appliedId";
const CONFIG: &str = "claude_desktop_config.json";
const RESTORE_FILE: &str = "claude-desktop-restore.json";

pub(in crate::gui) fn supported() -> bool {
    cfg!(any(target_os = "macos", windows))
}

pub(in crate::gui) fn has_restore_point() -> bool {
    crate::config::home_dir().join(RESTORE_FILE).is_file()
}

pub(in crate::gui) fn check_location() -> Result<()> {
    LinkFiles::for_user().map(|_| ())
}

pub(in crate::gui) fn apply(profile: &Value) -> Result<()> {
    LinkFiles::for_user()?.apply(profile)
}

pub(in crate::gui) fn restore() -> Result<()> {
    LinkFiles::for_user()?.restore()
}

#[cfg(target_os = "macos")]
pub(in crate::gui) fn is_running() -> Result<bool> {
    use objc2_app_kit::NSRunningApplication;
    use objc2_foundation::NSString;

    let bundle_id = NSString::from_str("com.anthropic.claudefordesktop");
    Ok(NSRunningApplication::runningApplicationsWithBundleIdentifier(&bundle_id).count() > 0)
}

#[cfg(windows)]
pub(in crate::gui) fn is_running() -> Result<bool> {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    struct Snapshot(HANDLE);
    impl Drop for Snapshot {
        fn drop(&mut self) {
            // SAFETY: The snapshot handle is valid and owned by this value.
            unsafe { CloseHandle(self.0) };
        }
    }
    // SAFETY: The process snapshot takes no pointers. The entry has the required size
    // and remains valid for the calls that fill it.
    unsafe {
        let handle = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().into());
        }
        let snapshot = Snapshot(handle);
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = Process32FirstW(snapshot.0, &mut entry);
        while found != 0 {
            let name = entry
                .szExeFile
                .split(|unit| *unit == 0)
                .next()
                .unwrap_or(&[]);
            if String::from_utf16_lossy(name).eq_ignore_ascii_case("Claude.exe") {
                return Ok(true);
            }
            found = Process32NextW(snapshot.0, &mut entry);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
            Ok(false)
        } else {
            Err(error.into())
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
pub(in crate::gui) fn is_running() -> Result<bool> {
    bail!("当前平台请使用导出配置")
}

struct LinkFiles {
    normal: PathBuf,
    third_party: PathBuf,
    meta: PathBuf,
    profile: PathBuf,
    restore: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct RestorePoint {
    normal_mode: Option<Value>,
    third_party_mode: Option<Value>,
    applied_id: Option<Value>,
}

impl LinkFiles {
    fn for_user() -> Result<Self> {
        ensure!(supported(), "当前平台请使用导出配置");
        let app_data = dirs::data_local_dir().context("无法定位 Claude Desktop 的数据目录")?;
        let files = Self::new(&app_data, &crate::config::home_dir());
        ensure!(
            files.normal.parent().is_some_and(Path::is_dir),
            "请先打开一次 Claude Desktop。也可在其第三方推理设置中导入配置"
        );
        for path in [&files.normal, &files.third_party, &files.meta] {
            let parent = path.parent().context("配置目录无效")?;
            match fs::symlink_metadata(parent) {
                Ok(metadata) => ensure!(metadata.is_dir(), "配置目录无法识别，请使用导出配置"),
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(files)
    }

    fn new(app_data: &Path, bridge_data: &Path) -> Self {
        let third_party = app_data.join("Claude-3p");
        let library = third_party.join("configLibrary");
        Self {
            normal: app_data.join("Claude").join(CONFIG),
            third_party: third_party.join(CONFIG),
            meta: library.join("_meta.json"),
            profile: library.join(format!("{PROFILE_ID}.json")),
            restore: bridge_data.join(RESTORE_FILE),
        }
    }

    fn lock(&self) -> Result<File> {
        let path = self.restore.with_extension("lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        lock.lock()?;
        Ok(lock)
    }

    fn apply(&self, profile: &Value) -> Result<()> {
        let _lock = self.lock()?;
        let mut normal = JsonFile::read(&self.normal)?;
        let mut third_party = JsonFile::read(&self.third_party)?;
        let mut meta = JsonFile::read(&self.meta)?;
        let mut owned = JsonFile::read(&self.profile)?;
        let mut restore = JsonFile::read(&self.restore)?;
        let already_selected = selects_bridge(&meta.value);
        let entries = entries_mut(&mut meta.value)?;
        let already_registered = entries.iter().any(is_bridge_entry);

        if restore.before.is_none() {
            ensure!(
                owned.before.is_none() && !already_registered && !already_selected,
                "检测到已有 Quick Bridge 配置，请在 Claude Desktop 中检查当前接入"
            );
        }
        if restore.before.is_none() || (!already_selected && already_registered) {
            let point = RestorePoint {
                normal_mode: normal.value.get(MODE).cloned(),
                third_party_mode: third_party.value.get(MODE).cloned(),
                applied_id: meta.value.get(APPLIED).cloned(),
            };
            restore.value = serde_json::from_value(serde_json::to_value(&point)?)?;
        }

        let entries = entries_mut(&mut meta.value)?;
        if !already_registered {
            entries.push(json!({ "id": PROFILE_ID, "name": "Quick Bridge" }));
        }
        meta.value.insert(APPLIED.into(), json!(PROFILE_ID));
        normal.value.insert(MODE.into(), json!("3p"));
        third_party.value.insert(MODE.into(), json!("3p"));
        owned
            .value
            .extend(profile.as_object().context("直连配置格式无效")?.clone());

        let changes = [
            owned.write_change()?,
            meta.write_change()?,
            third_party.write_change()?,
            normal.write_change()?,
        ];
        // Save the original selection before changing any Claude Desktop files.
        restore.write_change()?.commit()?;
        commit_changes(&changes)
    }

    fn restore(&self) -> Result<()> {
        let _lock = self.lock()?;
        let saved = JsonFile::read(&self.restore)?;
        let Some(before) = saved.before.as_deref() else {
            return Ok(());
        };
        let point: RestorePoint = serde_json::from_slice(before).context("原接入记录无法读取")?;
        let mut normal = JsonFile::read(&self.normal)?;
        let mut third_party = JsonFile::read(&self.third_party)?;
        let mut meta = JsonFile::read(&self.meta)?;
        let owned = JsonFile::read(&self.profile)?;

        let active = selects_bridge(&meta.value);
        entries_mut(&mut meta.value)?.retain(|entry| !is_bridge_entry(entry));
        if active {
            restore_field(&mut meta.value, APPLIED, point.applied_id);
            if normal.value.get(MODE).and_then(Value::as_str) == Some("3p") {
                restore_field(&mut normal.value, MODE, point.normal_mode);
            }
            if third_party.value.get(MODE).and_then(Value::as_str) == Some("3p") {
                restore_field(&mut third_party.value, MODE, point.third_party_mode);
            }
        }
        let changes = [
            third_party.write_change()?,
            normal.write_change()?,
            meta.write_change()?,
            owned.remove_change(),
            saved.remove_change(),
        ];
        commit_changes(&changes)
    }
}

fn selects_bridge(meta: &Map<String, Value>) -> bool {
    meta.get(APPLIED).and_then(Value::as_str) == Some(PROFILE_ID)
}

fn is_bridge_entry(entry: &Value) -> bool {
    entry.get("id").and_then(Value::as_str) == Some(PROFILE_ID)
}

fn entries_mut(meta: &mut Map<String, Value>) -> Result<&mut Vec<Value>> {
    meta.entry("entries")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .context("Claude Desktop 的配置列表格式无法识别，请使用导出配置")
}

fn restore_field(document: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        document.insert(key.to_owned(), value);
    } else {
        document.remove(key);
    }
}

struct JsonFile {
    path: PathBuf,
    before: Option<Vec<u8>>,
    value: Map<String, Value>,
}

impl JsonFile {
    fn read(path: &Path) -> Result<Self> {
        let before = read_regular_file(path)?;
        let value = before
            .as_deref()
            .map(serde_json::from_slice)
            .transpose()
            .with_context(|| format!("无法识别 {} 的配置格式，请使用导出配置", path.display()))?
            .unwrap_or_default();
        Ok(Self {
            path: path.into(),
            before,
            value,
        })
    }

    fn write_change(self) -> Result<Change> {
        let unchanged = self.before.as_deref().is_some_and(|bytes| {
            serde_json::from_slice::<Map<String, Value>>(bytes)
                .is_ok_and(|value| value == self.value)
        });
        let after = if unchanged {
            self.before.clone()
        } else {
            let mut bytes = serde_json::to_vec_pretty(&self.value)?;
            bytes.push(b'\n');
            Some(bytes)
        };
        Ok(Change {
            path: self.path,
            before: self.before,
            after,
        })
    }

    fn remove_change(self) -> Change {
        Change {
            path: self.path,
            before: self.before,
            after: None,
        }
    }
}

struct Change {
    path: PathBuf,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
}

impl Change {
    fn commit(&self) -> Result<()> {
        self.replace(self.before.as_deref(), self.after.as_deref())
    }

    fn replace(&self, expected: Option<&[u8]>, contents: Option<&[u8]>) -> Result<()> {
        if expected == contents {
            return Ok(());
        }
        ensure!(
            read_regular_file(&self.path)?.as_deref() == expected,
            "Claude Desktop 的配置已在其他地方更改，请重试"
        );
        if let Some(contents) = contents {
            let parent = self.path.parent().context("配置目录无效")?;
            fs::create_dir_all(parent)?;
            let mut file = tempfile::NamedTempFile::new_in(parent)?;
            file.write_all(contents)?;
            file.persist(&self.path)?;
        } else {
            fs::remove_file(&self.path)?;
        }
        Ok(())
    }
}

fn commit_changes(changes: &[Change]) -> Result<()> {
    for (index, change) in changes.iter().enumerate() {
        if let Err(error) = change.commit() {
            let mut rolled_back = true;
            for previous in changes[..index].iter().rev() {
                rolled_back &= previous
                    .replace(previous.after.as_deref(), previous.before.as_deref())
                    .is_ok();
            }
            if !rolled_back {
                bail!("{error}。可使用「恢复原接入」恢复配置");
            }
            return Err(error);
        }
    }
    Ok(())
}

fn read_regular_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(metadata.is_file(), "配置文件路径无效，请使用导出配置");
            Ok(Some(fs::read(path)?))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::claude_desktop::native_configuration;

    fn write(path: &Path, value: Value) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    }

    fn read(path: &Path) -> Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn updates_its_profile_without_touching_other_connections_and_restores_on_request() {
        let dir = tempfile::tempdir().unwrap();
        let files = LinkFiles::new(dir.path(), dir.path());
        let original_meta = json!({
            "entries": [{"id": "another-provider", "name": "My provider"}],
            "appliedId": "another-provider",
            "otherSetting": true,
        });
        let original_config = json!({ "deploymentMode": "1p", "mcpServers": {"example": {}} });
        let original_3p = json!({ "deploymentMode": "3p", "theme": "dark" });
        write(&files.normal, original_config.clone());
        write(&files.third_party, original_3p.clone());
        write(&files.meta, original_meta.clone());
        let other_profile = files.profile.with_file_name("another-provider.json");
        write(
            &other_profile,
            json!({ "inferenceGatewayApiKey": "other-credential" }),
        );
        let original_profile = fs::read(&other_profile).unwrap();

        let first =
            native_configuration("http://127.0.0.1:18765/claude-desktop", "test-key", "fast");
        files.apply(&first).unwrap();
        assert_eq!(read(&files.meta)[APPLIED], PROFILE_ID);
        assert_eq!(read(&files.normal)[MODE], "3p");
        assert_eq!(
            read(&files.normal)["mcpServers"],
            original_config["mcpServers"]
        );
        assert_eq!(read(&files.profile), first);
        assert!(
            !fs::read_to_string(&files.restore)
                .unwrap()
                .contains("test-key")
        );

        let next = native_configuration(
            "http://127.0.0.1:48765/claude-desktop",
            "updated-key",
            "balanced",
        );
        files.apply(&next).unwrap();
        assert_eq!(read(&files.profile), next);
        assert_eq!(read(&files.meta)["entries"].as_array().unwrap().len(), 2);
        assert_eq!(fs::read(&other_profile).unwrap(), original_profile);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                files.profile.metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        files.restore().unwrap();
        assert_eq!(read(&files.normal), original_config);
        assert_eq!(read(&files.third_party), original_3p);
        assert_eq!(read(&files.meta), original_meta);
        assert_eq!(fs::read(other_profile).unwrap(), original_profile);
        assert!(!files.profile.exists());
        assert!(!files.restore.exists());
    }

    #[test]
    fn a_concurrent_edit_reverts_already_written_files() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profile.json");
        let meta = dir.path().join("_meta.json");
        write(&meta, json!({ "appliedId": "new-external-selection" }));
        let changes = [
            Change {
                path: profile.clone(),
                before: None,
                after: Some(b"{}".to_vec()),
            },
            Change {
                path: meta.clone(),
                before: Some(b"{}".to_vec()),
                after: Some(b"{\"other\":true}".to_vec()),
            },
        ];
        assert!(commit_changes(&changes).is_err());
        assert!(!profile.exists());
        assert_eq!(read(&meta)[APPLIED], "new-external-selection");
    }
}
