//! Match process metadata to registered project checkouts.

use super::procinfo::{ProcInfo, normalize_unit};
use lantern_core::ids::{InstanceId, ProjectId};
use lantern_core::model::{CheckoutKind, MatchSource};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A registered checkout and its instance metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutRef {
    /// Instance for the checkout.
    pub instance: InstanceId,
    /// Project owning the checkout.
    pub project: ProjectId,
    /// Checkout kind.
    pub kind: CheckoutKind,
    /// Current branch, if any.
    pub branch: Option<String>,
    /// Canonical checkout directory.
    pub path: PathBuf,
}
/// Registry-published lookup data built from a plain list of checkouts.
#[derive(Debug, Clone)]
pub struct CheckoutIndex {
    /// Canonical checkout paths.
    pub by_path: HashMap<PathBuf, CheckoutRef>,
    /// Full normalized systemd instance units to instance identifiers.
    pub by_unit: HashMap<String, InstanceId>,
    /// Canonical home directory, excluded from matching.
    pub home: PathBuf,
    /// Workspace roots used to identify unregistered dev projects.
    pub workspace_roots: Vec<PathBuf>,
    /// Whether any instance has persistent adoption intent.
    pub any_adopted: bool,
}
impl CheckoutIndex {
    /// Build an index, dropping root, home and ancestors of home.
    pub fn new(checkouts: impl IntoIterator<Item = CheckoutRef>, home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        let home = home.canonicalize().unwrap_or(home);
        let mut index = Self {
            by_path: HashMap::new(),
            by_unit: HashMap::new(),
            home,
            workspace_roots: Vec::new(),
            any_adopted: false,
        };
        for mut checkout in checkouts {
            checkout.path = checkout.path.canonicalize().unwrap_or(checkout.path);
            if !checkout.path.is_absolute() || index.home.starts_with(&checkout.path) {
                continue;
            }
            for suffix in ["service", "scope"] {
                index.by_unit.insert(
                    format!("lantern-inst-{}.{}", checkout.instance.safe_name(), suffix),
                    checkout.instance.clone(),
                );
            }
            index.by_path.insert(checkout.path.clone(), checkout);
        }
        index
    }
    /// Find the deepest checkout that contains the given path.
    pub fn lookup(&self, path: &Path) -> Option<&CheckoutRef> {
        path.ancestors().find_map(|p| self.by_path.get(p))
    }
    fn by_instance(&self, id: &InstanceId) -> Option<&CheckoutRef> {
        self.by_path.values().find(|c| &c.instance == id)
    }
}

