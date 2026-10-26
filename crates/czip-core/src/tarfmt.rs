use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read};
use std::path::Path;

use crate::fsutil::{create_file, from_unix, safe_join, selected, to_unix};
use crate::progress::Counting;
use crate::scan::Scan;
use crate::{ArchiveInfo, Error, Level, ListEntry, Progress, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Codec {
    None,
    Zstd,
    Gzip,
    Xz,
}

impl Codec {
    fn name(self) -> &'static str {
        match self {
            Codec::None => "tar",
            Codec::Zstd => "tar.zst",
            Codec::Gzip => "tar.gz",
            Codec::Xz => "tar.xz",
        }
    }
}

pub(crate) fn decoder<'a>(codec: Codec, r: impl Read + 'a) -> Result<Box<dyn Read + 'a>> {
    Ok(match codec {
        Codec::None => Box::new(r),
        Codec::Zstd => {
            let mut d = zstd::stream::read::Decoder::new(r)?;
            d.window_log_max(31)?;
            Box::new(d)
        }
        Codec::Gzip => Box::new(flate2::read::MultiGzDecoder::new(BufReader::new(r))),
        Codec::Xz => Box::new(lzma_rust2::XzReader::new(r, true)),
    })
}

/// Whether the decompressed stream starts with a tar header.
pub(crate) fn is_tar(path: &Path, codec: Codec) -> Result<bool> {
    let f = File::open(path).map_err(Error::at(path))?;
    let mut d = decoder(codec, BufReader::new(f))?;
    let mut head = [0u8; 512];
    let mut n = 0;
    while n < head.len() {
        match d.read(&mut head[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(_) => return Ok(false),
        }
    }
    Ok(n == 512 && &head[257..262] == b"ustar")
}

pub(crate) fn write(
    scan: &Scan,
    out: File,
    level: Level,
    threads: usize,
    progress: &Progress,
) -> Result<u64> {
    let lvl = match level {
        Level::Fast => 3,
        Level::Balanced => 9,
        Level::Max => 19,
    };
    let mut enc = zstd::stream::write::Encoder::new(BufWriter::with_capacity(1 << 20, out), lvl)?;
    enc.multithread(threads as u32)?;
    if level == Level::Max {
        enc.long_distance_matching(true)?;
        enc.window_log(27)?;
    }
    let mut b = tar::Builder::new(enc);
    for item in &scan.items {
        progress.check()?;
        progress.set_current(&item.name);
        let mut h = tar::Header::new_gnu();
        let (secs, _) = to_unix(item.mtime);
        h.set_mtime(secs.max(0) as u64);
        let mode = if item.mode == 0 {
            if item.is_dir {
                0o755
            } else {
                0o644
            }
        } else {
            item.mode
        };
        h.set_mode(mode);
        if item.is_dir {
            h.set_entry_type(tar::EntryType::Directory);
            h.set_size(0);
            b.append_data(&mut h, format!("{}/", item.name), io::empty())?;
        } else {
            h.set_entry_type(tar::EntryType::Regular);
            h.set_size(item.size);
            let f = File::open(&item.path).map_err(Error::at(&item.path))?;
            let r = Counting::new(BufReader::with_capacity(1 << 20, f), progress);
            b.append_data(&mut h, &item.name, r.take(item.size))
                .map_err(Error::at(&item.path))?;
            progress.file_done();
        }
    }
    let enc = b.into_inner()?;
    let w = enc.finish()?;
    let f = w.into_inner().map_err(|e| Error::Io(e.into_error()))?;
    Ok(f.metadata()?.len())
}

fn entry_name(e: &tar::Entry<'_, impl Read>) -> Result<String> {
    let p = e.path().map_err(|err| Error::Corrupt(err.to_string()))?;
    let s = p.to_string_lossy().replace('\\', "/");
    Ok(s.trim_start_matches("./").trim_end_matches('/').to_string())
}

pub(crate) fn info(path: &Path, codec: Codec) -> Result<ArchiveInfo> {
    let f = File::open(path).map_err(Error::at(path))?;
    let mut ar = tar::Archive::new(decoder(codec, BufReader::new(f))?);
    let mut entries = Vec::new();
    for e in ar.entries()? {
        let e = e?;
        let kind = e.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            continue;
        }
        let name = entry_name(&e)?;
        if name.is_empty() {
            continue;
        }
        entries.push(ListEntry {
            path: name,
            is_dir: kind.is_dir(),
            size: e.header().size().unwrap_or(0),
            mtime: e.header().mtime().ok().and_then(|t| from_unix(t as i64, 0)),
        });
    }
    Ok(ArchiveInfo {
        format: codec.name(),
        encrypted: false,
        unpacked_bytes: entries.iter().map(|e| e.size).sum(),
        entries,
    })
}

