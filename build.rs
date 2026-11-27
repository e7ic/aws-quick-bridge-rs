fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-env-changed=QUICK_BRIDGE_BUNDLE_FONTS");
    println!("cargo:rustc-check-cfg=cfg(macos_bundle_fonts)");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && std::env::var("QUICK_BRIDGE_BUNDLE_FONTS").as_deref() == Ok("1")
    {
        println!("cargo:rustc-cfg=macos_bundle_fonts");
    }

    #[cfg(feature = "gui")]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/app-icon.rc");
        println!("cargo:rerun-if-changed=assets/app-icon.ico");
        println!("cargo:rerun-if-changed=assets/tray-on-light.ico");
        println!("cargo:rerun-if-changed=assets/tray-on-dark.ico");
        embed_resource::compile_for(
            "assets/app-icon.rc",
            ["aws-quick-bridge-gui", "aws-quick-bridge-rs"],
            embed_resource::NONE,
        )
        .manifest_required()?;
    }
    Ok(())
}
