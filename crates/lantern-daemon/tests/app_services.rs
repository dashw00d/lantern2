use lantern_core::{api::ServiceState, settings::Settings};
use lantern_daemon::{app::services::ServiceObserver, runtime::seams::ServiceControl};
use std::{net::Ipv4Addr, time::Duration};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn app_services_observe_only_injected_test_port() {
    let mut listener = None;
    for port in 39860..39890 {
        if let Ok(socket) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
            listener = Some((socket, port));
            break;
        }
    }
    let (socket, port) = listener.expect("available test port");
    let mut settings = Settings::default();
    settings.services.redis.port = Some(port);
    let service = ServiceObserver::new(settings, true);
    assert!(service.has("redis"));
    let running = service.status("redis").await.unwrap();
    assert_eq!(running.state, ServiceState::Running);
    assert!(!running.controllable);
    service
        .ensure_running("redis", Duration::from_secs(2), CancellationToken::new())
        .await
        .unwrap();
    drop(socket);
    assert_eq!(
        service.status("redis").await.unwrap().state,
        ServiceState::Stopped
    );
    assert!(
        service
            .ensure_running("redis", Duration::from_secs(2), CancellationToken::new())
            .await
            .is_err()
    );
}
