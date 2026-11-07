//! Desktop client and tray for Lantern.

mod app;
mod fonts;
mod logs;
mod model;
mod platform;
mod prefs;

use lantern_cli::args::GuiArgs;

/// Run the desktop client, returning when the GUI process exits.
pub fn run(args: GuiArgs) -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let bus = match runtime.block_on(platform::single::acquire(
        &platform::single::bus_name(),
        ui_tx.clone(),
        &args,
    ))? {
        platform::single::Acquired::Secondary => return Ok(()),
        platform::single::Acquired::Primary(bus) => bus,
    };
    if let Some(target) = &args.target {
        let _ = ui_tx.send(platform::UiEvent::ShowTarget(target.clone()));
    }
    let (tray_tx, tray_rx) = tokio::sync::watch::channel(platform::tray::TraySnapshot::default());
    let mut tray_task = runtime.spawn(platform::tray::run(ui_tx, tray_rx));
    let fonts = fonts::Fonts::resolve();
    let client = lantern_client::Client::new(Default::default())?;
    let result = app::run(args, fonts, client, ui_rx, tray_tx);
    runtime.block_on(async {
        if tokio::time::timeout(std::time::Duration::from_secs(2), &mut tray_task)
            .await
            .is_err()
        {
            tray_task.abort();
        }
    });
    drop(bus);
    result?;
    Ok(())
}
