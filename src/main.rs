mod discovery;
mod error;
mod escl;
mod ocr;
mod pdf;
mod state;
mod ui;
mod usb;

use std::time::Duration;

use tracing_subscriber::EnvFilter;

use crate::error::AppResult;

fn main() -> AppResult<()> {
    init_tracing();

    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--discover") | Some("-d") => {
            let timeout = Duration::from_secs(5);
            println!("Looking for scanners on the network for {timeout:?}…");
            let rt = tokio::runtime::Runtime::new()?;
            let found = rt.block_on(discovery::discover_once(timeout))?;
            if found.is_empty() {
                println!("No scanners found on this network.");
            } else {
                println!("mapped {} device(s)", found.len());
            }
            Ok(())
        }
        Some("--help") | Some("-h") => {
            eprintln!(
                "scanbro — scan from a Brother scanner\n\
                 \n\
                 Usage:\n\
                   scanbro              graphical dashboard\n\
                   scanbro --discover   list scanners found on the network\n"
            );
            Ok(())
        }
        _ => ui::run().map_err(|err| {
            crate::error::AppError::Escl(format!("GUI failed: {err}"))
        }),
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}
