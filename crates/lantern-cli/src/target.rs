//! Resolve a user-facing target against the daemon's instance snapshot.
use std::path::Path;

use lantern_core::{
    api::InstanceView,
    ids::{InstanceId, ProjectId},
};

use crate::error::Failure;

fn cwd_instance<'a>(instances: &'a [InstanceView], cwd: &Path) -> Option<&'a InstanceView> {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    instances
        .iter()
        .filter_map(|instance| {
            let path = instance
                .checkout
                .path
                .canonicalize()
                .unwrap_or_else(|_| instance.checkout.path.clone());
            cwd.starts_with(&path)
                .then_some((path.components().count(), instance))
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, instance)| instance)
}

fn choose<'a>(matches: Vec<&'a InstanceView>, name: &str) -> Result<&'a InstanceView, Failure> {
    match matches.as_slice() {
        [one] => Ok(*one),
        [] => Err(Failure::not_found(format!(
            "no instance matches {name:?}; use `lantern run` to create a branch instance"
        ))),
        _ => {
            let names = matches
                .iter()
                .take(5)
                .map(|i| i.id.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            Err(Failure::not_found(format!("ambiguous target {name:?}: {names}")))
        }
    }
}

/// Resolve an exact instance, branch name/slug, or unique prefix.
pub(crate) fn resolve(
    instances: &[InstanceView],
    target: Option<&str>,
    cwd: &Path,
) -> Result<InstanceId, Failure> {
    let Some(target) = target else {
        return cwd_instance(instances, cwd)
            .map(|i| i.id.clone())
            .ok_or_else(|| Failure::not_found("no instance contains the current directory"));
    };
    if let Some(i) = instances.iter().find(|i| i.id.as_str() == target) {
        return Ok(i.id.clone());
    }
    let matches = if let Some((project, branch)) = target.split_once('@') {
        if branch.is_empty() || branch.contains('@') {
            return Err(Failure::usage("target must be project[@branch]"));
        }
        let project = if project.is_empty() {
            cwd_instance(instances, cwd)
                .ok_or_else(|| Failure::not_found("no project contains the current directory"))?
                .project
                .as_str()
        } else {
            project
        };
        let scoped: Vec<_> = instances
            .iter()
            .filter(|i| i.project.as_str() == project)
            .collect();
        let exact: Vec<_> = scoped
            .iter()
            .copied()
            .filter(|i| {
                i.checkout.branch.as_deref() == Some(branch)
                    || i.id.slug() == Some(branch)
                    || i.id.slug() == Some(lantern_core::ids::branch_slug(branch).as_str())
            })
            .collect();
        if !exact.is_empty() {
            exact
        } else {
            scoped
                .into_iter()
                .filter(|i| {
                    i.id.slug().is_some_and(|s| s.starts_with(branch))
                        || i.checkout
                            .branch
                            .as_deref()
                            .is_some_and(|s| s.starts_with(branch))
                })
                .collect()
        }
    } else {
        instances
            .iter()
            .filter(|i| i.id.as_str().starts_with(target))
            .collect()
    };
    Ok(choose(matches, target)?.id.clone())
}

/// Resolve a project name, falling back to the project containing cwd.
pub(crate) fn project(
    instances: &[InstanceView],
    name: Option<&str>,
    cwd: &Path,
) -> Result<ProjectId, Failure> {
    if let Some(name) = name {
        let mut ids: Vec<_> = instances
            .iter()
            .map(|i| &i.project)
            .filter(|id| id.as_str() == name)
            .cloned()
            .collect();
        if ids.is_empty() {
            ids = instances
                .iter()
                .map(|i| &i.project)
                .filter(|id| id.as_str().starts_with(name))
                .cloned()
                .collect();
        }
        ids.sort();
        ids.dedup();
        return match ids.as_slice() {
            [id] => Ok(id.clone()),
            [] => Err(Failure::not_found(format!("no project matches {name:?}"))),
            _ => Err(Failure::not_found(format!(
                "ambiguous project {name:?}: {}",
                ids.iter()
                    .take(5)
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        };
    }
    cwd_instance(instances, cwd)
        .map(|i| i.project.clone())
        .ok_or_else(|| Failure::not_found("no project contains the current directory"))
}

#[cfg(test)]
mod tests {
    use super::resolve;
    use lantern_core::{
        api::{DiagnosticCounts, InstanceView, ManifestSummary},
        ids::{InstanceId, ProjectId},
        model::{Checkout, CheckoutKind, RuntimeState, ServeMode},
    };
    use std::path::{Path, PathBuf};

    fn view(id: &str, branch: &str, path: &str) -> InstanceView {
        let id = InstanceId::parse(id).unwrap();
        let project = ProjectId::parse("myapp").unwrap();
        InstanceView {
            id,
            project: project.clone(),
            checkout: Checkout {
                path: PathBuf::from(path),
                git_root: None,
                kind: CheckoutKind::Managed,
                branch: Some(branch.into()),
                head: None,
                dirty: None,
                locked: None,
                missing: false,
            },
            domain: "myapp.glow".into(),
            url: "https://myapp.glow".into(),
            pinned: true,
            adopt: false,
            visible: true,
            setup_stale: false,
            runtime: RuntimeState::default(),
            manifest: ManifestSummary {
                id: project,
                serve: ServeMode::Proxy,
                run_cmd: None,
                diagnostics: DiagnosticCounts {
                    errors: 0,
                    warnings: 0,
                    info: 0,
                },
                manifest_rev: "rev".into(),
            },
        }
    }
    #[test]
    fn current_directory_uses_longest_prefix_and_at_branch_uses_its_project() {
        let views = [
            view("myapp", "main", "/tmp/lantern-target"),
            view(
                "myapp@feature-login",
                "feature/login",
                "/tmp/lantern-target/child",
            ),
        ];
        let cwd = Path::new("/tmp/lantern-target/child/nested");
        assert_eq!(
            resolve(&views, None, cwd).unwrap().as_str(),
            "myapp@feature-login"
        );
        assert_eq!(
            resolve(&views, Some("@feature/login"), cwd).unwrap().as_str(),
            "myapp@feature-login"
        );
        assert_eq!(resolve(&views, Some("@main"), cwd).unwrap().as_str(), "myapp");
        assert!(resolve(&views, None, Path::new("/tmp/lantern-target-other")).is_err());
    }
}
