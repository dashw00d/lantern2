//! Contained checkout documentation discovery and safe text reads.

use globset::{Glob, GlobSet, GlobSetBuilder};
use lantern_core::{
    api::{DocContent, DocEntry, DocsMeta, IntelSource},
    manifest::ResolvedManifest,
};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::Read,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Component, Path},
};
use walkdir::WalkDir;

const PATTERNS: &[&str] = &[
    "README.md",
    "CHANGELOG.md",
    "AGENTS.md",
    "CLAUDE.md",
    "BRAIN.md",
    "SKILL.md",
    "docs/**/*.md",
];
const IGNORES: &[&str] = &[".git/**", "node_modules/**", "deps/**", "_build/**"];
const PRUNE: &[&str] = &[".git", "node_modules", "target", ".venv", "__pycache__"];
fn globs(patterns: &[String]) -> Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        builder.add(Glob::new(p).map_err(|e| format!("invalid glob {p}: {e}"))?);
    }
    builder.build().map_err(|e| e.to_string())
}
fn components(path: &str) -> Result<Vec<&std::ffi::OsStr>, String> {
    let parts: Vec<_> = Path::new(path)
        .components()
        .map(|p| match p {
            Component::Normal(s) => Ok(s),
            _ => Err("invalid checkout-relative path".to_string()),
        })
        .collect::<Result<_, _>>()?;
    if parts.is_empty() {
        return Err("empty path".into());
    }
    Ok(parts)
}
fn open_directory(root: &Path, parts: &[&std::ffi::OsStr]) -> Result<File, String> {
    let canonical = root
        .canonicalize()
        .map_err(|e| format!("checkout unavailable: {e}"))?;
    let mut current = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(canonical)
        .map_err(|e| e.to_string())?;
    for part in parts {
        current = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(Path::new(&format!("/proc/self/fd/{}", current.as_raw_fd())).join(part))
            .map_err(|e| format!("cannot open checkout directory: {e}"))?;
    }
    Ok(current)
}
/// Open each component relative to an already-open directory; no symlink is followed.
pub(super) fn open_contained(root: &Path, path: &str) -> Result<File, String> {
    let parts = components(path)?;
    let parent = open_directory(root, &parts[..parts.len() - 1])?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(Path::new(&format!("/proc/self/fd/{}", parent.as_raw_fd())).join(parts[parts.len() - 1]))
        .map_err(|e| format!("cannot open checkout file: {e}"))?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("document is not a regular file".into());
    }
    Ok(file)
}
fn text_file(mut file: File, max_bytes: u64) -> Result<(String, u64), String> {
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    if size > max_bytes {
        return Err("document exceeds max_bytes".into());
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > max_bytes {
        return Err("document exceeds max_bytes".into());
    }
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return Err("binary file".into());
    }
    Ok((String::from_utf8_lossy(&bytes).into_owned(), size))
}
pub(super) fn read_bytes(root: &Path, path: &str, max: u64) -> Result<Vec<u8>, String> {
    let file = open_contained(root, path)?;
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    if size > max {
        return Err("file exceeds limit".into());
    }
    let mut bytes = Vec::new();
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > max {
        return Err("file exceeds limit".into());
    }
    Ok(bytes)
}
fn kind(path: &str) -> String {
    let n = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    (if n.starts_with("readme") {
        "readme"
    } else if n.starts_with("changelog") {
        "changelog"
    } else if n.starts_with("contributing") {
        "contributing"
    } else if n.contains("openapi") || n.contains("swagger") {
        "openapi"
    } else if n.ends_with(".md") {
        "markdown"
    } else if n.ends_with(".json") {
        "json"
    } else if n.ends_with(".yaml") || n.ends_with(".yml") {
        "yaml"
    } else if n.ends_with(".txt") {
        "text"
    } else {
        "unknown"
    })
    .into()
}
fn entry(root: &Path, path: &str, kind: String, source: IntelSource, max: u64) -> DocEntry {
    let m = open_contained(root, path).ok().and_then(|f| f.metadata().ok());
    let exists = m.as_ref().is_some_and(|m| m.len() <= max);
    let size = m.as_ref().map(fs::Metadata::len);
    let mtime = m
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|t| i64::try_from(t.as_secs()).ok())
        .and_then(|s| lantern_core::Timestamp::from_second(s).ok());
    DocEntry {
        path: path.into(),
        kind,
        source,
        exists,
        size,
        mtime,
    }
}
pub(super) fn scan(
    root: &Path,
    manifest: &ResolvedManifest,
) -> Result<(Vec<DocEntry>, DocsMeta, u64), String> {
    let auto = manifest.docs_auto.as_ref();
    let enabled = auto.and_then(|a| a.enabled).unwrap_or(true);
    let max_bytes = auto
        .and_then(|a| a.max_bytes)
        .unwrap_or(1_048_576)
        .min(16 * 1_048_576);
    let max_files = auto.and_then(|a| a.max_files).unwrap_or(100).min(10_000) as usize;
    let patterns = auto
        .and_then(|a| a.patterns.clone())
        .unwrap_or_else(|| PATTERNS.iter().map(|s| s.to_string()).collect());
    let ignores = auto
        .and_then(|a| a.ignore.clone())
        .unwrap_or_else(|| IGNORES.iter().map(|s| s.to_string()).collect());
    let ignore = globs(&ignores)?;
    let mut docs = Vec::new();
    let mut skipped_large = 0;
    let mut seen = HashSet::new();
    for doc in &manifest.docs {
        if components(&doc.path).is_err() {
            continue;
        }
        if let Ok(file) = open_contained(root, &doc.path) {
            if file.metadata().map_err(|e| e.to_string())?.len() > max_bytes {
                skipped_large += 1;
                continue;
            }
            if text_file(file, max_bytes).is_err() {
                continue;
            }
        }
        if seen.insert(doc.path.clone()) {
            docs.push(entry(
                root,
                &doc.path,
                doc.kind.clone(),
                IntelSource::Manual,
                max_bytes,
            ));
        }
    }
    let mut candidates = Vec::new();
    if enabled {
        for (index, pattern) in patterns.iter().enumerate() {
            components(pattern)?;
            let matcher = globs(std::slice::from_ref(pattern))?;
            // Start at the literal file or directory prefix instead of scanning the whole checkout.
            let prefix = pattern
                .split('/')
                .take_while(|part| !part.contains(['*', '?', '[', '{']))
                .collect::<Vec<_>>();
            if prefix.len() == pattern.split('/').count() {
                if ignore.is_match(pattern) || seen.contains(pattern) {
                    continue;
                }
                if let Ok(file) = open_contained(root, pattern) {
                    if file.metadata().map_err(|e| e.to_string())?.len() > max_bytes {
                        skipped_large += 1;
                    } else if text_file(file, max_bytes).is_ok() {
                        candidates.push((index, pattern.clone()));
                    }
                }
                continue;
            }
            let prefix_string = prefix.join("/");
            let parts = if prefix_string.is_empty() {
                Vec::new()
            } else {
                components(&prefix_string)?
            };
            let directory = match open_directory(root, &parts) {
                Ok(v) => v,
                Err(_) => continue,
            };
            // The walk is rooted at an open directory handle, so swapping a prefix for a
            // symlink cannot redirect the scan; only this proc fd root is followed.
            let anchor = std::path::PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
            for item in WalkDir::new(&anchor)
                .follow_links(false)
                .follow_root_links(true)
                .max_depth(8)
                .into_iter()
                .filter_entry(|e| {
                    e.depth() == 0
                        || !e.file_type().is_dir()
                        || !PRUNE.contains(&e.file_name().to_string_lossy().as_ref())
                })
                .filter_map(Result::ok)
            {
                if !item.file_type().is_file() {
                    continue;
                }
                let relative = match item.path().strip_prefix(&anchor) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let rel = if prefix_string.is_empty() {
                    relative.to_path_buf()
                } else {
                    Path::new(&prefix_string).join(relative)
                };
                let rel = match rel.to_str() {
                    Some(v) => v.to_owned(),
                    None => continue,
                };
                if !matcher.is_match(&rel) || ignore.is_match(&rel) || seen.contains(&rel) {
                    continue;
                }
                if item.metadata().map_or(true, |m| m.len() > max_bytes) {
                    skipped_large += 1;
                    continue;
                }
                if let Ok(file) = open_contained(root, &rel)
                    && text_file(file, max_bytes).is_ok()
                {
                    candidates.push((index, rel));
                }
            }
        }
    }
    candidates.sort();
    let mut unique = HashSet::new();
    candidates.retain(|(_, path)| unique.insert(path.clone()));
    let truncated = candidates.len() > max_files;
    for (_, path) in candidates.into_iter().take(max_files) {
        if seen.insert(path.clone()) {
            docs.push(entry(
                root,
                &path,
                kind(&path),
                IntelSource::Discovered,
                max_bytes,
            ));
        }
    }
    let meta = DocsMeta {
        enabled,
        count: docs.len() as u32,
        source_count: patterns.len() as u32,
        sources: patterns,
        skipped_large,
        truncated,
    };
    Ok((docs, meta, max_bytes))
}
pub(super) fn read(
    root: &Path,
    docs: &[DocEntry],
    path: &str,
    offset: u64,
    max_chars: usize,
    max_bytes: u64,
) -> Result<DocContent, String> {
    if !docs.iter().any(|d| d.path == path && d.exists) {
        return Err("doc_not_found: document is not listed".into());
    }
    let (text, _) = text_file(open_contained(root, path)?, max_bytes)?;
    let total = text.chars().count() as u64;
    let offset = offset.min(total);
    let content: String = text
        .chars()
        .skip(offset as usize)
        .take(max_chars.min(40_000))
        .collect();
    let truncated = offset + (content.chars().count() as u64) < total;
    Ok(DocContent {
        path: path.into(),
        content,
        offset,
        total_chars: total,
        truncated,
    })
}
