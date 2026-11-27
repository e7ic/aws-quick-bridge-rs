use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ring::signature::{ED25519, UnparsedPublicKey};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;

const RELEASE_URL: &str = "https://github.com/elenchus-ai/aws-quick-bridge-rs/releases";
const MANIFEST_NAME: &str = "quick-bridge-update.json";
const MANIFEST_LIMIT: usize = 16 * 1024;
const CACHE_PREFIX: &str = "pending-";
pub(super) const ARCHIVE: &str = "package.zip";
pub(super) const MANIFEST: &str = "manifest.json";
pub(super) const LEASE: &str = "install.lock";
pub(super) const FINISHED: &str = "finished";

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum Platform {
    MacosUniversal,
    WindowsX64,
}

impl Platform {
    fn current() -> Self {
        #[cfg(target_os = "macos")]
        return Self::MacosUniversal;
        #[cfg(target_os = "windows")]
        return Self::WindowsX64;
    }

    fn suffix(self) -> &'static str {
        match self {
            Self::MacosUniversal => "macos-universal",
            Self::WindowsX64 => "windows-x64",
        }
    }
}

#[derive(Deserialize)]
struct Envelope {
    payload: String,
    signature: String,
}

#[derive(Deserialize)]
struct Manifest {
    version: String,
    packages: Vec<Artifact>,
}

#[derive(Deserialize)]
struct Artifact {
    platform: Platform,
    size: u64,
    sha256: String,
}

struct VerifiedUpdate {
    version: Version,
    asset_name: String,
    size: u64,
    digest: [u8; 32],
}

pub(super) struct PreparedUpdate {
    pub(super) directory: TempDir,
    pub(super) lease: File,
    pub(super) version: Version,
}

pub(super) fn public_key() -> Option<[u8; 32]> {
    STANDARD
        .decode(option_env!("QUICK_BRIDGE_UPDATE_PUBLIC_KEY")?.trim())
        .ok()?
        .try_into()
        .ok()
}

pub(super) fn cache_dir() -> PathBuf {
    crate::config::home_dir().join("updates")
}

pub(super) fn is_job_path(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with(CACHE_PREFIX))
        && path
            .parent()
            .and_then(|parent| parent.canonicalize().ok())
            .is_some_and(|parent| cache_dir().canonicalize().ok().as_ref() == Some(&parent))
        && fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

