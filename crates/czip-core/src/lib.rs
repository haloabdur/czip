//! czip engine: compress any mix of files and folders, open common archives.
//!
//! ```no_run
//! use czip_core::{compress, CompressOptions, Format, Level, Progress};
//! let inputs = vec!["Photos".into(), "report.docx".into()];
//! let opts = CompressOptions { format: Format::Czip, level: Level::Balanced, ..Default::default() };
//! let summary = compress(&inputs, "backup.czip".as_ref(), &opts, &Progress::new()).unwrap();
//! println!("{} -> {} bytes", summary.input_bytes, summary.output_bytes);
//! ```

mod crypto;
mod error;
mod fsutil;
mod native;
mod progress;
mod scan;
mod sevenz;
mod tarfmt;
mod zipfmt;

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

pub use error::{Error, Result};
pub use progress::{Progress, Snapshot};
pub use scan::{scan, RootSummary, Scan, SourceItem};

use tarfmt::Codec;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// Native format: parallel zstd blocks, dedup, integrity hashes, optional encryption.
    #[default]
    Czip,
    /// Opens everywhere (Deflate, optional AES-256).
    Zip,
    /// 7-Zip compatible (LZMA2, optional AES-256).
    SevenZ,
    /// tar + multithreaded zstd, common on Linux and macOS. No encryption.
    TarZst,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::Czip => "czip",
            Format::Zip => "zip",
            Format::SevenZ => "7z",
            Format::TarZst => "tar.zst",
        }
    }

    pub fn supports_password(self) -> bool {
        self != Format::TarZst
    }

    pub fn from_name(s: &str) -> Option<Self> {
        match s.trim_start_matches('.').to_ascii_lowercase().as_str() {
            "czip" => Some(Format::Czip),
            "zip" => Some(Format::Zip),
            "7z" => Some(Format::SevenZ),
            "tar.zst" | "tzst" | "tarzst" => Some(Format::TarZst),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Level {
    Fast,
    #[default]
    Balanced,
    Max,
}

impl Level {
    pub fn from_name(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "fast" | "cepat" => Some(Level::Fast),
            "balanced" | "seimbang" | "normal" => Some(Level::Balanced),
            "max" | "maksimal" | "ultra" => Some(Level::Max),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CompressOptions {
    pub format: Format,
    pub level: Level,
    pub password: Option<String>,
    /// Worker threads; 0 uses every core.
    pub threads: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ExtractOptions {
    pub password: Option<String>,
    /// Only these archive paths (files, or folders with everything inside).
    pub only: Option<Vec<String>>,
    pub threads: usize,
}

#[derive(Debug, Clone)]
pub struct CompressSummary {
    pub output: PathBuf,
    pub files: u64,
    pub dirs: u64,
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub elapsed: Duration,
    pub skipped: Vec<PathBuf>,
}

impl CompressSummary {
    /// Fraction of space saved, 0.0..=1.0.
    pub fn saved(&self) -> f64 {
        if self.input_bytes == 0 {
            0.0
        } else {
            (1.0 - self.output_bytes as f64 / self.input_bytes as f64).max(0.0)
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExtractSummary {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub elapsed: Duration,
}

#[derive(Debug, Clone)]
pub struct ListEntry {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: Option<SystemTime>,
}

#[derive(Debug, Clone)]
pub struct ArchiveInfo {
    pub format: &'static str,
    pub encrypted: bool,
    pub unpacked_bytes: u64,
    pub entries: Vec<ListEntry>,
}

fn threads(n: usize) -> usize {
    if n > 0 {
        n
    } else {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
    }
}

/// Compresses `inputs` (files and folders, any mix) into `output`.
///
/// Writes to a temporary file next to `output` and renames it into place only
/// on success, so a failed or cancelled run never leaves half an archive.
pub fn compress(
    inputs: &[PathBuf],
    output: &Path,
    opts: &CompressOptions,
    progress: &Progress,
) -> Result<CompressSummary> {
    let started = Instant::now();
    let password = opts.password.as_deref().filter(|p| !p.is_empty());
    if password.is_some() && !opts.format.supports_password() {
        return Err(Error::Unsupported(format!(
            "{} cannot be password protected",
            opts.format.extension()
        )));
    }

    let mut scan = scan::scan(inputs)?;
    if let Ok(out) = std::path::absolute(output) {
        scan.items.retain(|i| std::path::absolute(&i.path).map(|p| p != out).unwrap_or(true));
    }
    if scan.items.is_empty() {
        return Err(Error::Empty);
    }
    progress.start(scan.total_bytes);

    let tmp = temp_path(output);
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(Error::at(parent))?;
    }
    let file = File::create(&tmp).map_err(Error::at(&tmp))?;
    let n = threads(opts.threads);
    let result = match opts.format {
        Format::Czip => native::write(&scan, file, opts.level, password, n, progress),
        Format::Zip => zipfmt::write(&scan, file, opts.level, password, progress),
        Format::SevenZ => sevenz::write(&scan, file, opts.level, password, n, progress),
        Format::TarZst => tarfmt::write(&scan, file, opts.level, n, progress),
    };
    let output_bytes = match progress.settle(result) {
        Ok(n) => n,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    std::fs::rename(&tmp, output).map_err(Error::at(output))?;
    Ok(CompressSummary {
        output: output.to_path_buf(),
        files: scan.files,
        dirs: scan.dirs,
        input_bytes: scan.total_bytes,
        output_bytes,
        elapsed: started.elapsed(),
        skipped: scan.skipped,
    })
}

fn temp_path(output: &Path) -> PathBuf {
    let mut name = output.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".partial");
    output.with_file_name(name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Czip,
    Zip,
    SevenZ,
    Tar(Codec),
    Single(Codec),
}

fn detect(path: &Path) -> Result<Kind> {
    let mut head = [0u8; 512];
    let mut f = File::open(path).map_err(Error::at(path))?;
    let mut n = 0;
    while n < head.len() {
        match f.read(&mut head[n..]).map_err(Error::at(path))? {
            0 => break,
            k => n += k,
        }
    }
    let h = &head[..n];
    let codec = if h.starts_with(b"CZIP") {
        return Ok(Kind::Czip);
    } else if h.starts_with(b"PK\x03\x04") || h.starts_with(b"PK\x05\x06") {
        return Ok(Kind::Zip);
    } else if h.starts_with(b"7z\xBC\xAF\x27\x1C") {
        return Ok(Kind::SevenZ);
    } else if h.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        Codec::Zstd
    } else if h.starts_with(&[0x1F, 0x8B]) {
        Codec::Gzip
    } else if h.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0]) {
        Codec::Xz
    } else if n == 512 && &h[257..262] == b"ustar" {
        return Ok(Kind::Tar(Codec::None));
    } else {
        return Err(Error::UnknownFormat);
    };
    Ok(if tarfmt::is_tar(path, codec)? { Kind::Tar(codec) } else { Kind::Single(codec) })
}

/// Lists an archive's contents without extracting.
///
/// For `.czip` only the index is read. A password is needed for encrypted
/// `.czip` files and 7z archives with encrypted headers.
pub fn list(archive: &Path, password: Option<&str>) -> Result<ArchiveInfo> {
    match detect(archive)? {
        Kind::Czip => Ok(native::Archive::open(archive, password)?.info()),
        Kind::Zip => zipfmt::info(archive),
        Kind::SevenZ => sevenz::info(archive, password),
        Kind::Tar(c) => tarfmt::info(archive, c),
        Kind::Single(c) => Ok(tarfmt::single_info(archive, c)),
    }
}

/// Extracts `archive` into `dest` (created if missing).
pub fn extract(
    archive: &Path,
    dest: &Path,
    opts: &ExtractOptions,
    progress: &Progress,
) -> Result<ExtractSummary> {
    let started = Instant::now();
    std::fs::create_dir_all(dest).map_err(Error::at(dest))?;
    let password = opts.password.as_deref().filter(|p| !p.is_empty());
    let only = opts.only.as_deref();
    let result = match detect(archive)? {
        Kind::Czip => native::Archive::open(archive, password)
            .and_then(|a| a.extract(dest, only, threads(opts.threads), progress)),
        Kind::Zip => zipfmt::extract(archive, dest, password, only, progress),
        Kind::SevenZ => sevenz::extract(archive, dest, password, only, progress),
        Kind::Tar(c) => tarfmt::extract(archive, dest, c, only, progress),
        Kind::Single(c) => tarfmt::extract_single(archive, dest, c, progress),
    };
    let (files, dirs, bytes) = progress.settle(result)?;
    Ok(ExtractSummary { files, dirs, bytes, elapsed: started.elapsed() })
}

/// Default archive path for a set of inputs: `<first input>.<ext>` beside
/// it, or `Archive.<ext>` for several inputs, never overwriting a file.
pub fn suggest_output(inputs: &[PathBuf], format: Format) -> PathBuf {
    let first = inputs.first().cloned().unwrap_or_else(|| PathBuf::from("."));
    let dir = first.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = if inputs.len() == 1 {
        let name = first.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if first.is_file() {
            name.split_once('.').map(|(s, _)| s.to_string()).filter(|s| !s.is_empty()).unwrap_or(name)
        } else {
            name
        }
    } else {
        "Archive".to_string()
    };
    let stem = if stem.is_empty() { "Archive".to_string() } else { stem };
    let ext = format.extension();
    let mut candidate = dir.join(format!("{stem}.{ext}"));
    let mut n = 2;
    while candidate.exists() {
        candidate = dir.join(format!("{stem} ({n}).{ext}"));
        n += 1;
    }
    candidate
}
