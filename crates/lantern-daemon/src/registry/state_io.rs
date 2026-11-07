//! Locked, recoverable state loading and a single debounced atomic writer.

use lantern_core::{
    Timestamp,
    dirs::Dirs,
    state::{self, STATE_VERSION, StateFile},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use tokio::{
    sync::{mpsc, oneshot, watch},
    time::{Instant, sleep_until},
};

const DEBOUNCE: Duration = Duration::from_millis(250);
const MAX_WAIT: Duration = Duration::from_secs(2);
const RETRY: Duration = Duration::from_secs(1);
const BACKUP_AGE: Duration = Duration::from_secs(600);

/// State opening errors; incompatible state is never treated as corrupt.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("state IO: {0}")]
    Io(#[from] io::Error),
    #[error("state lock is held by another daemon")]
    Locked,
    #[error(transparent)]
    State(#[from] state::Error),
}

/// Details of a nonfatal recovery at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    /// No recovery was necessary.
    None,
    /// A corrupt file was moved aside and the backup was loaded.
    Backup { corrupt: PathBuf },
    /// A corrupt file was moved aside and no usable backup was available.
    Empty { corrupt: PathBuf },
}

/// Return a fresh empty state without touching any default user directories.
pub fn empty_state() -> StateFile {
    StateFile {
        version: STATE_VERSION,
        saved_at: Timestamp::now(),
        active_profile: None,
        repos: BTreeMap::new(),
        projects: BTreeMap::new(),
        instances: BTreeMap::new(),
        port_leases: BTreeMap::new(),
        dismissed_suggestions: BTreeSet::new(),
        imports: BTreeMap::new(),
    }
}

#[derive(Debug)]
enum Command {
    Dirty(StateFile),
    Flush(oneshot::Sender<Result<(), String>>),
}

/// Handle to the single writer. Send snapshots in mutation order, then flush at barriers.
#[derive(Debug, Clone)]
pub struct StateIo {
    tx: mpsc::UnboundedSender<Command>,
    errors: watch::Receiver<Option<String>>,
}

impl StateIo {
    /// Lock the state directory, load or recover its state, and spawn the writer on the current runtime.
    pub fn open(dirs: &Dirs) -> Result<(Self, StateFile, Recovery), Error> {
        fs::create_dir_all(&dirs.state)?;
        fs::set_permissions(&dirs.state, fs::Permissions::from_mode(0o700))?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(dirs.state.join("state.lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Err(Error::Locked),
            Err(std::fs::TryLockError::Error(e)) => return Err(Error::Io(e)),
        }
        let path = dirs.state.join("state.json");
        let (state, recovery, migrated) = load(&path)?;
        let (tx, rx) = mpsc::unbounded_channel();
        let (err_tx, errors) = watch::channel(None);
        tokio::spawn(writer(path, lock, rx, err_tx));
        let this = Self { tx, errors };
        if recovery != Recovery::None || migrated {
            this.mark_dirty(state.clone());
        }
        Ok((this, state, recovery))
    }

    /// Queue a complete snapshot. The caller must serialize calls with state mutations.
    pub fn mark_dirty(&self, state: StateFile) {
        let _ = self.tx.send(Command::Dirty(state));
    }

    /// Persist all snapshots sent before this call, bypassing debounce.
    pub async fn flush(&self) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Command::Flush(tx))
            .map_err(|_| "state writer stopped".to_string())?;
        rx.await.map_err(|_| "state writer stopped".to_string())?
    }

    /// Observe the latest write error; it clears after a successful retry.
    pub fn subscribe_errors(&self) -> watch::Receiver<Option<String>> {
        self.errors.clone()
    }
}

