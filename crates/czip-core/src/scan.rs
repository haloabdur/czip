use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use walkdir::WalkDir;

use crate::fsutil::mode_of;
use crate::{Error, Result};

/// One file or directory that will go into an archive.
#[derive(Debug, Clone)]
pub struct SourceItem {
    /// Location on disk.
    pub path: PathBuf,
    /// Path inside the archive, `/`-separated, no leading slash.
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: Option<SystemTime>,
    pub mode: u32,
}

/// Summary of one of the paths the user picked.
#[derive(Debug, Clone)]
pub struct RootSummary {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub bytes: u64,
    pub files: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Scan {
    pub items: Vec<SourceItem>,
    pub roots: Vec<RootSummary>,
    pub total_bytes: u64,
    pub files: u64,
    pub dirs: u64,
    /// Symlinks and unreadable entries that were left out.
    pub skipped: Vec<PathBuf>,
}

/// Walks every input (files and folders, any mix) into a flat list.
///
/// Each input becomes a top-level entry named after itself; clashing names get
/// a ` (2)` suffix. Directories come first, then files in folder order: files
/// of one project sit next to each other in solid streams, which in practice
/// compresses better than grouping by extension.
pub fn scan(inputs: &[PathBuf]) -> Result<Scan> {
    let mut out = Scan::default();
    let mut taken = HashSet::new();
    let mut dirs = Vec::new();
    let mut files = Vec::new();

    for input in inputs {
        let meta = std::fs::symlink_metadata(input).map_err(Error::at(input))?;
        let base = input
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "root".into());
        let root_name = unique_name(&base, &mut taken);
        let mut summary = RootSummary {
            path: input.clone(),
            name: root_name.clone(),
            is_dir: meta.is_dir(),
            bytes: 0,
            files: 0,
        };

        if meta.file_type().is_symlink() {
            out.skipped.push(input.clone());
            continue;
        }
        if meta.is_file() {
            summary.bytes = meta.len();
            summary.files = 1;
            files.push(item(input.clone(), root_name, &meta));
            out.roots.push(summary);
            continue;
        }

        for entry in WalkDir::new(input).follow_links(false).sort_by_file_name() {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    out.skipped.push(e.path().map(Path::to_path_buf).unwrap_or_else(|| input.clone()));
                    continue;
                }
            };
            let ft = entry.file_type();
            if ft.is_symlink() {
                out.skipped.push(entry.path().to_path_buf());
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                out.skipped.push(entry.path().to_path_buf());
                continue;
            };
            let rel = entry.path().strip_prefix(input).unwrap_or(entry.path());
            let mut name = root_name.clone();
            for c in rel.components() {
                name.push('/');
                name.push_str(&c.as_os_str().to_string_lossy());
            }
            if ft.is_dir() {
                dirs.push(item(entry.path().to_path_buf(), name, &meta));
            } else if ft.is_file() {
                summary.bytes += meta.len();
                summary.files += 1;
                files.push(item(entry.path().to_path_buf(), name, &meta));
            }
        }
        out.roots.push(summary);
    }

    out.dirs = dirs.len() as u64;
    out.files = files.len() as u64;
    out.total_bytes = files.iter().map(|f| f.size).sum();
    out.items = dirs;
    out.items.extend(files);
    Ok(out)
}

fn item(path: PathBuf, name: String, meta: &std::fs::Metadata) -> SourceItem {
    SourceItem {
        path,
        name,
        is_dir: meta.is_dir(),
        size: if meta.is_file() { meta.len() } else { 0 },
        mtime: meta.modified().ok(),
        mode: mode_of(meta),
    }
}

fn unique_name(base: &str, taken: &mut HashSet<String>) -> String {
    let mut name = base.to_string();
    let mut n = 2;
    while !taken.insert(name.to_lowercase()) {
        name = match base.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({n}).{ext}"),
            _ => format!("{base} ({n})"),
        };
        n += 1;
    }
    name
}

fn file_name(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

pub(crate) fn ext_of(name: &str) -> String {
    let f = file_name(name);
    match f.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}
