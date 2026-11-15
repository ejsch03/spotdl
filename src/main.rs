use std::process::ExitCode;

use clap::Parser;
use spotdl::{app, cli, ui};

fn main() -> ExitCode {
    spotdl::tune_allocator();
    let cli = cli::Cli::parse();
    ui::init_logging(cli.global.verbose);

    let rt = match spotdl::runtime() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{} starting the async runtime: {e}", ui::error_label());
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(async {
        tokio::select! {
            result = app::run(cli) => match result {
                Ok(code) => code,
                Err(e) => {
                    ui::ui().clear();
                    eprintln!("{} {e:#}", ui::error_label());
                    ExitCode::FAILURE
                }
            },
            // dropping the running future removes any half-written `.part` file
            _ = tokio::signal::ctrl_c() => {
                ui::ui().clear();
                eprintln!("\nInterrupted.");
                ExitCode::from(130)
            }
        }
    })
}