pub(super) fn boundary(p: &ProcInfo, uid: u32, daemon_pid: u32) -> bool {
    if p.key.pid <= 1 || p.ppid == 0 || p.uid != uid || p.key.pid == daemon_pid {
        return true;
    }
    let comm = p.comm.as_str();
    if [
        "systemd",
        "init",
        "konsole",
        "gnome-terminal-server",
        "kitty",
        "alacritty",
        "wezterm-gui",
        "foot",
        "xterm",
        "tilix",
        "ptyxis",
        "tmux: server",
        "screen",
        "sshd",
        "login",
        "su",
        "code",
        "cursor",
        "zed",
    ]
    .contains(&comm)
    {
        return true;
    }
    if ["bash", "zsh", "fish", "sh", "dash", "ksh", "nu", "xonsh"].contains(&comm) {
        let Some(first) = p.cmdline_raw.first() else {
            return true;
        };
        if first.starts_with('-') {
            return true;
        }
        // A command string or a script means the shell is a launcher, not an interactive boundary.
        let args = &p.cmdline_raw[1..];
        return !args.iter().any(|arg| {
            arg == "-c"
                || (arg.starts_with('-') && !arg.starts_with("--") && arg[1..].contains('c'))
                || (!arg.starts_with('-') && !arg.is_empty())
        });
    }
    false
}
pub(super) fn usable_ancestors(p: &ProcInfo, uid: u32, daemon_pid: u32) -> impl Iterator<Item = &ProcInfo> {
    p.ancestors
        .iter()
        .take(5)
        .take_while(move |a| !boundary(a, uid, daemon_pid))
}
pub(super) fn match_owner<'a>(
    p: &ProcInfo,
    index: &'a CheckoutIndex,
    uid: u32,
    daemon_pid: u32,
) -> Option<(&'a CheckoutRef, MatchSource)> {
    if let Some(unit) = &p.unit
        && let Some(id) = index.by_unit.get(&normalize_unit(unit))
        && let Some(c) = index.by_instance(id)
    {
        return Some((c, MatchSource::Unit));
    }
    if let Some(c) = p.cwd.as_deref().and_then(|path| index.lookup(path)) {
        return Some((c, MatchSource::Cwd));
    }
    for (i, ancestor) in usable_ancestors(p, uid, daemon_pid).enumerate() {
        if let Some(c) = ancestor.cwd.as_deref().and_then(|path| index.lookup(path)) {
            return Some((c, MatchSource::AncestorCwd { depth: (i + 1) as u8 }));
        }
    }
    for proc in std::iter::once(p).chain(usable_ancestors(p, uid, daemon_pid)) {
        for path in &proc.cmdline_paths {
            if let Some(c) = index.lookup(path) {
                return Some((c, MatchSource::CmdlinePath));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::procinfo::{ProcReader, ProcRoot, fixture::Tree};
    use std::time::Instant;
    fn checkout(path: &str, name: &str) -> CheckoutRef {
        let project = ProjectId::parse(name).unwrap();
        CheckoutRef {
            instance: InstanceId::primary(&project),
            project,
            kind: CheckoutKind::Primary,
            branch: Some("main".into()),
            path: path.into(),
        }
    }
    #[test]
    fn discovery_match_canonical_command_path() {
        use std::os::unix::fs::symlink;
        let tree = Tree::new();
        let real = tree.0.join("checkout");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("server.js"), "fixture").unwrap();
        let link = tree.0.join("shortcut");
        symlink(&real, &link).unwrap();
        let command = format!("--file={}", link.join("server.js").display());
        tree.process(200, 1, 1, "node", "/", &["node", &command], "/app.service");
        let mut reader = ProcReader::new(ProcRoot::new(&tree.0));
        let proc = reader.read(200, Instant::now()).unwrap();
        let index = CheckoutIndex::new(vec![checkout(real.to_str().unwrap(), "app")], "/home/test");
        assert_eq!(
            match_owner(&proc, &index, reader.root.uid(), 9999).unwrap().1,
            MatchSource::CmdlinePath
        );
    }
    #[test]
    fn discovery_match_fixture() {
        let index = CheckoutIndex::new(
            vec![
                checkout("/dev/app", "app"),
                checkout("/dev/app/.worktrees/branch", "other"),
                checkout("/", "bad"),
                checkout("/home/test", "home"),
            ],
            "/home/test",
        );
        assert_eq!(index.by_path.len(), 2);
        assert!(index.lookup(Path::new("/dev/app2")).is_none());
        assert_eq!(
            index
                .lookup(Path::new("/dev/app/.worktrees/branch/sub"))
                .unwrap()
                .project
                .as_str(),
            "other"
        );
        let t = Tree::new();
        t.process(100, 1, 1, "sh", "/dev/app", &["sh", "-c", "run"], "/app.service");
        t.process(200, 100, 2, "node", "/", &["node"], "/app.service");
        let mut reader = ProcReader::new(ProcRoot::new(&t.0));
        let now = Instant::now();
        let uid = reader.root.uid();
        let p = reader.read(200, now).unwrap();
        assert_eq!(
            match_owner(&p, &index, uid, 9999).unwrap().1,
            MatchSource::AncestorCwd { depth: 1 }
        );
        t.process(100, 1, 3, "bash", "/dev/app", &["bash"], "/app.service");
        t.process(
            200,
            100,
            4,
            "node",
            "/",
            &["node", "--script=/dev/app/src/server.js"],
            "/app.service",
        );
        let p = reader.read(200, now).unwrap();
        assert_eq!(
            match_owner(&p, &index, uid, 9999).unwrap().1,
            MatchSource::CmdlinePath
        );
        t.process(
            200,
            100,
            5,
            "node",
            "/dev/app",
            &["node"],
            "/lantern-inst-other.service",
        );
        let p = reader.read(200, now).unwrap();
        let (found, source) = match_owner(&p, &index, uid, 9999).unwrap();
        assert_eq!(found.project.as_str(), "other");
        assert_eq!(source, MatchSource::Unit);
    }
}
