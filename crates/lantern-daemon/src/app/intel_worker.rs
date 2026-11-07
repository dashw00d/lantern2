//! Event and manifest-driven scheduling for the shared intelligence service.

use crate::{api::facade::DaemonFacade, app::facade::AppFacade, events::EventBus};
use lantern_core::{
    api::Event,
    ids::{InstanceId, ProjectId},
    model::InstanceStatus,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::{task::JoinSet, time::Instant};
use tokio_util::sync::CancellationToken;

type Stamp = Option<(SystemTime, u64)>;
fn stamps(facade: &AppFacade) -> BTreeMap<PathBuf, (ProjectId, Stamp)> {
    let mut out = BTreeMap::new();
    for view in facade.registry.instances() {
        for name in ["lantern.yaml", "lantern.yml"] {
            let path = view.checkout.path.join(name);
            let stamp = std::fs::metadata(&path)
                .ok()
                .and_then(|m| m.modified().ok().map(|t| (t, m.len())));
            out.insert(path, (view.project.clone(), stamp));
        }
    }
    out
}
fn schedule(tasks: &mut JoinSet<()>, facade: Arc<AppFacade>, id: InstanceId, delay: Duration) {
    tasks.spawn(async move {
        tokio::time::sleep(delay).await;
        if let Err(error) = facade.refresh_intel(id.clone()).await {
            tracing::debug!(%id, message=%error.message, "intel refresh failed");
        }
    });
}
fn periodic(tasks: &mut JoinSet<()>, facade: &Arc<AppFacade>) {
    let enabled: BTreeSet<_> = facade
        .registry
        .projects()
        .into_iter()
        .filter(|p| p.project.enabled)
        .map(|p| p.primary)
        .collect();
    let mut count = 0u64;
    for instance in facade.registry.instances() {
        if enabled.contains(&instance.id)
            || instance.pinned
            || matches!(
                instance.runtime.status,
                InstanceStatus::Running | InstanceStatus::Unhealthy | InstanceStatus::External
            )
        {
            schedule(
                tasks,
                facade.clone(),
                instance.id,
                Duration::from_millis(count * 50),
            );
            count += 1;
        }
    }
}
/// Start the daemon-owned intel refresh worker.
pub fn spawn(
    facade: Arc<AppFacade>,
    bus: &EventBus,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let mut events = bus.subscribe();
    tokio::spawn(async move {
        let mut tasks = JoinSet::new();
        let mut known: HashMap<_, _> = facade
            .registry
            .instances()
            .into_iter()
            .map(|v| (v.id, (v.runtime.status, v.manifest.manifest_rev)))
            .collect();
        let mut files = stamps(&facade);
        let mut watch = tokio::time::interval(Duration::from_secs(2));
        let interval =
            Duration::from_secs(u64::from(facade.settings.intel.refresh_interval_minutes.max(1)) * 60);
        let mut refresh_at = Instant::now() + Duration::from_secs(2);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
                () = tokio::time::sleep_until(refresh_at) => { periodic(&mut tasks, &facade); refresh_at = Instant::now() + interval; },
                _ = watch.tick() => {
                    let next = stamps(&facade);
                    let changed: BTreeSet<_> = next.iter().filter_map(|(path, (id, stamp))| files.get(path).filter(|(_, old)| old != stamp).map(|_| id.clone())).collect();
                    files = next;
                    for project in changed {
                        match facade.registry.reload_project(&project) {
                            Ok(()) => for view in facade.registry.instances().into_iter().filter(|v| v.project == project) { facade.runtime.on_manifest_changed(&view.id); },
                            Err(error) => tracing::warn!(%error, %project, "manifest reload failed"),
                        }
                    }
                },
                event = events.recv() => match event {
                    Ok(Event::InstanceUpdated { instance }) => {
                        let id = instance.id;
                        let status = instance.runtime.status;
                        let rev = instance.manifest.manifest_rev;
                        let old = known.insert(id.clone(), (status, rev.clone()));
                        if old.as_ref().is_none_or(|(_, previous)| previous != &rev) { schedule(&mut tasks, facade.clone(), id.clone(), Duration::ZERO); }
                        if status == InstanceStatus::Running && old.as_ref().is_none_or(|(previous, _)| *previous != InstanceStatus::Running) {
                            schedule(&mut tasks, facade.clone(), id.clone(), Duration::from_secs(1));
                            schedule(&mut tasks, facade.clone(), id, Duration::from_secs(10));
                        }
                    },
                    Ok(Event::ProjectUpdated { project }) => {
                        for view in facade.registry.instances().into_iter().filter(|v| v.project == project.project.id) {
                            schedule(&mut tasks, facade.clone(), view.id, Duration::ZERO);
                        }
                    },
                    Ok(Event::InstanceRemoved { id }) => { known.remove(&id); if let Some(intel) = &facade.intel_service { intel.forget(&id).await; } },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => periodic(&mut tasks, &facade),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    _ => {}
                }
            }
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    })
}
