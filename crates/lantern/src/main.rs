//! The single Lantern binary dispatches to CLI, daemon or GUI.

fn main() {
    let code = match lantern_cli::run() {
        lantern_cli::CliAction::Exit(code) => code,
        // The controller replaces these placeholders when assembling the daemon and GUI.
        lantern_cli::CliAction::RunDaemon(_) | lantern_cli::CliAction::RunGui(_) => {
            eprintln!("daemon not assembled yet");
            2
        }
    };
    std::process::exit(code);
}
