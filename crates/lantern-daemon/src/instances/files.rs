//! Bounded, non-overwriting copy and symlink materialization.
use super::InstanceError;
use globset::GlobBuilder;
use std::{
    fs,
    os::unix::fs::symlink,
    path::{Component, Path, PathBuf},
};
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

const MAX_FILES: usize = 500;
const MAX_BYTES: u64 = 100 * 1024 * 1024;

/// Materialize paths relative to the destination git root; partial paths survive an error.
pub fn materialize(
    primary_root: &Path,
    primary_project: &Path,
    dest_root: &Path,
    dest_project: &Path,
    patterns: &[String],
    link: bool,
    cancel: &CancellationToken,
) -> (Vec<PathBuf>, Result<(), InstanceError>) {
    let mut made = Vec::new();
    let result = (|| {
        let primary_root = fs::canonicalize(primary_root)?;
        let dest_root = fs::canonicalize(dest_root)?;
        let primary_project = fs::canonicalize(primary_project)?;
        let dest_project = fs::canonicalize(dest_project)?;
        if !primary_project.starts_with(&primary_root) || !dest_project.starts_with(&dest_root) {
            return Err(InstanceError::UnsafePath("project outside its checkout".into()));
        }
        let mut budget = Budget {
            files: 0,
            bytes: 0,
            dest_root: &dest_root,
            made: &mut made,
        };
        for pattern in patterns {
            if cancel.is_cancelled() {
                return Err(InstanceError::Cancelled);
            }
            let from_root = pattern.starts_with('/');
            let pat = pattern.strip_prefix('/').unwrap_or(pattern);
            let p = Path::new(pat);
            if pat.is_empty()
                || p.components().any(|c| !matches!(c, Component::Normal(_)))
                || p.components().any(|c| c.as_os_str() == ".git")
            {
                return Err(InstanceError::UnsafePath(pattern.clone()));
            }
            let matcher = GlobBuilder::new(pat)
                .literal_separator(true)
                .build()
                .map_err(|e| InstanceError::UnsafePath(e.to_string()))?
                .compile_matcher();
            let (src_base, dst_base) = if from_root {
                (&primary_root, &dest_root)
            } else {
                (&primary_project, &dest_project)
            };
            let literal = pat.split(['*', '?', '[', '{']).next().unwrap_or("");
            let walker = WalkDir::new(src_base)
                .follow_links(false)
                .into_iter()
                .filter_entry(|e| allow(e, src_base, literal));
            let mut matches = Vec::new();
            for entry in walker {
                let entry = entry.map_err(|e| InstanceError::Io(std::io::Error::other(e)))?;
                if entry.depth() > 0
                    && matcher.is_match(entry.path().strip_prefix(src_base).expect("walk under root"))
                {
                    matches.push(entry.path().to_path_buf());
                }
            }
            matches.sort();
            for src in matches {
                if cancel.is_cancelled() {
                    return Err(InstanceError::Cancelled);
                }
                let meta = fs::symlink_metadata(&src)?;
                if meta.file_type().is_symlink() {
                    continue;
                }
                let canonical = fs::canonicalize(&src)?;
                if !canonical.starts_with(&primary_root) {
                    return Err(InstanceError::UnsafePath(src.display().to_string()));
                }
                let dest = dst_base.join(src.strip_prefix(src_base).expect("walk under root"));
                if fs::symlink_metadata(&dest).is_ok() {
                    continue;
                }
                safe_parent(
                    &dest_root,
                    dest.parent()
                        .ok_or_else(|| InstanceError::UnsafePath(dest.display().to_string()))?,
                )?;
                if link {
                    symlink(&canonical, &dest)?;
                    budget.record(&dest);
                } else if meta.is_dir() {
                    copy_dir(&src, &dest, &primary_root, src_base, literal, &mut budget, cancel)?;
                } else if meta.is_file() {
                    budget.copy_file(&src, &dest)?;
                }
            }
        }
        Ok(())
    })();
    (made, result)
}
fn allow(e: &walkdir::DirEntry, base: &Path, literal: &str) -> bool {
    if e.depth() == 0 {
        return true;
    }
    let name = e.file_name().to_string_lossy();
    if name == ".git" {
        return false;
    }
    if e.file_type().is_dir() && matches!(name.as_ref(), "node_modules" | ".venv" | "vendor" | "target") {
        let rel = e.path().strip_prefix(base).unwrap_or(Path::new(""));
        return Path::new(literal).starts_with(rel);
    }
    true
}
struct Budget<'a> {
    files: usize,
    bytes: u64,
    dest_root: &'a Path,
    made: &'a mut Vec<PathBuf>,
}
impl Budget<'_> {
    fn record(&mut self, path: &Path) {
        self.made.push(
            path.strip_prefix(self.dest_root)
                .expect("destination under root")
                .to_path_buf(),
        );
    }
    fn copy_file(&mut self, src: &Path, dst: &Path) -> Result<(), InstanceError> {
        let size = fs::metadata(src)?.len();
        self.files += 1;
        self.bytes = self.bytes.saturating_add(size);
        if self.files > MAX_FILES || self.bytes > MAX_BYTES {
            return Err(InstanceError::CopyLimit);
        }
        let mut input = fs::File::open(src)?;
        let mut output = fs::OpenOptions::new().write(true).create_new(true).open(dst)?;
        self.record(dst);
        std::io::copy(&mut input, &mut output)?;
        fs::set_permissions(dst, fs::metadata(src)?.permissions())?;
        Ok(())
    }
}
fn safe_parent(root: &Path, parent: &Path) -> Result<(), InstanceError> {
    let rel = parent
        .strip_prefix(root)
        .map_err(|_| InstanceError::UnsafePath(parent.display().to_string()))?;
    let mut current = root.to_path_buf();
    for component in rel.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(InstanceError::UnsafePath(parent.display().to_string()));
        }
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
                return Err(InstanceError::UnsafePath(current.display().to_string()));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&current)?,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn copy_dir(
    src: &Path,
    dst: &Path,
    root: &Path,
    base: &Path,
    literal: &str,
    budget: &mut Budget<'_>,
    cancel: &CancellationToken,
) -> Result<(), InstanceError> {
    fs::create_dir(dst)?;
    for entry in WalkDir::new(src)
        .follow_links(false)
        .min_depth(1)
        .into_iter()
        .filter_entry(|e| allow(e, base, literal))
    {
        if cancel.is_cancelled() {
            return Err(InstanceError::Cancelled);
        }
        let entry = entry.map_err(|e| InstanceError::Io(std::io::Error::other(e)))?;
        if entry.file_type().is_symlink() {
            continue;
        }
        let source = entry.path();
        if !fs::canonicalize(source)?.starts_with(root) {
            return Err(InstanceError::UnsafePath(source.display().to_string()));
        }
        let out = dst.join(source.strip_prefix(src).expect("walk under source"));
        if entry.file_type().is_dir() {
            fs::create_dir(&out)?;
        } else if entry.file_type().is_file() {
            budget.copy_file(source, &out)?;
        }
    }
    Ok(())
}
