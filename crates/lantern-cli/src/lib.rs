//! Command-line interface and binary dispatch boundary.

pub mod args;
mod error;
mod execute;
mod output;
mod target;

use args::GuiArgs;
pub use execute::dispatch;

/// Arguments forwarded to the daemon implementation by the binary.
#[derive(Debug, Clone, Default)]
pub struct DaemonArgs {}

/// Action returned to the single-binary entry point.
#[derive(Debug)]
pub enum CliAction {
    /// Run the daemon in this process.
    RunDaemon(DaemonArgs),
    /// Run or activate the GUI.
    RunGui(GuiArgs),
    /// Exit after a CLI command.
    Exit(i32),
}

/// Commands delegated to the system-integration crate at assembly time.
#[derive(Debug, Clone)]
pub enum SystemCommand {
    /// Initialize the machine.
    Init { yes: bool },
    /// Run diagnostics.
    Doctor { fix: bool },
    /// Remove system integration.
    Uninstall { purge_data: bool, yes: bool },
}

/// Consumer-defined boundary for offline system commands.
pub trait SystemActions {
    /// Execute an offline system command and return its JSON result.
    fn execute(&self, command: &SystemCommand) -> Result<serde_json::Value, String>;
}

/// Placeholder until the binary wires `lantern-system`.
#[derive(Debug, Default)]
pub struct UnassembledSystem;
impl SystemActions for UnassembledSystem {
    fn execute(&self, _command: &SystemCommand) -> Result<serde_json::Value, String> {
        Err("system integration not assembled yet".into())
    }
}

/// Parse and run the CLI, returning the process action to the binary.
pub fn run() -> CliAction {
    use clap::Parser;
    use std::io::IsTerminal;
    let argv: Vec<_> = std::env::args_os().collect();
    let cli = match args::Cli::try_parse_from(&argv) {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code();
            if code != 0 && argv.iter().any(|arg| arg == "--json") {
                let failure = error::Failure::usage(error.to_string());
                eprintln!("{}", failure.json());
            } else {
                let _ = error.print();
            }
            return CliAction::Exit(code);
        }
    };
    let tty = std::io::stdout().is_terminal();
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: cannot start async runtime: {error}");
            return CliAction::Exit(1);
        }
    };
    runtime.block_on(dispatch(
        cli,
        &UnassembledSystem,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
        tty,
    ))
}