pub(super) fn purge_finished_downloads() {
    let Ok(entries) = fs::read_dir(cache_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_job_path(&path) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| {
                SystemTime::now()
                    .duration_since(modified)
                    .is_ok_and(|age| age >= Duration::from_secs(24 * 3600))
            });
        if !stale && !path.join(FINISHED).is_file() {
            continue;
        }
        let Ok(lease) = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.join(LEASE))
        else {
            continue;
        };
        if lease.try_lock().is_ok() {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

fn verify_manifest(
    bytes: &[u8],
    key: &[u8; 32],
    platform: Platform,
    current: &Version,
) -> Result<Option<VerifiedUpdate>> {
    ensure!(bytes.len() <= MANIFEST_LIMIT, "更新清单大小超出限制");
    let envelope: Envelope = serde_json::from_slice(bytes).context("更新清单格式无效")?;
    let payload = STANDARD.decode(envelope.payload)?;
    let signature = STANDARD.decode(envelope.signature)?;
    UnparsedPublicKey::new(&ED25519, key)
        .verify(&payload, &signature)
        .map_err(|_| anyhow::anyhow!("更新清单签名无效"))?;
    let manifest: Manifest = serde_json::from_slice(&payload).context("签名清单内容无效")?;
    let version = Version::parse(&manifest.version).context("更新版本号无效")?;
    ensure!(
        version.pre.is_empty() && version.build.is_empty(),
        "更新清单不是正式版本"
    );
    if version <= *current {
        return Ok(None);
    }
    let mut matches = manifest
        .packages
        .into_iter()
        .filter(|asset| asset.platform == platform);
    let asset = matches.next().context("更新清单缺少当前平台的安装包")?;
    ensure!(
        matches.next().is_none() && asset.size > 0,
        "更新清单中的安装包信息无效"
    );
    let mut digest = [0; 32];
    hex::decode_to_slice(asset.sha256, &mut digest).context("更新包 SHA-256 无效")?;
    Ok(Some(VerifiedUpdate {
        asset_name: format!("Quick-Bridge-{version}-{}.zip", platform.suffix()),
        version,
        size: asset.size,
        digest,
    }))
}

fn current_version() -> Result<Version> {
    Version::parse(env!("CARGO_PKG_VERSION")).context("当前版本号无效")
}

pub(super) async fn download_latest(key: [u8; 32]) -> Result<Option<PreparedUpdate>> {
    let client = reqwest::Client::builder()
        .https_only(true)
        .user_agent(concat!("Quick-Bridge/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(240))
        .build()?;
    let mut response = client
        .get(format!("{RELEASE_URL}/latest/download/{MANIFEST_NAME}"))
        .send()
        .await
        .context("无法连接 GitHub Releases")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    response = response.error_for_status()?;
    let mut manifest = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            chunk.len() <= MANIFEST_LIMIT - manifest.len(),
            "更新清单大小超出限制"
        );
        manifest.extend_from_slice(&chunk);
    }
    let Some(update) = verify_manifest(&manifest, &key, Platform::current(), &current_version()?)?
    else {
        return Ok(None);
    };

    let root = std::path::absolute(cache_dir())?;
    fs::create_dir_all(&root)?;
    let directory = tempfile::Builder::new()
        .prefix(CACHE_PREFIX)
        .tempdir_in(root)?;
    let lease = File::create_new(directory.path().join(LEASE))?;
    lease.lock()?;
    let mut response = client
        .get(format!(
            "{RELEASE_URL}/download/v{}/{}",
            update.version, update.asset_name
        ))
        .send()
        .await
        .context("下载更新包失败")?
        .error_for_status()?;
    ensure!(
        response
            .content_length()
            .is_none_or(|size| size == update.size),
        "更新包大小与签名清单不符"
    );
    let mut archive = tokio::fs::File::create(directory.path().join(ARCHIVE)).await?;
    let mut digest = Sha256::new();
    let mut received = 0;
    while let Some(chunk) = response.chunk().await? {
        let length = chunk.len() as u64;
        ensure!(length <= update.size - received, "更新包大小与签名清单不符");
        archive.write_all(&chunk).await?;
        digest.update(&chunk);
        received += length;
    }
    ensure!(
        received == update.size && <[u8; 32]>::from(digest.finalize()) == update.digest,
        "更新包 SHA-256 校验失败"
    );
    archive.sync_all().await?;
    tokio::fs::write(directory.path().join(MANIFEST), manifest).await?;
    Ok(Some(PreparedUpdate {
        directory,
        lease,
        version: update.version,
    }))
}

pub(super) fn verify_download(directory: &Path, key: &[u8; 32]) -> Result<Version> {
    let mut manifest = Vec::new();
    File::open(directory.join(MANIFEST))?
        .take(MANIFEST_LIMIT as u64 + 1)
        .read_to_end(&mut manifest)?;
    let update = verify_manifest(&manifest, key, Platform::current(), &current_version()?)?
        .context("更新版本应高于当前版本")?;
    let archive = directory.join(ARCHIVE);
    ensure!(
        fs::metadata(&archive)?.len() == update.size,
        "更新包大小与签名清单不符"
    );
    ensure!(
        file_digest(&archive)? == update.digest,
        "更新包 SHA-256 校验失败"
    );
    Ok(update.version)
}

pub(super) fn file_digest(path: &Path) -> Result<[u8; 32]> {
    let mut file = File::open(path).with_context(|| format!("无法读取 {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let length = file.read(&mut buffer)?;
        if length == 0 {
            return Ok(digest.finalize().into());
        }
        digest.update(&buffer[..length]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};
    use serde_json::json;

    fn envelope(payload: &[u8], signature: &[u8]) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "payload": STANDARD.encode(payload),
            "signature": STANDARD.encode(signature)
        }))
        .unwrap()
    }

    #[test]
    fn signed_manifest_selects_the_platform_and_rejects_tampering_and_downgrades() {
        let signer = Ed25519KeyPair::from_seed_unchecked(&[37; 32]).unwrap();
        let key = signer.public_key().as_ref().try_into().unwrap();
        let payload = serde_json::to_vec(&json!({
            "version": "0.1.6",
            "packages": [
                { "platform": "macos-universal", "size": 5, "sha256": "12".repeat(32) },
                { "platform": "windows-x64", "size": 7, "sha256": "34".repeat(32) }
            ]
        }))
        .unwrap();
        let signature = signer.sign(&payload);
        let valid = envelope(&payload, signature.as_ref());
        let selected = verify_manifest(&valid, &key, Platform::WindowsX64, &Version::new(0, 1, 5))
            .unwrap()
            .unwrap();
        assert_eq!(selected.asset_name, "Quick-Bridge-0.1.6-windows-x64.zip");
        assert_eq!(selected.size, 7);
        assert_eq!(selected.digest, [0x34; 32]);
        for current in [Version::new(0, 1, 6), Version::new(0, 1, 7)] {
            assert!(
                verify_manifest(&valid, &key, Platform::MacosUniversal, &current)
                    .unwrap()
                    .is_none()
            );
        }
        let mut tampered = payload;
        tampered.push(b' ');
        let forged = envelope(&tampered, signature.as_ref());
        assert!(
            verify_manifest(
                &forged,
                &key,
                Platform::MacosUniversal,
                &Version::new(0, 1, 5)
            )
            .is_err()
        );
    }

    #[test]
    fn installer_rechecks_the_archive_even_if_its_size_is_unchanged() -> Result<()> {
        let signer = Ed25519KeyPair::from_seed_unchecked(&[48; 32]).unwrap();
        let key = signer.public_key().as_ref().try_into().unwrap();
        let payload = serde_json::to_vec(&json!({
            "version": "99999.0.0",
            "packages": [{
                "platform": Platform::current().suffix(),
                "size": 3,
                "sha256": hex::encode(Sha256::digest(b"zip"))
            }]
        }))?;
        let directory = tempfile::tempdir()?;
        let archive = directory.path().join(ARCHIVE);
        fs::write(
            directory.path().join(MANIFEST),
            envelope(&payload, signer.sign(&payload).as_ref()),
        )?;
        fs::write(&archive, b"zip")?;
        assert_eq!(
            verify_download(directory.path(), &key)?,
            Version::new(99999, 0, 0)
        );
        fs::write(archive, b"bad")?;
        assert!(verify_download(directory.path(), &key).is_err());
        Ok(())
    }
}
