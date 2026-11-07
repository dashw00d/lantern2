//! A staged, event-driven copy of daemon state.

use lantern_client::StreamEvent;
use lantern_core::{
    api::{InstanceView, OperationId, ProjectSummary, ServiceStatus, Suggestion, SystemHealth, WireEvent},
    ids::{InstanceId, ProjectId},
    model::{InstanceStatus, Listener},
};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default)]
pub(crate) struct Snapshot {
    pub projects: BTreeMap<ProjectId, ProjectSummary>,
    pub instances: BTreeMap<InstanceId, InstanceView>,
    pub listeners: Vec<Listener>,
    pub services: Vec<ServiceStatus>,
    pub suggestions: Vec<Suggestion>,
    pub health: Option<SystemHealth>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum Connection {
    #[default]
    Connecting,
    Live,
    Down {
        error: String,
        retry_in: std::time::Duration,
    },
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Mirror {
    pub live: Snapshot,
    pub connection: Connection,
    pub ever_live: bool,
    pub boot_id: Option<String>,
    staging: Option<Snapshot>,
    staging_boot_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum Effect {
    Restarted,
    Resynced,
    Transition {
        id: InstanceId,
        status: InstanceStatus,
    },
    Progress {
        op: OperationId,
        instance: InstanceId,
        step: String,
        done: bool,
        error: Option<String>,
    },
    Removed(InstanceId),
}

impl Mirror {
    pub fn stream(&mut self, event: StreamEvent) -> Vec<Effect> {
        match event {
            StreamEvent::Connecting { .. } => {
                self.staging = None;
                self.staging_boot_id = None;
                if !matches!(self.connection, Connection::Down { .. }) {
                    self.connection = Connection::Connecting;
                }
                Vec::new()
            }
            StreamEvent::Connected => {
                self.connection = Connection::Connecting;
                Vec::new()
            }
            StreamEvent::Lost { error, retry_in, .. } => {
                self.staging = None;
                self.staging_boot_id = None;
                self.connection = Connection::Down { error, retry_in };
                Vec::new()
            }
            StreamEvent::Event(event) => self.reduce(event),
        }
    }

    fn target(&mut self) -> &mut Snapshot {
        self.staging.as_mut().unwrap_or(&mut self.live)
    }

    pub fn reduce(&mut self, event: WireEvent) -> Vec<Effect> {
        match event {
            WireEvent::Hello { boot_id, .. } => {
                self.staging = Some(Snapshot::default());
                let restarted = self.boot_id.as_ref().is_some_and(|old| old != &boot_id);
                self.staging_boot_id = Some(boot_id);
                if restarted {
                    vec![Effect::Restarted]
                } else {
                    Vec::new()
                }
            }
            WireEvent::SnapshotComplete => {
                if let Some(snapshot) = self.staging.take() {
                    self.live = snapshot;
                    self.boot_id = self.staging_boot_id.take();
                    self.connection = Connection::Live;
                    self.ever_live = true;
                    vec![Effect::Resynced]
                } else {
                    Vec::new()
                }
            }
            WireEvent::ProjectUpdated { project } => {
                self.target().projects.insert(project.project.id.clone(), project);
                Vec::new()
            }
            WireEvent::ProjectRemoved { id } => {
                self.target().projects.remove(&id);
                Vec::new()
            }
            WireEvent::InstanceUpdated { instance } => {
                let id = instance.id.clone();
                let status = instance.runtime.status;
                let is_live = self.staging.is_none();
                self.target().instances.insert(id.clone(), instance);
                if is_live {
                    vec![Effect::Transition { id, status }]
                } else {
                    Vec::new()
                }
            }
            WireEvent::InstanceRemoved { id } => {
                let is_live = self.staging.is_none();
                self.target().instances.remove(&id);
                if is_live {
                    vec![Effect::Removed(id)]
                } else {
                    Vec::new()
                }
            }
            WireEvent::ListenersChanged { listeners, .. } => {
                self.target().listeners = listeners;
                Vec::new()
            }
            WireEvent::SuggestionsChanged { suggestions } => {
                self.target().suggestions = suggestions;
                Vec::new()
            }
            WireEvent::ServiceUpdated { service } => {
                let services = &mut self.target().services;
                if let Some(old) = services.iter_mut().find(|s| s.name == service.name) {
                    *old = service;
                } else {
                    services.push(service);
                }
                Vec::new()
            }
            WireEvent::SystemHealth { health } => {
                self.target().health = Some(health);
                Vec::new()
            }
            WireEvent::OperationProgress {
                op,
                instance,
                step,
                done,
                error,
            } => vec![Effect::Progress {
                op,
                instance,
                step,
                done,
                error,
            }],
            WireEvent::Resync { .. } => {
                self.staging = None;
                self.connection = Connection::Connecting;
                Vec::new()
            }
            WireEvent::Log { .. } => Vec::new(),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use lantern_core::{
        api::{DiagnosticCounts, ManifestSummary},
        model::{Checkout, CheckoutKind, Project, ProjectKind, ProjectSource, RuntimeState, ServeMode},
    };
    use std::time::Duration;

    fn id(value: &str) -> InstanceId {
        InstanceId::parse(value).unwrap()
    }
    fn project_id() -> ProjectId {
        ProjectId::parse("example").unwrap()
    }
    pub(crate) fn instance(status: InstanceStatus) -> InstanceView {
        let project = project_id();
        InstanceView {
            id: id("example"),
            project: project.clone(),
            checkout: Checkout {
                path: "/tmp/example".into(),
                git_root: None,
                kind: CheckoutKind::Primary,
                branch: Some("main".into()),
                head: None,
                dirty: None,
                locked: None,
                missing: false,
            },
            domain: "example.test".into(),
            url: "https://example.test".into(),
            pinned: true,
            adopt: false,
            visible: true,
            setup_stale: false,
            runtime: RuntimeState {
                status,
                ..RuntimeState::default()
            },
            manifest: ManifestSummary {
                id: project,
                serve: ServeMode::Proxy,
                run_cmd: Some("npm start".into()),
                diagnostics: DiagnosticCounts {
                    errors: 0,
                    warnings: 0,
                    info: 0,
                },
                manifest_rev: "abc".into(),
            },
        }
    }
    pub(crate) fn project() -> ProjectSummary {
        ProjectSummary {
            project: Project {
                id: project_id(),
                aliases: vec![],
                name: "Example".into(),
                root: "/tmp/example".into(),
                repo: None,
                kind: ProjectKind::Project,
                enabled: true,
                tags: vec![],
                registered_at: "2026-11-06T00:00:00Z".parse().unwrap(),
                source: ProjectSource::Manual,
            },
            primary: id("example"),
            status: InstanceStatus::Stopped,
            url: "https://example.test".into(),
            port: None,
            instance_count: 1,
            docs_count: 0,
            endpoints_count: 0,
            routing: None,
        }
    }
    pub(crate) fn hello(boot: &str) -> WireEvent {
        WireEvent::Hello {
            api_version: 1,
            daemon_version: "2".into(),
            boot_id: boot.into(),
            now: "2026-11-06T00:00:00Z".parse().unwrap(),
        }
    }
    fn recorded(m: &mut Mirror, event: WireEvent) -> Vec<Effect> {
        // Round-trip each event through the actual wire representation.
        let json = serde_json::to_string(&event).unwrap();
        m.stream(StreamEvent::Event(serde_json::from_str(&json).unwrap()))
    }
    #[test]
    fn snapshot_replays_atomically_then_live_transitions() {
        let mut m = Mirror::default();
        recorded(&mut m, hello("one"));
        recorded(&mut m, WireEvent::ProjectUpdated { project: project() });
        recorded(
            &mut m,
            WireEvent::InstanceUpdated {
                instance: instance(InstanceStatus::Stopped),
            },
        );
        assert!(m.live.projects.is_empty());
        assert!(matches!(
            recorded(&mut m, WireEvent::SnapshotComplete).as_slice(),
            [Effect::Resynced]
        ));
        assert_eq!(m.live.projects.len(), 1);
        assert_eq!(m.connection, Connection::Live);
        let effects = recorded(
            &mut m,
            WireEvent::InstanceUpdated {
                instance: instance(InstanceStatus::Running),
            },
        );
        assert!(matches!(
            &effects[..],
            [Effect::Transition {
                status: InstanceStatus::Running,
                ..
            }]
        ));
        assert_eq!(
            m.live.instances[&id("example")].runtime.status,
            InstanceStatus::Running
        );
    }
    #[test]
    fn resync_stages_replacement_and_disconnect_preserves_old_snapshot() {
        let mut m = Mirror::default();
        recorded(&mut m, hello("one"));
        recorded(&mut m, WireEvent::ProjectUpdated { project: project() });
        recorded(&mut m, WireEvent::SnapshotComplete);
        m.stream(StreamEvent::Lost {
            error: "connection lost".into(),
            daemon_unreachable: true,
            attempt: 0,
            retry_in: Duration::from_secs(1),
        });
        assert!(matches!(m.connection, Connection::Down { .. }));
        assert_eq!(m.live.projects.len(), 1);
        assert!(matches!(
            recorded(&mut m, hello("two")).as_slice(),
            [Effect::Restarted]
        ));
        recorded(
            &mut m,
            WireEvent::Resync {
                reason: "lagged".into(),
                skipped: Some(3),
            },
        );
        assert_eq!(m.live.projects.len(), 1);
        recorded(&mut m, hello("two"));
        recorded(&mut m, WireEvent::SnapshotComplete);
        assert!(m.live.projects.is_empty());
        assert_eq!(m.boot_id.as_deref(), Some("two"));
    }

    #[test]
    fn client_reconnects_and_replays_after_resync() {
        use futures_util::StreamExt;
        use std::{
            io::{Read, Write},
            net::TcpListener,
            thread,
            time::Instant,
        };
        let listener = (39340..39360)
            .find_map(|port| TcpListener::bind(("127.0.0.1", port)).ok())
            .expect("no free GUI test port");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let frame = |event: WireEvent| {
            format!(
                "event: {}\ndata: {}\n\n",
                event.event_name(),
                serde_json::to_string(&event).unwrap()
            )
        };
        let first = [
            hello("one"),
            WireEvent::ProjectUpdated { project: project() },
            WireEvent::InstanceUpdated {
                instance: instance(InstanceStatus::Stopped),
            },
            WireEvent::SnapshotComplete,
            WireEvent::InstanceUpdated {
                instance: instance(InstanceStatus::Running),
            },
            WireEvent::Resync {
                reason: "lagged".into(),
                skipped: Some(1),
            },
        ]
        .into_iter()
        .map(frame)
        .collect::<String>();
        let second = [
            hello("two"),
            WireEvent::ProjectUpdated { project: project() },
            WireEvent::SnapshotComplete,
        ]
        .into_iter()
        .map(frame)
        .collect::<String>();
        let server = thread::spawn(move || {
            for body in [first, second] {
                let deadline = Instant::now() + Duration::from_secs(5);
                let (mut socket, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(5))
                        }
                        Err(error) => panic!("test server accept: {error}"),
                    }
                };
                socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut request = [0; 4096];
                assert!(socket.read(&mut request).unwrap() > 0);
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
            }
        });
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let client = lantern_client::Client::new(lantern_client::ClientConfig {
                base_url: Some(format!("http://127.0.0.1:{port}")),
            })
            .unwrap();
            let mut events = client.events(lantern_client::EventsOptions::default());
            let mut mirror = Mirror::default();
            let mut snapshots = 0;
            tokio::time::timeout(Duration::from_secs(5), async {
                while let Some(event) = events.next().await {
                    if matches!(&event, StreamEvent::Event(WireEvent::SnapshotComplete)) {
                        snapshots += 1;
                    }
                    mirror.stream(event);
                    if snapshots == 2 {
                        break;
                    }
                }
            })
            .await
            .expect("SSE replay timed out");
            assert_eq!(snapshots, 2);
            assert_eq!(mirror.boot_id.as_deref(), Some("two"));
            assert_eq!(mirror.connection, Connection::Live);
            assert_eq!(mirror.live.projects.len(), 1);
            assert!(mirror.live.instances.is_empty());
        });
        server.join().unwrap();
    }
}