pub(crate) fn extract(
    path: &Path,
    dest: &Path,
    codec: Codec,
    only: Option<&[String]>,
    progress: &Progress,
) -> Result<(u64, u64, u64)> {
    let f = File::open(path).map_err(Error::at(path))?;
    progress.start(f.metadata()?.len());
    let input = Counting::new(BufReader::with_capacity(1 << 20, f), progress);
    let mut ar = tar::Archive::new(decoder(codec, input)?);
    ar.set_preserve_mtime(true);
    ar.set_preserve_permissions(true);
    ar.set_overwrite(true);
    std::fs::create_dir_all(dest).map_err(Error::at(dest))?;
    let (mut files, mut dirs, mut bytes) = (0, 0, 0);
    for e in ar.entries()? {
        progress.check()?;
        let mut e = e?;
        let kind = e.header().entry_type();
        // Links are skipped: a symlink followed by a file "inside" it is the
        // classic way to write outside the destination.
        if !(kind.is_file() || kind.is_dir()) {
            continue;
        }
        let name = entry_name(&e)?;
        if name.is_empty() || !selected(only, &name) {
            continue;
        }
        safe_join(dest, &name)?;
        progress.set_current(&name);
        if !e.unpack_in(dest)? {
            return Err(Error::UnsafePath(name));
        }
        if kind.is_dir() {
            dirs += 1;
        } else {
            files += 1;
            bytes += e.header().size().unwrap_or(0);
            progress.file_done();
        }
    }
    Ok((files, dirs, bytes))
}

/// A lone `.gz` / `.xz` / `.zst` file: decompress it next to its name.
pub(crate) fn extract_single(
    path: &Path,
    dest: &Path,
    codec: Codec,
    progress: &Progress,
) -> Result<(u64, u64, u64)> {
    let f = File::open(path).map_err(Error::at(path))?;
    progress.start(f.metadata()?.len());
    let name = single_name(path);
    let out = safe_join(dest, &name)?;
    progress.set_current(&name);
    let mut d = decoder(
        codec,
        Counting::new(BufReader::with_capacity(1 << 20, f), progress),
    )?;
    let mut w = BufWriter::with_capacity(1 << 20, create_file(&out)?);
    let n = io::copy(&mut d, &mut w).map_err(Error::at(&out))?;
    io::Write::flush(&mut w).map_err(Error::at(&out))?;
    progress.file_done();
    Ok((1, 0, n))
}

pub(crate) fn single_info(path: &Path, codec: Codec) -> ArchiveInfo {
    let format = match codec {
        Codec::Zstd => "zst",
        Codec::Gzip => "gz",
        Codec::Xz => "xz",
        Codec::None => "raw",
    };
    ArchiveInfo {
        format,
        encrypted: false,
        unpacked_bytes: 0,
        entries: vec![ListEntry {
            path: single_name(path),
            is_dir: false,
            size: 0,
            mtime: None,
        }],
    }
}

fn single_name(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if stem.is_empty() {
        "output".into()
    } else {
        stem
    }
}
