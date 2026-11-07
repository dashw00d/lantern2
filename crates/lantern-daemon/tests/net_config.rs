use lantern_core::{ids::InstanceId, settings::ProxySettings};
use lantern_daemon::{
    net::{
        caddy_config::{RenderCtx, render},
        routes::{Route, RouteOwner, RoutePlan, Target},
    },
    runtime::seams::RouteTarget,
};
use std::{collections::BTreeMap, path::PathBuf};

fn context() -> RenderCtx {
    RenderCtx {
        gateway: "127.0.0.1:39701".parse().unwrap(),
        admin_socket: "/run/lantern/caddy.sock".into(),
        storage: "/data/caddy".into(),
        root_cert: "/data/root.crt".into(),
        root_key: "/data/root.key".into(),
        php_socket: "/run/php/fpm.sock".into(),
        php_env: BTreeMap::from([("MAIL_HOST".into(), "127.0.0.1".into())]),
        proxy: ProxySettings {
            http_port: 39702,
            https_port: 39703,
            ..ProxySettings::default()
        },
    }
}
fn instance(target: Target) -> Route {
    Route {
        host: "app.glow".into(),
        owner: RouteOwner::Instance(InstanceId::parse("app").unwrap()),
        target,
    }
}
fn scenario(name: &str) -> RoutePlan {
    let live = |target| Target::Live { target, tls: false };
    let route = match name {
        "proxy" => Some(instance(live(RouteTarget::Socket(
            "127.0.0.1:39704".parse().unwrap(),
        )))),
        "php" => Some(instance(live(RouteTarget::Php(PathBuf::from("/site/public"))))),
        "static" => Some(instance(live(RouteTarget::Static(PathBuf::from("/site/static"))))),
        "adopted" => Some(instance(Target::Live {
            target: RouteTarget::Socket("127.0.0.1:39705".parse().unwrap()),
            tls: true,
        })),
        "ipv6" => Some(instance(live(RouteTarget::Socket(
            "[::1]:39706".parse().unwrap(),
        )))),
        "empty" => None,
        _ => unreachable!(),
    };
    let mut routes: Vec<_> = route.into_iter().collect();
    if name != "empty" {
        routes.push(Route {
            host: "down.glow".into(),
            owner: RouteOwner::Instance(InstanceId::parse("down").unwrap()),
            target: Target::Down,
        });
        routes.push(Route {
            host: "lantern.glow".into(),
            owner: RouteOwner::Gateway,
            target: Target::Gateway,
        });
    }
    RoutePlan {
        routes,
        skipped: vec![],
    }
}
#[test]
fn net_caddy_golden_configs() {
    for name in ["proxy", "php", "static", "adopted", "ipv6", "empty"] {
        let actual =
            serde_json::to_string_pretty(&render(&scenario(name), &context()).unwrap()).unwrap() + "\n";
        let path = format!("{}/src/net/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
        assert_eq!(actual, std::fs::read_to_string(path).unwrap(), "{name}");
    }
}
#[test]
fn net_caddy_rejects_non_origin_urls() {
    let plan = RoutePlan {
        routes: vec![instance(Target::Live {
            target: RouteTarget::Url("http://127.0.0.1/foo".into()),
            tls: false,
        })],
        skipped: vec![],
    };
    assert!(render(&plan, &context()).is_err());
}
