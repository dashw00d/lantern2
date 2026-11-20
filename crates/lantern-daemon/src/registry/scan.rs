//! Bounded, manifest-first workspace scans and registry reconciliation.

use super::{Error, RegistryHandle, expand_home};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use lantern_core::{
    Timestamp,
    api::{Confidence as ApiConfidence, Event, ScanSkip, Suggestion},
    detect::{self, Confidence, Framework},
    ids::{PROJECT_ID_MAX, ProjectId},
    manifest::{self, DetectionInfo},
    model::ProjectSource,
    settings::WorkspaceSettings,
    state::{ProjectRecord, ProjectRepoRecord, RepoRecord},
};
use std::{
    collections::{BTreeSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn build_ignore_matcher(patterns: &[String]) -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        if let Ok(glob) = GlobBuilder::new(pat).literal_separator(true).build() {
            builder.add(glob);
        }
    }
    builder.build().unwrap_or_else(|_| GlobSet::empty())
}

fn is_ignored_name(name: &str, custom: &[String], globs: &GlobSet) -> bool {
    IGNORE.contains(&name) || custom.iter().any(|v| v == name) || globs.is_match(name)
}

fn proposed_project_id(path: &Path, depth: u8) -> Result<ProjectId, lantern_core::ids::IdError> {
    let leaf = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let generic = matches!(
        leaf.to_ascii_lowercase().as_str(),
        "app"
            | "web"
            | "web-app"
            | "webapp"
            | "frontend"
            | "backend"
            | "server"
            | "client"
            | "ui"
            | "site"
            | "www"
            | "api"
    );
    if depth > 1
        && generic
        && let Some(parent) = path.parent().and_then(|p| p.file_name()).and_then(|s| s.to_str())
    {
        let combined = format!("{parent}-{leaf}");
        if let Ok(id) = ProjectId::from_name(&combined) {
            return Ok(id);
        }
    }
    match ProjectId::from_name(leaf) {
        Ok(id) => Ok(id),
        Err(err) => {
            if let Some(parent) = path.parent().and_then(|p| p.file_name()).and_then(|s| s.to_str()) {
                ProjectId::from_name(&format!("{parent}-{leaf}")).or(Err(err))
            } else {
                Err(err)
            }
        }
    }
}

/// Probe a canonical project directory for its enclosing git repository (supporting monorepos and worktrees).
fn probe_git_repo(path: &Path) -> Option<ProjectRepoRecord> {
    for ancestor in path.ancestors() {
        let dot_git = ancestor.join(".git");
        if dot_git.is_dir() {
            let common_dir = dot_git.canonicalize().ok()?;
            let subpath = path.strip_prefix(ancestor).ok()?.to_path_buf();
            return Some(ProjectRepoRecord { common_dir, subpath });
        }
        if dot_git.is_file() {
            let content = fs::read_to_string(&dot_git).ok()?;
            let raw = content.trim().strip_prefix("gitdir:")?.trim();
            let gitdir = if Path::new(raw).is_absolute() {
                PathBuf::from(raw)
            } else {
                ancestor.join(raw)
            };
            let gitdir = gitdir.canonicalize().ok()?;
            let common_dir = match fs::read_to_string(gitdir.join("commondir")) {
                Ok(rel) => {
                    let rel = rel.trim();
                    let p = if Path::new(rel).is_absolute() {
                        PathBuf::from(rel)
                    } else {
                        gitdir.join(rel)
                    };
                    p.canonicalize().ok()?
                }
                Err(_) => gitdir,
            };
            let subpath = path.strip_prefix(ancestor).ok()?.to_path_buf();
            return Some(ProjectRepoRecord { common_dir, subpath });
        }
    }
    None
}

const IGNORE: &[&str] = &[
    "node_modules",
    "vendor",
    "target",
    "dist",
    "build",
    ".next",
    ".nuxt",
    "__pycache__",
    "venv",
    ".venv",
    "_build",
    "deps",
    ".cache",
    ".git",
];
const LIMIT: usize = 5_000;
const BUDGET: Duration = Duration::from_secs(2);

/// A discovered manifest and its proposed project identity.
#[derive(Debug, Clone, PartialEq)]
pub struct ManifestHit {
    /// Canonical project directory.
    pub path: PathBuf,
    /// Parsed manifest, including diagnostics.
    pub manifest: manifest::ParsedManifest,
}
/// Results of bounded filesystem traversal before registry reconciliation.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceScan {
    /// Discovered manifests.
    pub manifests: Vec<ManifestHit>,
    /// Detected but unregistered candidates.
    pub suggestions: Vec<Suggestion>,
    /// Files or directories skipped during traversal.
    pub skipped: Vec<ScanSkip>,
    /// At least one root reached its budget.
    pub truncated: bool,
}
/// Results of applying a scan to the registry.
#[derive(Debug, Clone, PartialEq)]
pub struct ScanReport {
    /// Newly registered project identifiers.
    pub registered: Vec<ProjectId>,
    /// Manifest roots already present.
    pub already_known: u32,
    /// Current visible suggestions.
    pub suggestions: Vec<Suggestion>,
    /// Skipped paths and reasons, including invalid or conflicting projects.
    pub skipped: Vec<ScanSkip>,
    /// User-facing warnings, including suffixed duplicate ids.
    pub warnings: Vec<String>,
    /// At least one root reached its budget.
    pub truncated: bool,
}

