// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use czip_core::{CompressOptions, ExtractOptions, Format, Level, Progress};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

/// The one compress or extract job that may run at a time.
#[derive(Default)]
struct Jobs {
    current: Mutex<Option<Progress>>,
}

#[derive(Debug, Clone, Serialize)]
struct AppError {
    code: String,
    message: String,
}

impl From<czip_core::Error> for AppError {
    fn from(e: czip_core::Error) -> Self {
        Self {
            code: e.code().into(),
            message: e.to_string(),
        }
    }
}

impl AppError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

type CmdResult<T> = Result<T, AppError>;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanItem {
    path: String,
    name: String,
    is_dir: bool,
    bytes: u64,
    files: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanResult {
    items: Vec<ScanItem>,
    total_bytes: u64,
    files: u64,
    skipped: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompressRequest {
    paths: Vec<String>,
    output: String,
    format: String,
    level: String,
    password: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExtractRequest {
    archive: String,
    dest: String,
    password: Option<String>,
    only: Option<Vec<String>>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProgressEvent {
    kind: &'static str,
    total_bytes: u64,
    done_bytes: u64,
    files_done: u64,
    current: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompressDone {
    output: String,
    files: u64,
    input_bytes: u64,
    output_bytes: u64,
    elapsed_ms: u64,
    skipped: usize,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExtractDone {
    dest: String,
    files: u64,
    bytes: u64,
    elapsed_ms: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ListEntry {
    path: String,
    is_dir: bool,
    size: u64,
    mtime: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ArchiveListing {
    path: String,
    name: String,
    format: String,
    encrypted: bool,
    archive_bytes: u64,
    unpacked_bytes: u64,
    entries: Vec<ListEntry>,
    suggested_dest: String,
    /// Folder to open after extracting everything into `suggested_dest`.
    opens: String,
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[tauri::command]
async fn scan_paths(paths: Vec<String>) -> CmdResult<ScanResult> {
    let inputs: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
    let scan = czip_core::scan(&inputs)?;
    Ok(ScanResult {
        items: scan
            .roots
            .iter()
            .map(|r| ScanItem {
                path: s(&r.path),
                name: r.name.clone(),
                is_dir: r.is_dir,
                bytes: r.bytes,
                files: r.files,
            })
            .collect(),
        total_bytes: scan.total_bytes,
        files: scan.files,
        skipped: scan.skipped.len(),
    })
}

/// Where a new archive should go by default: beside the first input.
#[tauri::command]
fn default_output_dir(paths: Vec<String>) -> String {
    paths
        .first()
        .and_then(|p| Path::new(p).parent().map(s))
        .filter(|p| !p.is_empty())
        .or_else(|| dirs_home().map(|h| s(&h)))
        .unwrap_or_default()
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Full path for `name` in `dir`, adding " (2)", " (3)"… so nothing is overwritten.
#[tauri::command]
fn resolve_output(dir: String, name: String, format: String) -> CmdResult<String> {
    let format =
        Format::from_name(&format).ok_or_else(|| AppError::new("bad_request", "unknown format"))?;
    let clean: String = name
        .trim()
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '-'
            } else {
                c
            }
        })
        .collect();
    let stem = if clean.is_empty() {
        "Arsip".to_string()
    } else {
        clean
    };
    let ext = format.extension();
    let dir = PathBuf::from(dir);
    let mut candidate = dir.join(format!("{stem}.{ext}"));
    let mut n = 2;
    while candidate.exists() {
        candidate = dir.join(format!("{stem} ({n}).{ext}"));
        n += 1;
    }
    Ok(s(&candidate))
}

fn begin(jobs: &Jobs) -> CmdResult<Progress> {
    let mut cur = jobs
        .current
        .lock()
        .map_err(|_| AppError::new("busy", "job state poisoned"))?;
    if cur.is_some() {
        return Err(AppError::new("busy", "another job is running"));
    }
    let p = Progress::new();
    *cur = Some(p.clone());
    Ok(p)
}

/// Emits progress every 100 ms until `stop` is set.
fn watch(app: AppHandle, progress: Progress, kind: &'static str, stop: Arc<AtomicBool>) {
    thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            let snap = progress.snapshot();
            let _ = app.emit(
                "job-progress",
                ProgressEvent {
                    kind,
                    total_bytes: snap.total_bytes,
                    done_bytes: snap.done_bytes,
                    files_done: snap.files_done,
                    current: snap.current,
                },
            );
            thread::sleep(Duration::from_millis(100));
        }
    });
}

fn finish(app: &AppHandle) {
    if let Ok(mut cur) = app.state::<Jobs>().current.lock() {
        *cur = None;
    }
}

#[tauri::command]
fn start_compress(app: AppHandle, jobs: State<'_, Jobs>, req: CompressRequest) -> CmdResult<()> {
    let format = Format::from_name(&req.format)
        .ok_or_else(|| AppError::new("bad_request", "unknown format"))?;
    let level = Level::from_name(&req.level)
        .ok_or_else(|| AppError::new("bad_request", "unknown level"))?;
    let progress = begin(&jobs)?;
    let inputs: Vec<PathBuf> = req.paths.iter().map(PathBuf::from).collect();
    let output = PathBuf::from(&req.output);
    let opts = CompressOptions {
        format,
        level,
        password: req.password.filter(|p| !p.is_empty()),
        threads: 0,
    };

    thread::spawn(move || {
        let stop = Arc::new(AtomicBool::new(false));
        watch(app.clone(), progress.clone(), "compress", Arc::clone(&stop));
        let result = czip_core::compress(&inputs, &output, &opts, &progress);
        stop.store(true, Ordering::Relaxed);
        finish(&app);
        let _ = match result {
            Ok(r) => app.emit(
                "compress-done",
                CompressDone {
                    output: s(&r.output),
                    files: r.files,
                    input_bytes: r.input_bytes,
                    output_bytes: r.output_bytes,
                    elapsed_ms: r.elapsed.as_millis() as u64,
                    skipped: r.skipped.len(),
                },
            ),
            Err(e) => app.emit("job-error", AppError::from(e)),
        };
    });
    Ok(())
}

#[tauri::command]
fn start_extract(app: AppHandle, jobs: State<'_, Jobs>, req: ExtractRequest) -> CmdResult<()> {
    let progress = begin(&jobs)?;
    let archive = PathBuf::from(&req.archive);
    let dest = PathBuf::from(&req.dest);
    let opts = ExtractOptions {
        password: req.password.filter(|p| !p.is_empty()),
        only: req.only.filter(|o| !o.is_empty()),
        threads: 0,
    };
    thread::spawn(move || {
        let stop = Arc::new(AtomicBool::new(false));
        watch(app.clone(), progress.clone(), "extract", Arc::clone(&stop));
        let result = czip_core::extract(&archive, &dest, &opts, &progress);
        stop.store(true, Ordering::Relaxed);
        finish(&app);
        let _ = match result {
            Ok(r) => app.emit(
                "extract-done",
                ExtractDone {
                    dest: s(&dest),
                    files: r.files,
                    bytes: r.bytes,
                    elapsed_ms: r.elapsed.as_millis() as u64,
                },
            ),
            Err(e) => app.emit("job-error", AppError::from(e)),
        };
    });
    Ok(())
}

#[tauri::command]
fn cancel_job(jobs: State<'_, Jobs>) {
    if let Ok(cur) = jobs.current.lock() {
        if let Some(p) = cur.as_ref() {
            p.cancel();
        }
    }
}

#[tauri::command]
async fn list_archive(path: String, password: Option<String>) -> CmdResult<ArchiveListing> {
    let p = PathBuf::from(&path);
    let info = czip_core::list(&p, password.as_deref().filter(|p| !p.is_empty()))?;
    let archive_bytes = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
    let (dest, opens) = czip_core::extract_plan(&p, &info);
    Ok(ArchiveListing {
        name: p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        path,
        format: info.format.into(),
        encrypted: info.encrypted,
        archive_bytes,
        unpacked_bytes: info.unpacked_bytes,
        entries: info
            .entries
            .into_iter()
            .map(|e| ListEntry {
                path: e.path,
                is_dir: e.is_dir,
                size: e.size,
                mtime: e
                    .mtime
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64),
            })
            .collect(),
        suggested_dest: s(&dest),
        opens: s(&opens),
    })
}

#[tauri::command]
fn is_archive(path: String) -> bool {
    let p = Path::new(&path);
    p.is_file() && czip_core::looks_like_archive(p)
}

#[tauri::command]
async fn pick_files(app: AppHandle) -> Vec<String> {
    app.dialog()
        .file()
        .set_title("Pilih file")
        .blocking_pick_files()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|f| f.into_path().ok().map(|p| s(&p)))
        .collect()
}

#[tauri::command]
async fn pick_folders(app: AppHandle) -> Vec<String> {
    app.dialog()
        .file()
        .set_title("Pilih folder")
        .blocking_pick_folders()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|f| f.into_path().ok().map(|p| s(&p)))
        .collect()
}

#[tauri::command]
async fn pick_dir(app: AppHandle, start: Option<String>) -> Option<String> {
    let mut d = app.dialog().file().set_title("Pilih lokasi");
    if let Some(start) = start.filter(|s| Path::new(s).is_dir()) {
        d = d.set_directory(start);
    }
    d.blocking_pick_folder()
        .and_then(|f| f.into_path().ok())
        .map(|p| s(&p))
}

#[tauri::command]
async fn pick_archive(app: AppHandle) -> Option<String> {
    app.dialog()
        .file()
        .set_title("Buka arsip")
        .add_filter(
            "Arsip",
            &[
                "czip", "zip", "7z", "zst", "tzst", "gz", "tgz", "xz", "txz", "tar",
            ],
        )
        .blocking_pick_file()
        .and_then(|f| f.into_path().ok())
        .map(|p| s(&p))
}

#[tauri::command]
fn reveal(app: AppHandle, path: String) -> CmdResult<()> {
    app.opener()
        .reveal_item_in_dir(&path)
        .map_err(|e| AppError::new("io", e.to_string()))
}

#[tauri::command]
fn open_folder(app: AppHandle, path: String) -> CmdResult<()> {
    app.opener()
        .open_path(&path, None::<&str>)
        .map_err(|e| AppError::new("io", e.to_string()))
}

/// Paths the app was launched with (e.g. double-clicking an archive).
#[tauri::command]
fn launch_paths() -> Vec<String> {
    std::env::args()
        .skip(1)
        .filter(|a| Path::new(a).exists())
        .collect()
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(Jobs::default())
        .invoke_handler(tauri::generate_handler![
            scan_paths,
            default_output_dir,
            resolve_output,
            start_compress,
            start_extract,
            cancel_job,
            list_archive,
            is_archive,
            pick_files,
            pick_folders,
            pick_dir,
            pick_archive,
            reveal,
            open_folder,
            launch_paths,
        ])
        .run(tauri::generate_context!())
        .expect("error while running czip");
}