fn load(path: &Path) -> Result<(StateFile, Recovery, bool), Error> {
    let src = match fs::read_to_string(path) {
        Ok(src) => src,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((empty_state(), Recovery::None, false)),
        Err(e) if e.kind() == io::ErrorKind::InvalidData => String::new(),
        Err(e) => return Err(e.into()),
    };
    match state::from_str(&src) {
        Ok(state) => {
            let version = serde_json::from_str::<serde_json::Value>(&src)
                .ok()
                .and_then(|v| v.get("version").and_then(|v| v.as_u64()));
            let migrated = version.is_some_and(|v| v < u64::from(STATE_VERSION));
            if let Some(version) = version.filter(|_| migrated) {
                let backup = path.with_file_name(format!("state.json.pre-migrate-v{version}"));
                if !backup.exists() {
                    write_copy(path, &backup)?;
                }
            }
            Ok((state, Recovery::None, migrated))
        }
        Err(
            e @ (state::Error::FutureVersion { .. }
            | state::Error::V1File
            | state::Error::UnsupportedVersion(_)),
        ) => Err(e.into()),
        Err(_) => {
            let corrupt =
                path.with_file_name(format!("state.json.corrupt-{}", Timestamp::now().as_nanosecond()));
            fs::rename(path, &corrupt)?;
            let backup = path.with_file_name("state.json.bak");
            match fs::read_to_string(backup)
                .ok()
                .and_then(|s| state::from_str(&s).ok())
            {
                Some(state) => Ok((state, Recovery::Backup { corrupt }, false)),
                None => Ok((empty_state(), Recovery::Empty { corrupt }, false)),
            }
        }
    }
}

fn write_copy(from: &Path, to: &Path) -> io::Result<()> {
    let data = fs::read(from)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(to)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(&data)?;
    file.sync_all()
}

fn write_atomic(path: &Path, state: &StateFile) -> Result<(), String> {
    let mut snapshot = state.clone();
    snapshot.saved_at = Timestamp::now();
    let json = state::to_string(&snapshot).map_err(|e| e.to_string())?;
    let result = (|| -> io::Result<()> {
        let dir = path
            .parent()
            .ok_or_else(|| io::Error::other("state path has no parent"))?;
        let tmp = dir.join("state.json.tmp");
        match fs::remove_file(&tmp) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        let backup = dir.join("state.json.bak");
        let stale = fs::metadata(&backup)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_none_or(|age| age >= BACKUP_AGE);
        if stale
            && fs::read_to_string(path)
                .ok()
                .is_some_and(|s| state::from_str(&s).is_ok())
        {
            write_copy(path, &backup)?;
        }
        fs::rename(&tmp, path)?;
        File::open(dir)?.sync_all()?;
        Ok(())
    })();
    result.map_err(|e| format!("writing {}: {e}", path.display()))
}

async fn writer(
    path: PathBuf,
    _lock: File,
    mut rx: mpsc::UnboundedReceiver<Command>,
    errors: watch::Sender<Option<String>>,
) {
    let mut pending: Option<StateFile> = None;
    let mut first: Option<Instant> = None;
    let mut deadline: Option<Instant> = None;
    loop {
        let timer = deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(86400 * 365));
        tokio::select! {
            command = rx.recv() => match command {
                Some(Command::Dirty(state)) => {
                    let now = Instant::now();
                    let start = *first.get_or_insert(now);
                    pending = Some(state);
                    deadline = Some((now + DEBOUNCE).min(start + MAX_WAIT));
                }
                Some(Command::Flush(reply)) => {
                    let result = persist(&path, &mut pending, &errors);
                    if result.is_ok() { first = None; deadline = None; } else { deadline = Some(Instant::now() + RETRY); }
                    let _ = reply.send(result);
                }
                None => { let _ = persist(&path, &mut pending, &errors); break; }
            },
            () = sleep_until(timer), if deadline.is_some() => {
                if persist(&path, &mut pending, &errors).is_ok() { first = None; deadline = None; }
                else { deadline = Some(Instant::now() + RETRY); }
            }
        }
    }
}

fn persist(
    path: &Path,
    pending: &mut Option<StateFile>,
    errors: &watch::Sender<Option<String>>,
) -> Result<(), String> {
    let Some(state) = pending.as_ref() else {
        return Ok(());
    };
    match write_atomic(path, state) {
        Ok(()) => {
            *pending = None;
            errors.send_replace(None);
            Ok(())
        }
        Err(e) => {
            tracing::warn!(error = %e, "state write failed; will retry");
            errors.send_replace(Some(e.clone()));
            Err(e)
        }
    }
}
