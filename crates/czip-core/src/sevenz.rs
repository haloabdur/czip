use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read};
use std::path::{Path, PathBuf};

use sevenz_rust2::encoder_options::{AesEncoderOptions, Lzma2Options};
use sevenz_rust2::{ArchiveEntry, ArchiveReader, ArchiveWriter, EncoderConfiguration, Password, SourceReader};

use crate::fsutil::{apply_meta, create_file, safe_join, selected};
use crate::progress::Counting;
use crate::scan::Scan;
use crate::{ArchiveInfo, Error, Level, ListEntry, Progress, Result};

/// Opens its file on first read, so a solid block over thousands of files
/// does not hold thousands of descriptors open.
struct LazyFile<'p> {
    path: PathBuf,
    file: Option<BufReader<File>>,
    progress: &'p Progress,
}

impl Read for LazyFile<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.file.is_none() {
            self.progress.set_current(&self.path.to_string_lossy());
            self.file = Some(BufReader::with_capacity(1 << 20, File::open(&self.path)?));
        }
        let mut r = Counting::new(self.file.as_mut().expect("opened above"), self.progress);
        let n = r.read(buf)?;
        if n == 0 {
            self.file = None;
            self.progress.file_done();
        }
        Ok(n)
    }
}

pub(crate) fn write(
    scan: &Scan,
    out: File,
    level: Level,
    password: Option<&str>,
    threads: usize,
    progress: &Progress,
) -> Result<u64> {
    let preset = match level {
        Level::Fast => 1,
        Level::Balanced => 5,
        Level::Max => 9,
    };
    let mut w = ArchiveWriter::new(out)?;
    let lzma: EncoderConfiguration = Lzma2Options::from_level_mt(preset, threads as u32, 64 << 20).into();
    match password {
        Some(pw) => {
            w.set_content_methods(vec![AesEncoderOptions::new(Password::new(pw)).into(), lzma]);
        }
        None => {
            w.set_content_methods(vec![lzma]);
        }
    }

    let mut entries = Vec::new();
    let mut readers = Vec::new();
    for item in &scan.items {
        if item.is_dir {
            w.push_archive_entry::<File>(ArchiveEntry::from_path(&item.path, item.name.clone()), None)?;
        } else {
            entries.push(ArchiveEntry::from_path(&item.path, item.name.clone()));
            readers.push(SourceReader::new(LazyFile { path: item.path.clone(), file: None, progress }));
        }
    }
    if !entries.is_empty() {
        w.push_archive_entries(entries, readers)?;
    }
    let f = w.finish()?;
    Ok(f.metadata()?.len())
}

fn reader(path: &Path, password: Option<&str>) -> Result<ArchiveReader<BufReader<File>>> {
    let f = BufReader::new(File::open(path).map_err(Error::at(path))?);
    let pw = password.map(Password::new).unwrap_or_else(Password::empty);
    Ok(ArchiveReader::new(f, pw)?)
}

pub(crate) fn info(path: &Path, password: Option<&str>) -> Result<ArchiveInfo> {
    let r = reader(path, password)?;
    let entries: Vec<ListEntry> = r
        .archive()
        .files
        .iter()
        .map(|e| ListEntry {
            path: e.name().trim_end_matches('/').to_string(),
            is_dir: e.is_directory,
            size: e.size,
            mtime: e.has_last_modified_date.then(|| e.last_modified_date.into()),
        })
        .collect();
    Ok(ArchiveInfo {
        format: "7z",
        encrypted: password.is_some_and(|p| !p.is_empty()),
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
    let mut r = reader(path, password)?;
    let total = r
        .archive()
        .files
        .iter()
        .filter(|e| selected(only, e.name()))
        .map(|e| e.size)
        .sum();
    progress.start(total);
    let (mut files, mut dirs, mut bytes) = (0u64, 0u64, 0u64);
    let mut failure: Option<Error> = None;
    let mut dir_meta = Vec::new();
    r.for_each_entries(|entry, data| {
        if progress.is_cancelled() {
            failure = Some(Error::Cancelled);
            return Ok(false);
        }
        let name = entry.name().trim_end_matches('/');
        if name.is_empty() || !selected(only, name) {
            io::copy(data, &mut io::sink())?;
            return Ok(true);
        }
        let mut step = || -> Result<()> {
            let out = safe_join(dest, name)?;
            let mtime = entry.has_last_modified_date.then(|| entry.last_modified_date.into());
            if entry.is_directory {
                std::fs::create_dir_all(&out).map_err(Error::at(&out))?;
                dir_meta.push((out, mtime));
                dirs += 1;
                return Ok(());
            }
            progress.set_current(name);
            let mut w = BufWriter::with_capacity(1 << 20, create_file(&out)?);
            let n = crate::zipfmt::copy_counting(data, &mut w, progress).map_err(Error::at(&out))?;
            drop(w);
            apply_meta(&out, mtime, 0);
            files += 1;
            bytes += n;
            progress.file_done();
            Ok(())
        };
        match step() {
            Ok(()) => Ok(true),
            Err(e) => {
                failure = Some(e);
                Ok(false)
            }
        }
    })?;
    if let Some(e) = failure {
        return Err(e);
    }
    for (p, t) in dir_meta.iter().rev() {
        apply_meta(p, *t, 0);
    }
    Ok((files, dirs, bytes))
}
