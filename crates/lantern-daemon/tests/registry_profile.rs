use lantern_core::{detect::Templates, dirs::Dirs, settings::Settings};
use lantern_daemon::{
    events::EventBus,
    registry::{Error, RegistryHandle},
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "lantern-registry-profile-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn dirs(&self) -> Dirs {
        Dirs {
            config: self.0.join("config"),
            state: self.0.join("state"),
            data: self.0.join("data"),
        }
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[tokio::test]
async fn registry_profile_activation_persists_and_can_be_cleared() {
    let t = Temp::new();
    let open = || {
        RegistryHandle::open(
            &t.dirs(),
            Settings::default(),
            Templates::default(),
            t.0.clone(),
            EventBus::new(),
        )
        .unwrap()
        .0
    };
    let registry = open();
    assert_eq!(registry.active_profile(), None);
    assert!(matches!(
        registry.set_active_profile(Some("../bad".into())),
        Err(Error::InvalidProfile(_))
    ));
    registry.set_active_profile(Some("full_stack".into())).unwrap();
    registry.flush().await.unwrap();
    drop(registry);
    tokio::time::sleep(Duration::from_millis(30)).await;
    let registry = open();
    assert_eq!(registry.active_profile().as_deref(), Some("full_stack"));
    registry.set_active_profile(None).unwrap();
    registry.flush().await.unwrap();
    assert!(registry.state_snapshot().active_profile.is_none());
}
