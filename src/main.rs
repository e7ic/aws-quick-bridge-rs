use aws_quick_bridge_rs::cli::{Cli, Command, run_cli};
use clap::Parser;

fn main() {
    let cli = Cli::parse();
    match cli.command {
        None | Some(Command::Gui) => {
            #[cfg(feature = "gui")]
            {
                if let Err(e) = aws_quick_bridge_rs::gui::run_gui() {
                    eprintln!("aws-quick-bridge-rs gui error: {e}");
                    std::process::exit(1);
                }
            }
            #[cfg(not(feature = "gui"))]
            {
                eprintln!("Built without `gui` feature. Run `aws-quick-bridge-rs serve` or rebuild with `--features gui`.");
                std::process::exit(1);
            }
        }
        Some(_) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime");
            if let Err(e) = rt.block_on(run_cli(cli)) {
                eprintln!("aws-quick-bridge-rs: {e}");
                std::process::exit(1);
            }
        }
    }
}
