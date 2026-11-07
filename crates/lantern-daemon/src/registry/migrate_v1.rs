//! Explicit, read-only v1 input planning and idempotent import into caller-provided directories.

use super::{Error as RegistryError, RegistryHandle, scan::unique_id};
use lantern_core::{
    Timestamp, detect,
    dirs::Dirs,
    ids::ProjectId,
    manifest::{self, raw::RawManifest},
    model::ProjectSource,
    profiles::{self, Profiles},
    state::{ImportRecord, ProjectRecord, ProjectRepoRecord, RepoRecord},
    yaml,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use toml_edit::{DocumentMut, Item};

const INPUT_LIMIT: u64 = 16 * 1024 * 1024;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Import operation failed before or during application.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("import IO: {0}")]
    Io(#[from] io::Error),
    #[error("invalid v1 input: {0}")]
    Invalid(String),
    #[error(transparent)]
    Registry(#[from] RegistryError),
}
/// Outcome of an individual planned import action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Added,
    AlreadyPresent,
    Updated,
    Skipped,
}
/// One import action, including a non-sensitive reason when relevant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportAction {
    /// Planned result.
    pub kind: ActionKind,
    /// Project name or target filename.
    pub what: String,
    /// Explanation, when applicable.
    pub reason: Option<String>,
}
/// Dry-run or applied import report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportPlan {
    /// Individual projects and files considered.
    pub actions: Vec<ImportAction>,
    /// Nonfatal warnings.
    pub warnings: Vec<String>,
}
#[derive(Debug)]
struct Prepared {
    plan: ImportPlan,
    projects: Vec<ProjectRecord>,
    repos: BTreeMap<PathBuf, RepoRecord>,
    files: Vec<(PathBuf, Vec<u8>, bool)>,
    profile: Option<String>,
    from: PathBuf,
    added: u32,
}