/// Scan configured roots without mutating the registry; all supplied paths are explicit.
pub fn scan_roots(settings: &WorkspaceSettings, home: &Path, managed_worktrees: &Path) -> WorkspaceScan {
    let mut report = WorkspaceScan {
        manifests: vec![],
        suggestions: vec![],
        skipped: vec![],
        truncated: false,
    };
    let managed = managed_worktrees
        .canonicalize()
        .unwrap_or_else(|_| managed_worktrees.to_owned());
    let ignore_globs = build_ignore_matcher(&settings.ignore);
    let mut visited = BTreeSet::new();
    for root in &settings.roots {
        let root = expand_home(root, home);
        let canonical = match root.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                report.skipped.push(ScanSkip {
                    path: root,
                    reason: e.to_string(),
                });
                continue;
            }
        };
        let mut queue = VecDeque::from([(canonical, 0_u8)]);
        let start = Instant::now();
        let mut count = 0;
        while let Some((path, depth)) = queue.pop_front() {
            if count >= LIMIT || start.elapsed() >= BUDGET {
                report.truncated = true;
                break;
            }
            let path = match path.canonicalize() {
                Ok(path) => path,
                Err(e) => {
                    report.skipped.push(ScanSkip {
                        path,
                        reason: e.to_string(),
                    });
                    continue;
                }
            };
            if !visited.insert(path.clone()) {
                continue;
            }
            if path == managed || path.starts_with(&managed) {
                report.skipped.push(ScanSkip {
                    path,
                    reason: "managed_worktree".into(),
                });
                continue;
            }
            count += 1;
            if path.join(".git").is_file() {
                report.skipped.push(ScanSkip {
                    path,
                    reason: "linked_worktree".into(),
                });
                continue;
            }
            match manifest::load(&path) {
                Ok(Some(manifest)) => {
                    report.manifests.push(ManifestHit { path, manifest });
                    continue;
                }
                Ok(None) => {}
                Err(e) => {
                    report.skipped.push(ScanSkip {
                        path,
                        reason: e.to_string(),
                    });
                    continue;
                }
            }
            let git = path.join(".git").is_dir();
            if depth > 0 && settings.suggest {
                let detection = detect::detect_dir(&path);
                if (git || detection.framework != Framework::Unknown)
                    && (git || detection.confidence != Confidence::Low)
                {
                    match proposed_project_id(&path, depth) {
                        Ok(proposed_id) => report.suggestions.push(Suggestion {
                            path: path.clone(),
                            proposed_id,
                            framework: detection.framework_name().into(),
                            confidence: match detection.confidence {
                                Confidence::High => ApiConfidence::High,
                                Confidence::Medium => ApiConfidence::Medium,
                                Confidence::Low => ApiConfidence::Low,
                            },
                        }),
                        Err(e) => report.skipped.push(ScanSkip {
                            path: path.clone(),
                            reason: e.to_string(),
                        }),
                    }
                }
            }
            if depth >= settings.depth {
                continue;
            }
            let entries = match fs::read_dir(&path) {
                Ok(e) => e,
                Err(e) => {
                    report.skipped.push(ScanSkip {
                        path,
                        reason: e.to_string(),
                    });
                    continue;
                }
            };
            let mut children = Vec::new();
            for entry in entries {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if is_ignored_name(name.as_ref(), &settings.ignore, &ignore_globs)
                    || (name.starts_with('.') && !settings.include_hidden.iter().any(|v| v == name.as_ref()))
                {
                    continue;
                }
                let ty = match entry.file_type() {
                    Ok(ty) => ty,
                    Err(_) => continue,
                };
                if ty.is_dir() || (ty.is_symlink() && entry.path().is_dir()) {
                    children.push(entry.path());
                }
            }
            children.sort();
            queue.extend(children.into_iter().map(|p| (p, depth + 1)));
        }
    }
    report
}

