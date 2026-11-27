use super::*;

#[cfg(target_os = "macos")]
#[test]
fn macos_install_replaces_the_signed_bundle_and_recovers_if_launch_fails() -> Result<()> {
    fn bundle(parent: &Path, version: &str) -> Result<PathBuf> {
        let bundle = parent.join("Quick Bridge.app");
        let contents = bundle.join("Contents");
        fs::create_dir_all(contents.join("MacOS"))?;
        fs::copy("/usr/bin/true", contents.join("MacOS/QuickBridge"))?;
        fs::write(
            contents.join("Info.plist"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
    <key>CFBundleIdentifier</key><string>org.quickbridge.desktop</string>
    <key>CFBundleExecutable</key><string>QuickBridge</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>{version}</string>
</dict></plist>"#
            ),
        )?;
        let signed = background_command("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(&bundle)
            .status()?;
        ensure!(signed.success(), "could not sign fixture bundle");
        Ok(bundle)
    }

    let installed = tempfile::tempdir()?;
    let incoming = tempfile::tempdir()?;
    let original = bundle(installed.path(), "0.1.5")?;
    let replacement = bundle(incoming.path(), "0.1.6")?;
    let archive = incoming.path().join("package.zip");
    let zipped = background_command("/usr/bin/ditto")
        .args(["-c", "-k", "--sequesterRsrc", "--keepParent"])
        .arg(replacement)
        .arg(&archive)
        .status()?;
    ensure!(zipped.success(), "could not archive fixture bundle");
    let location = InstallLocation::from_executable(original.join("Contents/MacOS/QuickBridge"))?;
    let version = Version::new(0, 1, 6);
    let plist = original.join("Contents/Info.plist");
    let result = apply_package(&location, &archive, &version, || {
        assert!(fs::read_to_string(&plist)?.contains("0.1.6"));
        bail!("new app could not launch");
    });
    assert_eq!(result.unwrap_err().to_string(), "new app could not launch");
    assert!(fs::read_to_string(&plist)?.contains("0.1.5"));
    apply_package(&location, &archive, &version, || Ok(()))?;
    assert!(fs::read_to_string(plist)?.contains("0.1.6"));
    Ok(())
}

#[cfg(target_os = "windows")]
#[test]
fn windows_install_recovers_both_binaries_after_a_failed_restart() -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt as _;

    let installed = tempfile::tempdir()?;
    let incoming = tempfile::tempdir()?;
    let package = incoming.path().join("Quick Bridge");
    fs::create_dir(&package)?;
    for name in [GUI_EXE, CLI_EXE] {
        fs::write(installed.path().join(name), b"old")?;
        fs::write(package.join(name), b"new")?;
    }
    fs::write(installed.path().join("user-file.txt"), b"user data")?;
    let archive = incoming.path().join("package.zip");
    let system_root = PathBuf::from(std::env::var_os("SystemRoot").context("locate Windows")?);
    let powershell = system_root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let zipped = background_command(powershell)
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
        .arg("$ErrorActionPreference = 'Stop'; Compress-Archive -Path $env:QUICK_BRIDGE_UPDATE_SOURCE -DestinationPath $env:QUICK_BRIDGE_UPDATE_ARCHIVE -CompressionLevel Optimal")
        .env("QUICK_BRIDGE_UPDATE_SOURCE", package)
        .env("QUICK_BRIDGE_UPDATE_ARCHIVE", &archive)
        .status()?;
    ensure!(zipped.success(), "could not archive Windows fixture");
    let location = InstallLocation::from_executable(installed.path().join(GUI_EXE))?;
    let version = Version::new(0, 1, 6);
    // Other processes can still hold the executable open after the updater got its lease.
    let locked = OpenOptions::new()
        .read(true)
        .share_mode(1) // FILE_SHARE_READ
        .open(installed.path().join(GUI_EXE))?;
    let blocked = apply_package(&location, &archive, &version, || Ok(())).unwrap_err();
    assert!(
        blocked.to_string().starts_with("替换更新失败"),
        "{blocked:#}"
    );
    assert_eq!(fs::read(installed.path().join(CLI_EXE))?, b"old");
    drop(locked);

    let result = apply_package(&location, &archive, &version, || {
        assert_eq!(fs::read(installed.path().join(GUI_EXE))?, b"new");
        assert_eq!(fs::read(installed.path().join(CLI_EXE))?, b"new");
        bail!("new app could not launch");
    });
    assert_eq!(result.unwrap_err().to_string(), "new app could not launch");
    assert_eq!(fs::read(installed.path().join(GUI_EXE))?, b"old");
    assert_eq!(fs::read(installed.path().join(CLI_EXE))?, b"old");
    apply_package(&location, &archive, &version, || Ok(()))?;
    assert_eq!(fs::read(installed.path().join(GUI_EXE))?, b"new");
    assert_eq!(fs::read(installed.path().join(CLI_EXE))?, b"new");
    assert_eq!(
        fs::read(installed.path().join("user-file.txt"))?,
        b"user data"
    );
    Ok(())
}