fn read_limited(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut data = Vec::new();
    file.take(INPUT_LIMIT + 1).read_to_end(&mut data)?;
    if data.len() as u64 > INPUT_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} exceeds 16 MiB", path.display()),
        ));
    }
    Ok(Some(data))
}
fn project_repo(path: &Path) -> Option<ProjectRepoRecord> {
    for root in path.ancestors() {
        let dotgit = root.join(".git");
        let git_dir = if dotgit.is_dir() {
            dotgit.canonicalize().ok()?
        } else if dotgit.is_file() {
            let src = read_limited(&dotgit).ok()??;
            let text = std::str::from_utf8(&src).ok()?;
            let target = text.trim().strip_prefix("gitdir:")?.trim();
            root.join(target).canonicalize().ok()?
        } else {
            continue;
        };
        let common = match read_limited(&git_dir.join("commondir")).ok()? {
            Some(bytes) => git_dir
                .join(std::str::from_utf8(&bytes).ok()?.trim())
                .canonicalize()
                .ok()?,
            None => git_dir,
        };
        return Some(ProjectRepoRecord {
            common_dir: common,
            subpath: path.strip_prefix(root).ok()?.to_owned(),
        });
    }
    None
}
fn read_json(path: &Path) -> Result<Option<Value>, Error> {
    read_limited(path)?
        .map(|data| {
            serde_json::from_slice(&data).map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))
        })
        .transpose()
}
fn action(plan: &mut ImportPlan, kind: ActionKind, what: impl Into<String>, reason: Option<String>) {
    plan.actions.push(ImportAction {
        kind,
        what: what.into(),
        reason,
    });
}
fn prune(value: &mut Value) {
    match value {
        Value::Object(m) => {
            m.retain(|_, v| !v.is_null());
            for v in m.values_mut() {
                prune(v);
            }
        }
        Value::Array(a) => {
            for v in a {
                prune(v);
            }
        }
        _ => {}
    }
}
fn difference(current: &Value, baseline: &Value) -> Value {
    match (current, baseline) {
        (Value::Object(a), Value::Object(b)) => {
            let mut out = Map::new();
            for (key, value) in a {
                let diff = difference(value, b.get(key).unwrap_or(&Value::Null));
                if !diff.is_null() {
                    out.insert(key.clone(), diff);
                }
            }
            if out.is_empty() {
                Value::Null
            } else {
                Value::Object(out)
            }
        }
        (a, b) if a == b => Value::Null,
        (a, _) => a.clone(),
    }
}
fn merge_json(base: &mut Value, incoming: &Value) {
    match (base, incoming) {
        (Value::Object(a), Value::Object(b)) => {
            for (key, value) in b {
                merge_json(a.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
        (a, b) => *a = b.clone(),
    }
}
fn project_values(value: &Value, name: &str, tld: &str) -> Value {
    let mut out = Map::new();
    for key in [
        "name",
        "description",
        "kind",
        "tags",
        "enabled",
        "repo_url",
        "type",
        "domain",
        "root",
        "upstream_url",
        "base_url",
        "health_endpoint",
        "template",
        "features",
        "routing",
        "deploy",
        "depends_on",
        "docs",
        "endpoints",
        "docs_auto",
        "api_auto",
    ] {
        if let Some(v) = value.get(key).filter(|v| !v.is_null()) {
            out.insert(key.into(), v.clone());
        }
    }
    if out
        .get("domain")
        .and_then(Value::as_str)
        .is_some_and(|d| d == format!("{name}.{}", tld.trim_start_matches('.')))
    {
        out.remove("domain");
    }
    let mut run = Map::new();
    for (old, new) in [("run_cmd", "cmd"), ("run_cwd", "cwd"), ("run_env", "env")] {
        if let Some(v) = value.get(old).filter(|v| !v.is_null()) {
            run.insert(new.into(), v.clone());
        }
    }
    if let Some(existing) = value.get("run").and_then(Value::as_object) {
        run.extend(existing.clone());
    }
    if !run.is_empty() {
        out.insert("run".into(), Value::Object(run));
    }
    let mut result = Value::Object(out);
    prune(&mut result);
    result
}
fn plan_file(
    plan: &mut ImportPlan,
    files: &mut Vec<(PathBuf, Vec<u8>, bool)>,
    path: PathBuf,
    contents: Vec<u8>,
) -> Result<bool, Error> {
    let what = path.display().to_string();
    match read_limited(&path)? {
        None => {
            action(plan, ActionKind::Added, what, None);
            files.push((path, contents, false));
            Ok(true)
        }
        Some(old) if old == contents => {
            action(plan, ActionKind::AlreadyPresent, what, None);
            Ok(true)
        }
        Some(_) => {
            action(
                plan,
                ActionKind::Skipped,
                what,
                Some("target already exists with different contents".into()),
            );
            Ok(false)
        }
    }
}
fn files_in(dir: &Path, ext: &str) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == ext) && path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}
fn config_plan(
    settings: &Value,
    dirs: &Dirs,
    plan: &mut ImportPlan,
    files: &mut Vec<(PathBuf, Vec<u8>, bool)>,
) -> Result<(), Error> {
    let target = dirs.config.join("config.toml");
    let source = read_limited(&target)?
        .map(|v| String::from_utf8(v).map_err(|e| Error::Invalid(e.to_string())))
        .transpose()?
        .unwrap_or_default();
    let mut doc: DocumentMut = source
        .parse()
        .map_err(|e| Error::Invalid(format!("{}: {e}", target.display())))?;
    let mut changed = false;
    for (old, section, key) in [
        ("workspace_roots", "workspaces", "roots"),
        ("tld", "domains", "tld"),
        ("php_fpm_socket", "php", "fpm_socket"),
        ("default_template", "templates", "default"),
        ("port_range_start", "ports", "range_start"),
        ("port_range_end", "ports", "range_end"),
    ] {
        let Some(value) = settings.get(old).filter(|v| !v.is_null()) else {
            continue;
        };
        if doc.get(section).and_then(|v| v.get(key)).is_some() {
            continue;
        }
        let val = match value {
            Value::String(s) => toml_edit::Value::from(if old == "tld" {
                s.trim_start_matches('.').to_string()
            } else {
                s.clone()
            }),
            Value::Number(n) if n.as_i64().is_some() => toml_edit::Value::from(n.as_i64().unwrap()),
            Value::Array(a) if a.iter().all(Value::is_string) => {
                let mut arr = toml_edit::Array::new();
                for v in a {
                    arr.push(v.as_str().unwrap());
                }
                toml_edit::Value::Array(arr)
            }
            _ => {
                plan.warnings.push(format!("ignored invalid v1 setting {old}"));
                continue;
            }
        };
        if !doc.contains_key(section) {
            doc[section] = Item::Table(toml_edit::Table::new());
        }
        let table = doc[section]
            .as_table_mut()
            .ok_or_else(|| Error::Invalid(format!("{section} is not a TOML table")))?;
        table[key] = Item::Value(val);
        changed = true;
    }
    if changed {
        action(
            plan,
            if source.is_empty() {
                ActionKind::Added
            } else {
                ActionKind::Updated
            },
            target.display().to_string(),
            None,
        );
        files.push((target, doc.to_string().into_bytes(), true));
    }
    Ok(())
}

fn prepare(reg: &RegistryHandle, from: &Path, dirs: &Dirs, force: bool) -> Result<Prepared, Error> {
    let from = from.canonicalize()?;
    // Never write back into the v1 source tree, even when caller supplied overlapping dirs.
    for destination in [&dirs.config, &dirs.state] {
        let path = destination.canonicalize().unwrap_or_else(|_| destination.clone());
        if path.starts_with(&from) || from.starts_with(&path) {
            return Err(Error::Invalid(
                "v1 source and v2 destination directories overlap".into(),
            ));
        }
    }
    let mut plan = ImportPlan {
        actions: vec![],
        warnings: vec![],
    };
    let settings = read_json(&from.join("settings.json"))?.unwrap_or_else(|| serde_json::json!({}));
    let input = read_json(&from.join("state.json"))?
        .ok_or_else(|| Error::Invalid("v1 state.json is missing".into()))?;
    let projects = input
        .get("projects")
        .and_then(Value::as_object)
        .ok_or_else(|| Error::Invalid("v1 projects must be a map".into()))?;
    let mut files = Vec::new();
    config_plan(&settings, dirs, &mut plan, &mut files)?;
    let mut available_profiles: BTreeSet<String> = ["light", "full_stack", "demo"]
        .into_iter()
        .map(str::to_string)
        .collect();
    let (existing_profiles, _) = Profiles::load_dir(&dirs.config.join("profiles"))?;
    available_profiles.extend(existing_profiles.user.keys().cloned());
    for path in files_in(&from.join("profiles"), "json")? {
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if !profiles::valid_name(name) {
            action(
                &mut plan,
                ActionKind::Skipped,
                path.display().to_string(),
                Some("invalid profile name".into()),
            );
            continue;
        }
        let mut json = read_json(&path)?.unwrap_or(Value::Null);
        let Some(obj) = json.as_object_mut() else {
            action(
                &mut plan,
                ActionKind::Skipped,
                path.display().to_string(),
                Some("profile is not an object".into()),
            );
            continue;
        };
        obj.remove("name");
        if let Some(auto) = obj.remove("auto_start_projects") {
            obj.entry("auto_start").or_insert(auto);
        }
        if let (Some(start), Some(end)) = (obj.remove("port_range_start"), obj.remove("port_range_end")) {
            obj.entry("port_range").or_insert(Value::Array(vec![start, end]));
        }
        let text = yaml::to_string(&json).map_err(Error::Invalid)?;
        let parsed = profiles::parse_profile(&text, &path);
        if !parsed.usable {
            action(
                &mut plan,
                ActionKind::Skipped,
                path.display().to_string(),
                Some("invalid profile".into()),
            );
            continue;
        }
        let target = dirs.config.join("profiles").join(format!("{name}.yaml"));
        if plan_file(&mut plan, &mut files, target, text.into_bytes())? {
            available_profiles.insert(name.to_owned());
        }
    }
    for path in files_in(&from.join("templates"), "yml")? {
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if !detect::valid_template_name(name) {
            action(
                &mut plan,
                ActionKind::Skipped,
                path.display().to_string(),
                Some("invalid template name".into()),
            );
            continue;
        }
        let bytes = read_limited(&path)?.unwrap_or_default();
        let text = match std::str::from_utf8(&bytes) {
            Ok(s) => s,
            Err(_) => {
                action(
                    &mut plan,
                    ActionKind::Skipped,
                    path.display().to_string(),
                    Some("template is not UTF-8".into()),
                );
                continue;
            }
        };
        let parsed = manifest::parse_manifest(text, &path);
        if !parsed.usable {
            action(
                &mut plan,
                ActionKind::Skipped,
                path.display().to_string(),
                Some("invalid template YAML".into()),
            );
            continue;
        }
        let mut json = serde_json::to_value(parsed.raw).map_err(|e| Error::Invalid(e.to_string()))?;
        prune(&mut json);
        let text = yaml::to_string(&json).map_err(Error::Invalid)?;
        plan_file(
            &mut plan,
            &mut files,
            dirs.config.join("templates").join(format!("{name}.yaml")),
            text.into_bytes(),
        )?;
    }
    let active = read_limited(&from.join("active_profile"))?
        .and_then(|b| String::from_utf8(b).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            settings
                .get("active_profile")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let profile = active.filter(|name| {
        if available_profiles.contains(name) {
            true
        } else {
            plan.warnings
                .push(format!("active profile {name} is unavailable"));
            false
        }
    });
    let profile = profile.and_then(|name| match reg.active_profile() {
        None => {
            action(
                &mut plan,
                ActionKind::Added,
                "active_profile",
                Some(format!("activate {name}")),
            );
            Some(name)
        }
        Some(existing) if existing == name => {
            action(
                &mut plan,
                ActionKind::AlreadyPresent,
                "active_profile",
                Some(name),
            );
            None
        }
        Some(existing) => {
            action(
                &mut plan,
                ActionKind::Skipped,
                "active_profile",
                Some(format!("existing profile {existing} retained")),
            );
            None
        }
    });
    let tld = settings.get("tld").and_then(Value::as_str).unwrap_or("glow");
    let snapshot = reg.state_snapshot();
    let mut known = snapshot.projects;
    let mut known_repos = snapshot.repos;
    let mut prepared_repos = BTreeMap::new();
    let mut prepared_projects = Vec::new();
    let mut added = 0;
    for (key, project) in projects {
        let name = project.get("name").and_then(Value::as_str).unwrap_or(key);
        let v1_id = project
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let path_text = project.get("path").and_then(Value::as_str).unwrap_or("");
        let path = Path::new(path_text);
        if key == "lantern"
            || v1_id == Some("lantern")
            || name == "lantern"
            || project.pointer("/detection/source").and_then(Value::as_str) == Some("self")
        {
            action(
                &mut plan,
                ActionKind::Skipped,
                key,
                Some("v1 self-registration is reserved".into()),
            );
            continue;
        }
        if !path.is_absolute() {
            action(
                &mut plan,
                ActionKind::Skipped,
                key,
                Some("relative or missing path".into()),
            );
            continue;
        }
        let path = match path.canonicalize() {
            Ok(p) if p.is_dir() => p,
            _ => {
                action(
                    &mut plan,
                    ActionKind::Skipped,
                    key,
                    Some("project path is missing".into()),
                );
                continue;
            }
        };
        let parsed = manifest::load(&path)?;
        let proposed = parsed
            .as_ref()
            .and_then(|m| m.raw.id.as_deref())
            .or(v1_id)
            .unwrap_or(name);
        let base = match ProjectId::from_name(proposed) {
            Ok(id) => id,
            Err(e) => {
                action(&mut plan, ActionKind::Skipped, key, Some(e.to_string()));
                continue;
            }
        };
        let original = known.values().find(|p| p.root == path).cloned();
        if original.is_some() && !force {
            action(
                &mut plan,
                ActionKind::AlreadyPresent,
                key,
                Some("canonical root already registered".into()),
            );
            continue;
        }
        let incoming = project_values(project, name, tld);
        let overrides = if let Some(manifest) = &parsed {
            let mut base = serde_json::to_value(&manifest.raw).map_err(|e| Error::Invalid(e.to_string()))?;
            prune(&mut base);
            let d = difference(&incoming, &base);
            if d.is_null() { serde_json::json!({}) } else { d }
        } else {
            incoming
        };
        let overrides = if let Some(existing) = &original {
            let mut merged =
                serde_json::to_value(&existing.overrides).map_err(|e| Error::Invalid(e.to_string()))?;
            prune(&mut merged);
            merge_json(&mut merged, &overrides);
            merged
        } else {
            overrides
        };
        let overrides: RawManifest = match serde_json::from_value(overrides) {
            Ok(v) => v,
            Err(e) => {
                action(
                    &mut plan,
                    ActionKind::Skipped,
                    key,
                    Some(format!("invalid project fields: {e}")),
                );
                continue;
            }
        };
        if let Some(mut record) = original {
            record.overrides = overrides;
            known.insert(record.id.clone(), record.clone());
            prepared_projects.push(record);
            action(
                &mut plan,
                ActionKind::Updated,
                key,
                Some("re-merged overrides".into()),
            );
            continue;
        }
        let id = unique_id(&base, &known);
        if id != base {
            plan.warnings
                .push(format!("duplicate id {base} imported as {id}"));
        }
        let mut aliases = Vec::new();
        for alias in [Some(key.as_str()), Some(name), v1_id].into_iter().flatten() {
            if alias != id.as_str()
                && !aliases.iter().any(|a| a == alias)
                && !known
                    .values()
                    .any(|p| p.id.as_str() == alias || p.aliases.iter().any(|a| a == alias))
            {
                aliases.push(alias.to_string());
            }
        }
        let repo = project_repo(&path);
        if let Some(repo) = &repo
            && !known_repos.contains_key(&repo.common_dir)
        {
            let record = RepoRecord { slug: id.to_string() };
            known_repos.insert(repo.common_dir.clone(), record.clone());
            prepared_repos.insert(repo.common_dir.clone(), record);
        }
        let now = Timestamp::now();
        let record = ProjectRecord {
            id: id.clone(),
            aliases,
            root: path,
            repo,
            source: if parsed.is_some() {
                ProjectSource::Manifest
            } else {
                ProjectSource::Manual
            },
            registered_at: now,
            last_seen_at: now,
            missing: false,
            overrides,
        };
        known.insert(id, record.clone());
        prepared_projects.push(record);
        added += 1;
        action(&mut plan, ActionKind::Added, key, None);
    }
    Ok(Prepared {
        plan,
        projects: prepared_projects,
        repos: prepared_repos,
        files,
        profile,
        from,
        added,
    })
}

fn write_file(path: &Path, contents: &[u8], replace: bool) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("target has no parent"))?;
    fs::create_dir_all(dir)?;
    let temp = dir.join(format!(
        ".lantern-import-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        if replace {
            fs::rename(&temp, path)?;
        } else {
            fs::hard_link(&temp, path)?;
            fs::remove_file(&temp)?;
        }
        fs::File::open(dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

impl RegistryHandle {
    /// Plan or apply an idempotent import from an explicitly provided v1 directory.
    pub fn migrate_v1(
        &self,
        from: &Path,
        dirs: &Dirs,
        dry_run: bool,
        force: bool,
    ) -> Result<ImportPlan, Error> {
        let prepared = prepare(self, from, dirs, force)?;
        if dry_run {
            return Ok(prepared.plan);
        }
        for (path, contents, replace) in &prepared.files {
            write_file(path, contents, *replace)?;
        }
        for (common_dir, repo) in prepared.repos {
            self.upsert_repo(common_dir, repo);
        }
        for project in prepared.projects {
            self.upsert_project(project)?;
        }
        let mut state = self.lock();
        if state.file.active_profile.is_none() {
            state.file.active_profile = prepared.profile;
        }
        state.file.imports.insert(
            "v1".into(),
            ImportRecord {
                at: Timestamp::now(),
                from: prepared.from,
                projects: prepared.added,
            },
        );
        state.dirty();
        Ok(prepared.plan)
    }
}