pub(super) fn unique_id(
    base: &ProjectId,
    projects: &std::collections::BTreeMap<ProjectId, ProjectRecord>,
) -> ProjectId {
    if !projects.contains_key(base)
        && !projects
            .values()
            .any(|p| p.aliases.iter().any(|a| a == base.as_str()))
    {
        return base.clone();
    }
    for n in 2_u32.. {
        let suffix = format!("-{n}");
        let take = PROJECT_ID_MAX
            .saturating_sub(suffix.len())
            .min(base.as_str().len());
        let candidate = format!("{}{}", base.as_str()[..take].trim_end_matches('-'), suffix);
        if let Ok(id) = ProjectId::parse(&candidate)
            && !projects.contains_key(&id)
            && !projects
                .values()
                .any(|p| p.aliases.iter().any(|a| a == id.as_str()))
        {
            return id;
        }
    }
    unreachable!("u32 suffix space exhausted")
}

impl RegistryHandle {
    /// Scan configured roots, register manifests, preserve manual records and replace suggestions.
    pub fn scan_workspace(&self, dirs: &lantern_core::dirs::Dirs) -> Result<ScanReport, Error> {
        let (settings, home) = {
            let s = self.lock();
            (s.settings.clone(), s.home.clone())
        };
        let managed = if settings.worktrees.dir.is_empty() {
            dirs.data.join("worktrees")
        } else {
            expand_home(&settings.worktrees.dir, &home)
        };
        let scan = scan_roots(&settings.workspaces, &home, &managed);
        let mut report = ScanReport {
            registered: vec![],
            already_known: 0,
            suggestions: vec![],
            skipped: scan.skipped,
            warnings: vec![],
            truncated: scan.truncated,
        };
        for hit in scan.manifests {
            let state = self.state_snapshot();
            if let Some(mut existing) = state.projects.values().find(|p| p.root == hit.path).cloned() {
                report.already_known += 1;
                existing.last_seen_at = Timestamp::now();
                existing.missing = false;
                if existing.source == ProjectSource::Detected {
                    existing.source = ProjectSource::Manifest;
                }
                if existing.repo.is_none()
                    && let Some(repo) = probe_git_repo(&hit.path)
                {
                    self.upsert_repo(
                        repo.common_dir.clone(),
                        RepoRecord {
                            slug: existing.id.to_string(),
                        },
                    );
                    existing.repo = Some(repo);
                }
                self.upsert_project(existing)?;
                continue;
            }
            let proposed = hit
                .manifest
                .raw
                .id
                .as_deref()
                .unwrap_or_else(|| hit.path.file_name().and_then(|v| v.to_str()).unwrap_or(""));
            let base = match ProjectId::from_name(proposed) {
                Ok(id) => id,
                Err(e) => {
                    report.skipped.push(ScanSkip {
                        path: hit.path,
                        reason: e.to_string(),
                    });
                    continue;
                }
            };
            if let Some(mut old) = state
                .projects
                .get(&base)
                .filter(|p| !p.root.exists() && p.source != ProjectSource::Manual)
                .cloned()
            {
                let previous = old.root.clone();
                old.root = hit.path.clone();
                old.last_seen_at = Timestamp::now();
                old.missing = false;
                old.source = ProjectSource::Manifest;
                self.upsert_project(old)?;
                report.warnings.push(format!(
                    "relocated {base} from {} to {}",
                    previous.display(),
                    hit.path.display()
                ));
                continue;
            }
            let id = unique_id(&base, &state.projects);
            if id != base {
                report.warnings.push(format!(
                    "duplicate_id: {} and {} both request {base}; registered as {id}",
                    state
                        .projects
                        .get(&base)
                        .map_or_else(|| "an existing alias".into(), |p| p.root.display().to_string()),
                    hit.path.display()
                ));
            }
            let now = Timestamp::now();
            let repo = probe_git_repo(&hit.path).inspect(|repo| {
                self.upsert_repo(repo.common_dir.clone(), RepoRecord { slug: id.to_string() });
            });
            self.upsert_project(ProjectRecord {
                id: id.clone(),
                aliases: vec![],
                root: hit.path,
                repo,
                source: ProjectSource::Manifest,
                registered_at: now,
                last_seen_at: now,
                missing: false,
                overrides: Default::default(),
            })?;
            report.registered.push(id);
        }
        let mut s = self.lock();
        let changed: Vec<_> = s
            .file
            .projects
            .iter()
            .filter_map(|(id, project)| {
                let missing = !project.root.exists();
                (missing != project.missing).then_some((id.clone(), missing))
            })
            .collect();
        for (id, missing) in &changed {
            if let Some(project) = s.file.projects.get_mut(id) {
                project.missing = *missing;
            }
            let primary = lantern_core::ids::InstanceId::primary(id);
            s.rebuild_checkout(&primary);
            s.publish_instance(&primary);
            s.publish_project(id);
        }
        if !changed.is_empty() {
            s.dirty();
            s.publish_index();
        }
        let mut combined = std::collections::BTreeMap::new();
        for suggestion in scan
            .suggestions
            .into_iter()
            .chain(s.listener_suggestions.values().cloned())
        {
            if !s.file.dismissed_suggestions.contains(&suggestion.path)
                && !s.file.projects.values().any(|p| p.root == suggestion.path)
            {
                combined.insert(suggestion.path.clone(), suggestion);
            }
        }
        let suggestions: Vec<_> = combined.into_values().collect();
        if suggestions != s.suggestions {
            s.suggestions = suggestions.clone();
            s.bus.publish(Event::SuggestionsChanged {
                suggestions: suggestions.clone(),
            });
        }
        report.suggestions = suggestions;
        Ok(report)
    }
    /// Offer an unmatched listener's cwd as a suggestion, confined to the injected home directory.
    pub fn offer_suggestion(&self, cwd: &Path) -> Result<Option<Suggestion>, Error> {
        let cwd = cwd.canonicalize()?;
        let mut s = self.lock();
        if !s.settings.workspaces.suggest {
            return Ok(None);
        }
        let home = s.home.canonicalize().unwrap_or_else(|_| s.home.clone());
        if !cwd.starts_with(&home) || cwd == home || s.checkouts.values().any(|c| cwd.starts_with(&c.path)) {
            return Ok(None);
        }
        let ignore_globs = build_ignore_matcher(&s.settings.workspaces.ignore);
        let mut manifest_root = None;
        let mut git_root = None;
        let mut marker_root = None;
        for path in cwd.ancestors().take_while(|p| *p != home) {
            if !path.starts_with(&home) {
                break;
            }
            let ignored = path.strip_prefix(&home).ok().is_some_and(|relative| {
                relative.components().any(|part| {
                    let name = part.as_os_str().to_string_lossy();
                    is_ignored_name(name.as_ref(), &s.settings.workspaces.ignore, &ignore_globs)
                })
            });
            if ignored {
                continue;
            }
            if manifest_root.is_none() && manifest::find_manifest(path).is_some() {
                manifest_root = Some(path.to_path_buf());
            }
            if git_root.is_none() && path.join(".git").exists() {
                git_root = Some(path.to_path_buf());
            }
            if marker_root.is_none()
                && [
                    "package.json",
                    "pyproject.toml",
                    "composer.json",
                    "Cargo.toml",
                    "go.mod",
                    "Gemfile",
                    "deno.json",
                ]
                .iter()
                .any(|f| path.join(f).is_file())
            {
                marker_root = Some(path.to_path_buf());
            }
        }
        let Some(root) = manifest_root.or(git_root).or(marker_root) else {
            return Ok(None);
        };
        if s.file.dismissed_suggestions.contains(&root) || s.file.projects.values().any(|p| p.root == root) {
            return Ok(None);
        }
        if s.suggestion_checked
            .get(&root)
            .is_some_and(|at| at.elapsed() < Duration::from_secs(600))
        {
            return Ok(s.listener_suggestions.get(&root).cloned());
        }
        s.suggestion_checked.insert(root.clone(), Instant::now());
        let name = root.file_name().and_then(|v| v.to_str()).unwrap_or("");
        let Ok(proposed_id) = ProjectId::from_name(name) else {
            return Ok(None);
        };
        let detection = detect::detect_dir(&root);
        let suggestion = Suggestion {
            path: root.clone(),
            proposed_id,
            framework: detection.framework_name().into(),
            confidence: match detection.confidence {
                Confidence::High => ApiConfidence::High,
                Confidence::Medium => ApiConfidence::Medium,
                Confidence::Low => ApiConfidence::Low,
            },
        };
        s.listener_suggestions.insert(root.clone(), suggestion.clone());
        if let Some(old) = s.suggestions.iter_mut().find(|candidate| candidate.path == root) {
            *old = suggestion.clone();
        } else {
            s.suggestions.push(suggestion.clone());
        }
        s.suggestions.sort_by(|a, b| a.path.cmp(&b.path));
        s.bus.publish(Event::SuggestionsChanged {
            suggestions: s.suggestions.clone(),
        });
        Ok(Some(suggestion))
    }
    /// Persistently dismiss one suggestion and immediately update subscribers.
    pub fn dismiss_suggestion(&self, path: &Path) {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_owned());
        let mut s = self.lock();
        if s.file.dismissed_suggestions.insert(path.clone()) {
            s.dirty();
        }
        s.listener_suggestions.remove(&path);
        let old = s.suggestions.len();
        s.suggestions.retain(|v| v.path != path);
        if old != s.suggestions.len() {
            s.bus.publish(Event::SuggestionsChanged {
                suggestions: s.suggestions.clone(),
            });
        }
    }
}
