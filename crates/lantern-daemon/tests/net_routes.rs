use lantern_core::{
    Timestamp,
    detect::Detection,
    ids::{InstanceId, ProjectId},
    manifest::{self, ResolveInput, TemplateDefinition, TemplateLookup, raw::RawManifest},
    model::{
        Checkout, CheckoutKind, ExternalState, Instance, InstanceStatus, RuntimeState, Scheme, ServeMode,
    },
    settings::Settings,
};
use lantern_daemon::{
    net::{
        routes::{PlanInput, RouteOwner, RoutePlan, Target, build_plan},
        sync::{ApplyFuture, NetRouter, RouteApplier},
    },
    runtime::seams::{RouteTarget, RuntimeRouter},
};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;
struct NoTemplates;
impl TemplateLookup for NoTemplates {
    fn get(&self, _: &str) -> Option<TemplateDefinition> {
        None
    }
}
fn instance(name: &str) -> Instance {
    let project = ProjectId::parse(name).unwrap();
    let manifest = manifest::parse_manifest(
        &format!("id: {name}\ntype: proxy\nrun:\n  cmd: echo\n"),
        "lantern.yaml",
    );
    let resolved = manifest::resolve(ResolveInput {
        dir_name: name,
        is_primary: true,
        primary: Some(&manifest),
        checkout: None,
        overrides: &RawManifest::default(),
        templates: &NoTemplates,
        detection: &Detection::default(),
        settings: &Settings::default(),
    });
    assert!(resolved.usable, "{:?}", resolved.diagnostics);
    Instance {
        id: InstanceId::primary(&project),
        project,
        checkout: Checkout {
            path: PathBuf::from("/checkout"),
            git_root: None,
            kind: CheckoutKind::Primary,
            branch: None,
            head: None,
            dirty: None,
            locked: None,
            missing: false,
        },
        domain: format!("{name}.glow"),
        manifest: Arc::new(resolved),
        runtime: RuntimeState::default(),
        pinned: false,
        adopt: false,
        setup_stale: false,
    }
}
fn target<'a>(plan: &'a RoutePlan, host: &str) -> &'a Target {
    &plan.routes.iter().find(|r| r.host == host).unwrap().target
}
#[test]
fn net_route_d9_statuses_adoption_and_collisions() {
    let mut app = instance("app");
    app.runtime.upstream = Some("127.0.0.1:39730".parse().unwrap());
    for status in [
        InstanceStatus::Stopped,
        InstanceStatus::Preparing,
        InstanceStatus::Starting,
        InstanceStatus::Stopping,
        InstanceStatus::Crashed,
        InstanceStatus::NeedsConfig,
    ] {
        app.runtime.status = status;
        let plan = build_plan(&PlanInput {
            instances: &[app.clone()],
            tld: "glow",
            auto_adopt: false,
        });
        assert_eq!(target(&plan, "app.glow"), &Target::Down, "{status:?}");
    }
    for status in [InstanceStatus::Running, InstanceStatus::Unhealthy] {
        app.runtime.status = status;
        let plan = build_plan(&PlanInput {
            instances: &[app.clone()],
            tld: "glow",
            auto_adopt: false,
        });
        assert!(matches!(
            target(&plan, "app.glow"),
            Target::Live {
                target: RouteTarget::Socket(_),
                ..
            }
        ));
    }
    app.runtime.status = InstanceStatus::External;
    app.runtime.external = Some(ExternalState {
        port: 39731,
        upstream: "[::1]:39731".parse().unwrap(),
        scheme: Scheme::Https,
        pid: None,
        label: "hand started".into(),
        extra_ports: vec![],
        since: Timestamp::now(),
    });
    let plan = build_plan(&PlanInput {
        instances: &[app.clone()],
        tld: "glow",
        auto_adopt: false,
    });
    assert_eq!(target(&plan, "app.glow"), &Target::Down);
    for auto in [false, true] {
        app.adopt = !auto;
        let plan = build_plan(&PlanInput {
            instances: &[app.clone()],
            tld: "glow",
            auto_adopt: auto,
        });
        assert!(
            matches!(target(&plan,"app.glow"),Target::Live{target:RouteTarget::Socket(addr),tls:true} if addr.to_string()=="[::1]:39731")
        );
    }
    app.runtime.status = InstanceStatus::Running;
    Arc::make_mut(&mut app.manifest).serve = ServeMode::Php;
    Arc::make_mut(&mut app.manifest).root = Some("public".into());
    let plan = build_plan(&PlanInput {
        instances: &[app.clone()],
        tld: "glow",
        auto_adopt: false,
    });
    assert!(
        matches!(target(&plan,"app.glow"),Target::Live{target:RouteTarget::Php(p),..} if p==&PathBuf::from("/checkout/public"))
    );
    let mut other = instance("other");
    other.domain = app.domain.clone();
    let mut bad = instance("bad");
    bad.domain = "bad_.glow".into();
    let plan = build_plan(&PlanInput {
        instances: &[other, app, bad],
        tld: "glow",
        auto_adopt: false,
    });
    assert_eq!(plan.skipped.len(), 2);
    assert!(matches!(&plan.routes[0].owner,RouteOwner::Instance(id) if id.as_str()=="app"));
}
#[derive(Debug, Default)]
struct Fake {
    plans: Mutex<Vec<RoutePlan>>,
    fail: AtomicBool,
}
impl RouteApplier for Fake {
    fn apply<'a>(&'a self, p: &'a RoutePlan) -> ApplyFuture<'a> {
        Box::pin(async move {
            self.plans.lock().unwrap().push(p.clone());
            if self.fail.load(Ordering::SeqCst) {
                Err("offline".into())
            } else {
                Ok(())
            }
        })
    }
}
#[tokio::test]
async fn net_router_debounces_keeps_desired_state_and_retries() {
    let fake = Arc::new(Fake::default());
    let cancel = CancellationToken::new();
    let router = NetRouter::new(
        fake.clone(),
        "glow".into(),
        false,
        "/missing/fpm.sock".into(),
        cancel.clone(),
    );
    let mut app = instance("app");
    app.runtime.status = InstanceStatus::Starting;
    router.replace_instances(vec![app.clone()]);
    router.replace_instances(vec![app.clone()]);
    router.flush().await.unwrap();
    assert_eq!(fake.plans.lock().unwrap().len(), 1);
    let socket = RouteTarget::Socket("127.0.0.1:39730".parse().unwrap());
    router.publish(&app, socket.clone()).await.unwrap();
    assert_eq!(target(&router.current_plan(), "app.glow"), &Target::Down);
    app.runtime.status = InstanceStatus::Running;
    router.replace_instances(vec![app.clone()]);
    router.flush().await.unwrap();
    assert_eq!(
        target(&router.current_plan(), "app.glow"),
        &Target::Live {
            target: socket,
            tls: false
        }
    );
    router.unpublish(&app.id).await.unwrap();
    assert_eq!(target(&router.current_plan(), "app.glow"), &Target::Down);
    fake.fail.store(true, Ordering::SeqCst);
    router.replace_instances(vec![app]);
    assert!(router.flush().await.is_err());
    fake.fail.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1400)).await;
    assert!(router.flush().await.is_ok());
    assert!(router.php_ready().await.is_err());
    cancel.cancel();
}
