//! Desktop client and tray for Lantern.

mod app;
mod fonts;
mod logs;
mod model;

use lantern_cli::args::GuiArgs;

/// Run the desktop client, returning when the GUI process exits.
pub fn run(args: GuiArgs) -> Result<(), Box<dyn std::error::Error>> {
    let fonts = fonts::Fonts::resolve();
    let client = lantern_client::Client::new(Default::default())?;
    app::run(args, fonts, client)?;
    Ok(())
}
