use lantern_core::{dirs::Dirs, state};
use lantern_daemon::registry::state_io::{Error, Recovery, StateIo, empty_state};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "lantern-registry-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
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
async fn registry_state_lock_atomic_flush_backup_and_recovery() {
    let temp = Temp::new();
    let dirs = temp.dirs();
    let (io, mut state, recovery) = StateIo::open(&dirs).unwrap();
    assert_eq!(recovery, Recovery::None);
    assert!(matches!(StateIo::open(&dirs), Err(Error::Locked)));
    state.active_profile = Some("first".into());
    io.mark_dirty(state.clone());
    io.flush().await.unwrap();
    let path = dirs.state.join("state.json");
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    state.active_profile = Some("second".into());
    io.mark_dirty(state.clone());
    io.flush().await.unwrap();
    assert_eq!(
        state::from_str(&fs::read_to_string(dirs.state.join("state.json.bak")).unwrap())
            .unwrap()
            .active_profile
            .as_deref(),
        Some("first")
    );
    assert_eq!(
        state::from_str(&fs::read_to_string(&path).unwrap())
            .unwrap()
            .active_profile
            .as_deref(),
        Some("second")
    );
    drop(io);
    tokio::time::sleep(Duration::from_millis(30)).await;
    fs::write(&path, b"{broken").unwrap();
    let (io, recovered, why) = StateIo::open(&dirs).unwrap();
    assert!(matches!(why, Recovery::Backup { .. }));
    assert_eq!(recovered.active_profile.as_deref(), Some("first"));
    io.flush().await.unwrap();
    assert_eq!(
        state::from_str(&fs::read_to_string(path).unwrap())
            .unwrap()
            .active_profile
            .as_deref(),
        Some("first")
    );
}

#[tokio::test]
async fn registry_state_future_refused_and_corruption_without_backup_is_preserved() {
    let temp = Temp::new();
    let dirs = temp.dirs();
    fs::create_dir_all(&dirs.state).unwrap();
    let path = dirs.state.join("state.json");
    let future = "{\"version\":999}";
    fs::write(&path, future).unwrap();
    assert!(matches!(
        StateIo::open(&dirs),
        Err(Error::State(state::Error::FutureVersion { .. }))
    ));
    assert_eq!(fs::read_to_string(&path).unwrap(), future);
    fs::write(&path, "{bad").unwrap();
    let (io, loaded, recovery) = StateIo::open(&dirs).unwrap();
    let Recovery::Empty { corrupt } = recovery else {
        panic!("expected empty recovery");
    };
    assert_eq!(fs::read_to_string(corrupt).unwrap(), "{bad");
    assert!(loaded.projects.is_empty());
    io.flush().await.unwrap();
}

#[tokio::test]
async fn registry_state_debounce_and_retry() {
    let temp = Temp::new();
    let dirs = temp.dirs();
    let (io, mut state, _) = StateIo::open(&dirs).unwrap();
    state.active_profile = Some("light".into());
    io.mark_dirty(state.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!dirs.state.join("state.json").exists());
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(dirs.state.join("state.json").exists());
    fs::create_dir(dirs.state.join("state.json.tmp")).unwrap();
    state.active_profile = Some("retry".into());
    io.mark_dirty(state);
    assert!(io.flush().await.is_err());
    assert!(io.subscribe_errors().borrow().is_some());
    fs::remove_dir(dirs.state.join("state.json.tmp")).unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        state::from_str(&fs::read_to_string(dirs.state.join("state.json")).unwrap())
            .unwrap()
            .active_profile
            .as_deref(),
        Some("retry")
    );
    assert!(io.subscribe_errors().borrow().is_none());
    assert!(empty_state().projects.is_empty());
}

#[tokio::test]
async fn registry_state_max_wait_under_continuous_mutations() {
    let temp = Temp::new();
    let dirs = temp.dirs();
    let (io, mut state, _) = StateIo::open(&dirs).unwrap();
    for i in 0..12 {
        state.active_profile = Some(format!("{i}"));
        io.mark_dirty(state.clone());
        tokio::time::sleep(Duration::from_millis(190)).await;
    }
    assert!(dirs.state.join("state.json").exists());
    io.flush().await.unwrap();
    assert_eq!(
        state::from_str(&fs::read_to_string(dirs.state.join("state.json")).unwrap())
            .unwrap()
            .active_profile
            .as_deref(),
        Some("11")
    );
}
