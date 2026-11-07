use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
}
impl ThemePreference {
    pub fn next(self) -> Self {
        match self {
            Self::System => Self::Light,
            Self::Light => Self::Dark,
            Self::Dark => Self::System,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct GuiPrefs {
    pub theme: ThemePreference,
}

#[derive(Debug, Clone)]
pub(crate) struct PrefsStore {
    path: PathBuf,
}
impl PrefsStore {
    fn from_state_home(home: &Path) -> Self {
        Self {
            path: home.join("lantern/gui.json"),
        }
    }
    pub fn from_env() -> Option<Self> {
        let home = std::env::var_os("XDG_STATE_HOME")
            .filter(|p| Path::new(p).is_absolute())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state")))?;
        Some(Self::from_state_home(&home))
    }
    pub fn load(&self) -> io::Result<GuiPrefs> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(GuiPrefs::default()),
            Err(error) => Err(error),
        }
    }
    pub fn save(&self, prefs: &GuiPrefs) -> io::Result<()> {
        use std::io::Write;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = self
            .path
            .parent()
            .ok_or_else(|| io::Error::other("invalid preferences path"))?;
        std::fs::create_dir_all(parent)?;
        let path = parent.join(format!(
            ".gui-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            serde_json::to_writer_pretty(&mut file, prefs).map_err(io::Error::other)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            std::fs::rename(&path, &self.path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(path);
        }
        result
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prefs_default_roundtrip_and_corrupt_file() {
        let temp = tempfile::tempdir().unwrap();
        let store = PrefsStore::from_state_home(temp.path());
        assert_eq!(store.load().unwrap(), GuiPrefs::default());
        let prefs = GuiPrefs {
            theme: ThemePreference::Dark,
        };
        store.save(&prefs).unwrap();
        assert_eq!(store.load().unwrap(), prefs);
        std::fs::write(temp.path().join("lantern/gui.json"), b"invalid").unwrap();
        assert_eq!(store.load().unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
