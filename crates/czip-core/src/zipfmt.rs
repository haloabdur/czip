use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read};
use std::path::Path;

use zip::write::SimpleFileOptions;
use zip::{AesMode, CompressionMethod, DateTime, ZipArchive, ZipWriter};

use crate::fsutil::{apply_meta, civil, create_file, from_civil, safe_join, selected};
use crate::progress::Counting;
use crate::scan::{ext_of, Scan};
use crate::{ArchiveInfo, Error, Level, ListEntry, Progress, Result};

/// Already-compressed types are stored rather than deflated again.
const STORED_EXT: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "heic", "avif", "mp4", "mov", "mkv", "webm", "mp3", "m4a",
    "aac", "ogg", "opus", "flac", "zip", "gz", "xz", "zst", "7z", "rar", "czip", "docx", "xlsx",
    "pptx",
];

pub(crate) fn write(
    scan: &Scan,
    out: File,
    level: Level,
    password: Option<&str>,
    progress: &Progress,
) -> Result<u64> {
    let deflate = match level {
        Level::Fast => 1,
        Level::Balanced => 6,
        Level::Max => 9,
    };
    let mut zip = ZipWriter::new(BufWriter::with_capacity(1 << 20, out));
    for item in &scan.items {
        progress.check()?;
        progress.set_current(&item.name);
        let mut opts = SimpleFileOptions::default()
            .unix_permissions(if item.mode == 0 {
                if item.is_dir {
                    0o755
                } else {
                    0o644
                }
            } else {
                item.mode
            })
            .large_file(item.size >= u32::MAX as u64 / 2);
        if let Some(t) = item.mtime.and_then(to_zip_time) {
            opts = opts.last_modified_time(t);
        }
        if item.is_dir {
            zip.add_directory(format!("{}/", item.name), opts)?;
            continue;
        }
        opts = if STORED_EXT.contains(&ext_of(&item.name).as_str()) {
            opts.compression_method(CompressionMethod::Stored)
        } else {
            opts.compression_method(CompressionMethod::Deflated)
                .compression_level(Some(deflate))
        };
        let file = File::open(&item.path).map_err(Error::at(&item.path))?;
        let mut r = Counting::new(BufReader::with_capacity(1 << 20, file), progress);
        match password {
            Some(pw) => zip.start_file(
                item.name.as_str(),
                opts.with_aes_encryption(AesMode::Aes256, pw),
            )?,
            None => zip.start_file(item.name.as_str(), opts)?,
        }
        io::copy(&mut r, &mut zip).map_err(Error::at(&item.path))?;
        progress.file_done();
    }
    let w = zip.finish()?;
    let f = w.into_inner().map_err(|e| Error::Io(e.into_error()))?;
    Ok(f.metadata()?.len())
}

fn to_zip_time(t: std::time::SystemTime) -> Option<DateTime> {
    let (y, m, d, hh, mm, ss) = civil(t);
    DateTime::from_date_and_time(u16::try_from(y).ok()?, m, d, hh, mm, ss).ok()
}

fn open(path: &Path) -> Result<ZipArchive<BufReader<File>>> {
    let f = File::open(path).map_err(Error::at(path))?;
    Ok(ZipArchive::new(BufReader::new(f))?)
}

pub(crate) fn info(path: &Path) -> Result<ArchiveInfo> {
    let mut z = open(path)?;
    let mut entries = Vec::with_capacity(z.len());
    let mut encrypted = false;
    for i in 0..z.len() {
        let f = z.by_index_raw(i)?;
        encrypted |= f.encrypted();
        let name = f.name()?.trim_end_matches('/').to_string();
        entries.push(ListEntry {
            path: name,
            is_dir: f.is_dir(),
            size: f.size(),
            mtime: f.last_modified().and_then(|t| {
                from_civil(
                    i64::from(t.year()),
                    t.month(),
                    t.day(),
                    t.hour(),
                    t.minute(),
                    t.second(),
                )
            }),
        });
    }
    Ok(ArchiveInfo {
        format: "zip",
        encrypted,
        unpacked_bytes: entries.iter().map(|e| e.size).sum(),
        entries,
    })
}

pub(crate) fn extract(
    path: &Path,
    dest: &Path,
    password: Option<&str>,
    only: Option<&[String]>,
    progress: &Progress,
) -> Result<(u64, u64, u64)> {
    let info = info(path)?;
    let total = info
        .entries
        .iter()
        .filter(|e| selected(only, &e.path))
        .map(|e| e.size)
        .sum();
    progress.start(total);
    let mut z = open(path)?;
    let (mut files, mut dirs, mut bytes) = (0, 0, 0);
    for i in 0..z.len() {
        progress.check()?;
        let encrypted = z.by_index_raw(i)?.encrypted();
        let mut f = if encrypted {
            let pw = password
                .filter(|p| !p.is_empty())
                .ok_or(Error::PasswordRequired)?;
            z.by_index_decrypt(i, pw.as_bytes())?
        } else {
            z.by_index(i)?
        };
        let name = f.name()?.trim_end_matches('/').to_string();
        if name.is_empty() || !selected(only, &name) {
            continue;
        }
        let out = safe_join(dest, &name)?;
        let mtime = f.last_modified().and_then(|t| {
            from_civil(
                i64::from(t.year()),
                t.month(),
                t.day(),
                t.hour(),
                t.minute(),
                t.second(),
            )
        });
        let mode = f.unix_mode().unwrap_or(0) & 0o7777;
        if f.is_dir() {
            std::fs::create_dir_all(&out).map_err(Error::at(&out))?;
            dirs += 1;
            continue;
        }
        progress.set_current(&name);
        let mut w = BufWriter::with_capacity(1 << 20, create_file(&out)?);
        let n = copy_counting(&mut f, &mut w, progress).map_err(|e| match e.kind() {
            io::ErrorKind::InvalidData if encrypted => Error::WrongPassword,
            _ => Error::IoPath {
                path: out.clone(),
                source: e,
            },
        })?;
        drop(w);
        apply_meta(&out, mtime, mode);
        files += 1;
        bytes += n;
        progress.file_done();
    }
    Ok((files, dirs, bytes))
}

pub(crate) fn copy_counting<R: Read + ?Sized>(
    r: &mut R,
    w: &mut impl io::Write,
    progress: &Progress,
) -> io::Result<u64> {
    let mut r = Counting::new(r, progress);
    let n = io::copy(&mut r, w)?;
    w.flush()?;
    Ok(n)
}
