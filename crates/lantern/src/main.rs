//! The single Lantern binary dispatches to CLI, daemon or GUI.

mod system;

fn main() {
    let code = match lantern_cli::run_with_system(&system::System) {
        lantern_cli::CliAction::Exit(code) => code,
        lantern_cli::CliAction::RunDaemon(args) => {
            match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(runtime) => {
                    match runtime.block_on(lantern_daemon::app::run(lantern_daemon::app::DaemonArgs {
                        dev: args.dev,
                        ..Default::default()
                    })) {
                        Ok(()) => 0,
                        Err(error) => {
                            eprintln!("daemon: {error}");
                            1
                        }
                    }
                }
                Err(error) => {
                    eprintln!("daemon runtime: {error}");
                    1
                }
            }
        }
        lantern_cli::CliAction::RunGui(_) => {
            eprintln!("GUI is not assembled yet");
            2
        }
    };
    std::process::exit(code);
}
