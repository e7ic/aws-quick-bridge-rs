//! 独立桌面 GUI 启动入口（Windows 下自动隐藏控制台黑框）。
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

fn main() {
    #[cfg(feature = "gui")]
    {
        if let Err(e) = aws_quick_bridge_rs::gui::run_gui() {
            eprintln!("aws-quick-bridge-gui error: {e}");
            std::process::exit(1);
        }
    }
    #[cfg(not(feature = "gui"))]
    {
        eprintln!("Rebuild with `--features gui` to launch the desktop app.");
        std::process::exit(1);
    }
}
