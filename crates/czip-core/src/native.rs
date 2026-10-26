//! The native `.czip` container.
//!
//! ```text
//! header   "CZIP" | version u16 | flags u16 | [salt 16 | nonce prefix 16 | m,t,p u32]
//! blocks   compressed or stored chunks (sealed when a password is set)
//! index    zstd(block table + entries), sealed with counter u64::MAX
//! footer   index offset u64 | index length u64 | "CZIPEND!"
//! ```
//!
//! File data goes into two solid streams. Compressible content streams into
//! multithreaded zstd as files are read, so reading and compressing overlap
//! and every core is busy; the stream is cut into segments of 128 MiB so one
//! file can be pulled out without decoding the whole archive, and segments
//! decode in parallel. Content that is already compressed (photos, video,
//! archives) is stored as-is instead of burning CPU on it.
//!
//! Files record the block extents holding their bytes, identical files are
//! stored once, and every file carries a 64-bit BLAKE3 checksum verified on
//! extraction (with a password, AEAD also authenticates every block).

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;
use std::sync::Arc;

use rayon::prelude::*;

use crate::crypto::{self, Cipher, PREFIX_LEN, SALT_LEN};
use crate::fsutil::{apply_meta, create_file, from_unix, read_at, safe_join, selected, to_unix};
use crate::progress::Counting;
use crate::scan::{Scan, SourceItem};
use crate::{ArchiveInfo, Error, Level, ListEntry, Progress, Result};

const MAGIC: &[u8; 4] = b"CZIP";
const END_MAGIC: &[u8; 8] = b"CZIPEND!";
const VERSION: u16 = 1;
const FLAG_ENCRYPTED: u16 = 1;
const FOOTER_LEN: u64 = 24;
const INDEX_COUNTER: u64 = u64::MAX;

const M_STORE: u8 = 0;
/// One zstd frame.
const M_ZSTD: u8 = 1;

/// Raw bytes per compressed segment.
const SEGMENT: usize = 128 << 20;
/// Raw bytes per stored block.
const STORE_BLOCK: usize = 32 << 20;
const READ_CHUNK: usize = 1 << 20;
const SAMPLE: usize = 256 << 10;
/// Files up to this size are read whole, which lets duplicates be detected
/// before any of their bytes are written.
const DEDUP_MAX: u64 = 16 << 20;
const MAX_BLOCK_RAW: u64 = 1 << 30;
/// Stored checksum length: plenty to catch corruption, and it keeps the
/// index small, since random hash bytes do not compress.
const HASH_LEN: usize = 8;
const MAX_WINDOW_LOG: u32 = 27;
/// Segments decoded at once during extraction; each holds up to `SEGMENT`.
const DECODE_PARALLEL: usize = 4;

struct Params {
    level: i32,
    /// Long-distance matching with this window (log2), 0 for the default.
    long_window: u32,
}

fn params(level: Level) -> Params {
    match level {
        Level::Fast => Params { level: 3, long_window: 0 },
        Level::Balanced => Params { level: 9, long_window: 0 },
        // A 128 MiB window finds repeats far apart (similar files, vendored
        // copies), which is where most of the gain over level 9 comes from.
        Level::Max => Params { level: 19, long_window: MAX_WINDOW_LOG },
    }
}

/// With a long window zstd would otherwise make each job 4x the window and
/// run on one core; 32 MiB jobs overlapping by half a window keep every core
/// busy for about 0.1% in size.
const LONG_JOB_SIZE: u32 = 32 << 20;
const LONG_OVERLAP_LOG: u32 = 8;

#[derive(Debug, Clone, Copy)]
struct Extent {
    block: u32,
    offset: u32,
    len: u32,
}

#[derive(Debug, Clone)]
struct Entry {
    is_dir: bool,
    path: String,
    size: u64,
    /// Unix seconds.
    mtime: i64,
    mode: u32,
    hash: [u8; HASH_LEN],
    extents: Vec<Extent>,
    /// Same content as an earlier entry; its extents are copied at the end
    /// because that entry's blocks may not be numbered yet.
    dup_of: Option<usize>,
}

#[derive(Debug, Clone, Copy)]
struct BlockInfo {
    method: u8,
    offset: u64,
    stored: u64,
    raw: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Compress = 0,
    Store = 1,
}

/// Open segment or block: what has been appended and the extents waiting
/// for its block number.
#[derive(Default)]
struct Open {
    raw: usize,
    waiting: Vec<(usize, usize)>,
}

// ---------------------------------------------------------------------------
// Writing

pub(crate) fn write(
    scan: &Scan,
    out: File,
    level: Level,
    password: Option<&str>,
    threads: usize,
    progress: &Progress,
) -> Result<u64> {
    write_with(scan, out, level, password, threads, progress, [SEGMENT, STORE_BLOCK])
}

/// `limits`: raw bytes per compressed segment and per stored block.
fn write_with(
    scan: &Scan,
    out: File,
    level: Level,
    password: Option<&str>,
    threads: usize,
    progress: &Progress,
    limits: [usize; 2],
) -> Result<u64> {
    let mut out = BufWriter::with_capacity(1 << 20, out);
    let mut header = Vec::with_capacity(64);
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&VERSION.to_le_bytes());
    let cipher = match password {
        Some(pw) => {
            let salt: [u8; SALT_LEN] = crypto::random()?;
            let prefix: [u8; PREFIX_LEN] = crypto::random()?;
            header.extend_from_slice(&FLAG_ENCRYPTED.to_le_bytes());
            header.extend_from_slice(&salt);
            header.extend_from_slice(&prefix);
            for v in [crypto::KDF_M, crypto::KDF_T, crypto::KDF_P] {
                header.extend_from_slice(&v.to_le_bytes());
            }
            Some(Cipher::derive(pw, &salt, prefix, (crypto::KDF_M, crypto::KDF_T, crypto::KDF_P))?)
        }
        None => {
            header.extend_from_slice(&0u16.to_le_bytes());
            None
        }
    };
    out.write_all(&header)?;

    let mut w = Writer {
        pos: header.len() as u64,
        out,
        p: params(level),
        limits,
        threads: threads as u32,
        cipher,
        encoder: None,
        store_buf: Vec::new(),
        open: Default::default(),
        blocks: Vec::new(),
        entries: Vec::with_capacity(scan.items.len()),
        dedup: HashMap::new(),
        progress,
    };
    for item in &scan.items {
        progress.check()?;
        if item.is_dir {
            w.entries.push(Entry {
                is_dir: true,
                path: item.name.clone(),
                size: 0,
                mtime: to_unix(item.mtime).0,
                mode: item.mode,
                hash: [0; HASH_LEN],
                extents: Vec::new(),
                dup_of: None,
            });
        } else {
            w.add_file(item)?;
        }
    }
    w.finish()
}

type Encoder = zstd::stream::write::Encoder<'static, Vec<u8>>;

struct Writer<'a> {
    out: BufWriter<File>,
    pos: u64,
    p: Params,
    limits: [usize; 2],
    threads: u32,
    cipher: Option<Cipher>,
    /// Current compressed segment; zstd's workers compress while we read.
    encoder: Option<Encoder>,
    store_buf: Vec<u8>,
    open: [Open; 2],
    blocks: Vec<BlockInfo>,
    entries: Vec<Entry>,
    dedup: HashMap<[u8; 32], usize>,
    progress: &'a Progress,
}

impl Writer<'_> {
    fn add_file(&mut self, item: &SourceItem) -> Result<()> {
        self.progress.set_current(&item.name);
        let at = || Error::at(&item.path);
        let file = File::open(&item.path).map_err(at())?;
        let mut r = Counting::new(file, self.progress);

        let idx = self.entries.len();
        self.entries.push(Entry {
            is_dir: false,
            path: item.name.clone(),
            size: 0,
            mtime: to_unix(item.mtime).0,
            mode: item.mode,
            hash: [0; HASH_LEN],
            extents: Vec::new(),
            dup_of: None,
        });

        let mut head = vec![0u8; SAMPLE.min(item.size as usize)];
        let n = read_full(&mut r, &mut head).map_err(at())?;
        head.truncate(n);
        let class = classify(&head);

        if item.size <= DEDUP_MAX {
            let mut data = head;
            r.read_to_end(&mut data).map_err(at())?;
            let hash = *blake3::hash(&data).as_bytes();
            let e = &mut self.entries[idx];
            e.size = data.len() as u64;
            e.hash = short(&hash);
            match self.dedup.get(&hash) {
                Some(&orig) if !data.is_empty() => e.dup_of = Some(orig),
                _ => {
                    self.append(class, idx, &data)?;
                    if !data.is_empty() {
                        self.dedup.insert(hash, idx);
                    }
                }
            }
        } else {
            let mut hasher = blake3::Hasher::new();
            hasher.update(&head);
            self.append(class, idx, &head)?;
            let mut total = head.len() as u64;
            let mut buf = vec![0u8; READ_CHUNK];
            loop {
                let n = read_full(&mut r, &mut buf).map_err(at())?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                self.append(class, idx, &buf[..n])?;
                total += n as u64;
            }
            let e = &mut self.entries[idx];
            e.size = total;
            e.hash = short(hasher.finalize().as_bytes());
        }
        self.progress.file_done();
        Ok(())
    }

    fn append(&mut self, class: Class, idx: usize, mut data: &[u8]) -> Result<()> {
        let limit = self.limits[class as usize];
        while !data.is_empty() {
            let open = &mut self.open[class as usize];
            let take = (limit - open.raw).min(data.len());
            let off = open.raw as u32;

            let e = &mut self.entries[idx];
            let contiguous = matches!(open.waiting.last(),
                Some(&(ei, xi)) if ei == idx && e.extents[xi].offset + e.extents[xi].len == off);
            if contiguous {
                e.extents.last_mut().expect("waiting extent exists").len += take as u32;
            } else {
                e.extents.push(Extent { block: u32::MAX, offset: off, len: take as u32 });
                open.waiting.push((idx, e.extents.len() - 1));
            }
            open.raw += take;

            match class {
                Class::Compress => {
                    if self.encoder.is_none() {
                        self.encoder = Some(new_encoder(&self.p, self.threads)?);
                    }
                    self.encoder.as_mut().expect("created above").write_all(&data[..take])?;
                }
                Class::Store => self.store_buf.extend_from_slice(&data[..take]),
            }
            data = &data[take..];
            if self.open[class as usize].raw >= limit {
                self.seal(class)?;
            }
        }
        Ok(())
    }

    fn seal(&mut self, class: Class) -> Result<()> {
        let open = std::mem::take(&mut self.open[class as usize]);
        if open.raw == 0 {
            return Ok(());
        }
        let (method, data) = match class {
            Class::Compress => {
                let enc = self.encoder.take().expect("segment has data");
                (M_ZSTD, enc.finish()?)
            }
            Class::Store => (M_STORE, std::mem::take(&mut self.store_buf)),
        };
        let index = self.blocks.len() as u32;
        for (ei, xi) in open.waiting {
            self.entries[ei].extents[xi].block = index;
        }
        let data = match &self.cipher {
            Some(c) => c.seal(u64::from(index), &data)?,
            None => data,
        };
        self.out.write_all(&data)?;
        self.blocks.push(BlockInfo { method, offset: self.pos, stored: data.len() as u64, raw: open.raw as u64 });
        self.pos += data.len() as u64;
        Ok(())
    }

    fn finish(mut self) -> Result<u64> {
        self.seal(Class::Compress)?;
        self.seal(Class::Store)?;

        for i in 0..self.entries.len() {
            if let Some(orig) = self.entries[i].dup_of {
                self.entries[i].extents = self.entries[orig].extents.clone();
            }
        }

        let index = encode_index(&self.blocks, &self.entries);
        let mut index = zstd::bulk::compress(&index, 19)?;
        if let Some(c) = &self.cipher {
            index = c.seal(INDEX_COUNTER, &index)?;
        }
        let index_off = self.pos;
        self.out.write_all(&index)?;
        self.out.write_all(&index_off.to_le_bytes())?;
        self.out.write_all(&(index.len() as u64).to_le_bytes())?;
        self.out.write_all(END_MAGIC)?;
        self.out.flush()?;
        Ok(index_off + index.len() as u64 + FOOTER_LEN)
    }
}

fn new_encoder(p: &Params, threads: u32) -> Result<Encoder> {
    let mut e = zstd::stream::write::Encoder::new(Vec::with_capacity(SEGMENT / 4), p.level)?;
    e.multithread(threads)?;
    if p.long_window > 0 {
        e.long_distance_matching(true)?;
        e.window_log(p.long_window)?;
        e.set_parameter(zstd::stream::raw::CParameter::JobSize(LONG_JOB_SIZE))?;
        e.set_parameter(zstd::stream::raw::CParameter::OverlapSizeLog(LONG_OVERLAP_LOG))?;
    }
    Ok(e)
}

fn zstd_decompress(data: &[u8], raw_len: usize) -> io::Result<Vec<u8>> {
    let mut d = zstd::stream::read::Decoder::with_buffer(data)?;
    d.window_log_max(MAX_WINDOW_LOG)?;
    let mut out = Vec::with_capacity(raw_len);
    d.take(raw_len as u64 + 1).read_to_end(&mut out)?;
    Ok(out)
}

/// Decides from the content itself whether a file is worth compressing.
/// Extensions lie: plenty of `.7z`, `.zip` or `.pdf` files hold stored data
/// that compresses well, while unlabeled files can be pure noise.
fn classify(head: &[u8]) -> Class {
    if head.len() >= 16 << 10 {
        if let Ok(c) = zstd::bulk::compress(head, 1) {
            if c.len() * 100 > head.len() * 97 {
                return Class::Store;
            }
        }
    }
    Class::Compress
}

fn short(h: &[u8; 32]) -> [u8; HASH_LEN] {
    h[..HASH_LEN].try_into().expect("sized")
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

// ---------------------------------------------------------------------------
// Index encoding

const BLOCK_RECORD: usize = 1 + 8 * 3;

// The entry table is stored column by column (all kinds, then all paths,
// then all sizes, ...) with varints and front-coded paths: similar values sit
// together, so the index compresses several times better than row records.

fn put_var(b: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        b.push(v as u8 | 0x80);
        v >>= 7;
    }
    b.push(v as u8);
}

fn encode_index(blocks: &[BlockInfo], entries: &[Entry]) -> Vec<u8> {
    let mut b = Vec::with_capacity(16 + blocks.len() * BLOCK_RECORD + entries.len() * 48);
    b.extend_from_slice(&(blocks.len() as u32).to_le_bytes());
    for bl in blocks {
        b.push(bl.method);
        for v in [bl.offset, bl.stored, bl.raw] {
            b.extend_from_slice(&v.to_le_bytes());
        }
    }
    put_var(&mut b, entries.len() as u64);
    b.extend(entries.iter().map(|e| u8::from(e.is_dir)));
    let mut prev: &str = "";
    for e in entries {
        let shared = prev.bytes().zip(e.path.bytes()).take_while(|(a, b)| a == b).count();
        let shared = (0..=shared).rev().find(|&n| e.path.is_char_boundary(n)).unwrap_or(0);
        put_var(&mut b, shared as u64);
        put_var(&mut b, (e.path.len() - shared) as u64);
        b.extend_from_slice(&e.path.as_bytes()[shared..]);
        prev = &e.path;
    }
    for e in entries {
        put_var(&mut b, e.size);
    }
    let mut last = 0i64;
    for e in entries {
        // Zig-zag delta: files of one folder usually share similar times.
        put_var(&mut b, zigzag(e.mtime.wrapping_sub(last)));
        last = e.mtime;
    }
    for e in entries {
        put_var(&mut b, u64::from(e.mode));
    }
    for e in entries {
        b.extend_from_slice(&e.hash);
    }
    // Extents mostly continue where the previous one ended, so store the
    // difference from that guess; a lone extent's length is the file size.
    let (mut blk, mut end) = (0u32, 0u32);
    for e in entries {
        put_var(&mut b, e.extents.len() as u64);
        for x in &e.extents {
            let guess = if x.block == blk { end } else { 0 };
            put_var(&mut b, zigzag(i64::from(x.block) - i64::from(blk)));
            put_var(&mut b, zigzag(i64::from(x.offset) - i64::from(guess)));
            if e.extents.len() > 1 {
                put_var(&mut b, u64::from(x.len));
            }
            blk = x.block;
            end = x.offset + x.len;
        }
    }
    b
}

fn zigzag(d: i64) -> u64 {
    ((d << 1) ^ (d >> 63)) as u64
}

fn unzigzag(z: u64) -> i64 {
    ((z >> 1) as i64) ^ -((z & 1) as i64)
}

struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(Error::Corrupt("index is truncated".into()));
        }
        let (h, t) = self.0.split_at(n);
        self.0 = t;
        Ok(h)
    }
    fn arr<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().expect("length checked"))
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.arr()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.arr()?))
    }
    fn var(&mut self) -> Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = self.u8()?;
            v |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(Error::Corrupt("bad number in index".into()))
    }
    fn var32(&mut self) -> Result<u32> {
        u32::try_from(self.var()?).map_err(|_| Error::Corrupt("number out of range in index".into()))
    }
}

fn decode_index(data: &[u8], index_off: u64) -> Result<(Vec<BlockInfo>, Vec<Entry>)> {
    let bad = |m: &str| Error::Corrupt(m.to_string());
    let mut c = Cursor(data);
    let nblocks = c.u32()? as usize;
    if nblocks > data.len() / BLOCK_RECORD {
        return Err(bad("block count out of range"));
    }
    let mut blocks = Vec::with_capacity(nblocks);
    for _ in 0..nblocks {
        let b = BlockInfo { method: c.u8()?, offset: c.u64()?, stored: c.u64()?, raw: c.u64()? };
        let in_file = b.offset.checked_add(b.stored).is_some_and(|end| end <= index_off);
        if b.method > M_ZSTD || b.raw > MAX_BLOCK_RAW || !in_file {
            return Err(bad("block table entry out of range"));
        }
        blocks.push(b);
    }

    let n = c.var()? as usize;
    // Every entry takes at least a dozen bytes across the columns.
    if n > c.0.len() / 12 + 1 {
        return Err(bad("entry count out of range"));
    }
    let kinds = c.take(n)?;
    let mut paths: Vec<String> = Vec::with_capacity(n);
    let mut prev: Vec<u8> = Vec::new();
    for _ in 0..n {
        let shared = c.var()? as usize;
        let rest = c.var()? as usize;
        if shared > prev.len() {
            return Err(bad("path prefix out of range"));
        }
        prev.truncate(shared);
        prev.extend_from_slice(c.take(rest)?);
        paths.push(String::from_utf8(prev.clone()).map_err(|_| bad("path is not UTF-8"))?);
    }
    let sizes = (0..n).map(|_| c.var()).collect::<Result<Vec<_>>>()?;
    let mut last = 0i64;
    let mut mtimes = Vec::with_capacity(n);
    for _ in 0..n {
        let z = c.var()?;
        last = last.wrapping_add(unzigzag(z));
        mtimes.push(last);
    }
    let modes = (0..n).map(|_| c.var32()).collect::<Result<Vec<_>>>()?;
    let hashes = (0..n).map(|_| c.arr::<HASH_LEN>()).collect::<Result<Vec<_>>>()?;

    let mut entries = Vec::with_capacity(n);
    let (mut blk, mut end) = (0u32, 0u32);
    let num = |v: i64| u32::try_from(v).map_err(|_| bad("extent out of range"));
    for (i, path) in paths.into_iter().enumerate() {
        let is_dir = kinds[i] != 0;
        let nx = c.var()? as usize;
        if nx > c.0.len() / 2 {
            return Err(bad("extent count out of range"));
        }
        let mut extents = Vec::with_capacity(nx);
        let mut sum = 0u64;
        for _ in 0..nx {
            let block = num(i64::from(blk) + unzigzag(c.var()?))?;
            let guess = if block == blk { end } else { 0 };
            let offset = num(i64::from(guess) + unzigzag(c.var()?))?;
            let len = if nx > 1 { c.var32()? } else { num(sizes[i] as i64)? };
            let x = Extent { block, offset, len };
            blk = block;
            end = offset.saturating_add(len);
            let raw = blocks.get(x.block as usize).ok_or_else(|| bad("extent names a missing block"))?.raw;
            if u64::from(x.offset) + u64::from(x.len) > raw {
                return Err(bad("extent out of block range"));
            }
            sum += u64::from(x.len);
            extents.push(x);
        }
        if !is_dir && sum != sizes[i] {
            return Err(bad("file size does not match its data"));
        }
        entries.push(Entry {
            is_dir,
            path,
            size: sizes[i],
            mtime: mtimes[i],
            mode: modes[i],
            hash: hashes[i],
            extents,
            dup_of: None,
        });
    }
    Ok((blocks, entries))
}

// ---------------------------------------------------------------------------
// Reading

pub(crate) struct Archive {
    file: File,
    cipher: Option<Cipher>,
    blocks: Vec<BlockInfo>,
    entries: Vec<Entry>,
    encrypted: bool,
}

impl Archive {
    pub(crate) fn open(path: &Path, password: Option<&str>) -> Result<Self> {
        let at = || Error::at(path);
        let file = File::open(path).map_err(at())?;
        let len = file.metadata().map_err(at())?.len();
        let mut head = [0u8; 8];
        read_at(&file, 0, &mut head).map_err(|_| Error::UnknownFormat)?;
        if &head[..4] != MAGIC {
            return Err(Error::UnknownFormat);
        }
        let version = u16::from_le_bytes([head[4], head[5]]);
        if version > VERSION {
            return Err(Error::Unsupported(format!(
                "this archive needs a newer czip (format version {version})"
            )));
        }
        let flags = u16::from_le_bytes([head[6], head[7]]);
        let encrypted = flags & FLAG_ENCRYPTED != 0;
        let mut header_len = 8u64;
        let cipher = if encrypted {
            let mut h = [0u8; SALT_LEN + PREFIX_LEN + 12];
            read_at(&file, 8, &mut h).map_err(|_| Error::Corrupt("header is truncated".into()))?;
            header_len += h.len() as u64;
            let pw = password.filter(|p| !p.is_empty()).ok_or(Error::PasswordRequired)?;
            let salt: [u8; SALT_LEN] = h[..SALT_LEN].try_into().expect("sized");
            let prefix: [u8; PREFIX_LEN] = h[SALT_LEN..SALT_LEN + PREFIX_LEN].try_into().expect("sized");
            let k = &h[SALT_LEN + PREFIX_LEN..];
            let u = |i: usize| u32::from_le_bytes(k[i * 4..i * 4 + 4].try_into().expect("sized"));
            let (m, t, p) = (u(0), u(1), u(2));
            if m > 4 * 1024 * 1024 || t > 64 || p > 64 {
                return Err(Error::Corrupt("key parameters out of range".into()));
            }
            Some(Cipher::derive(pw, &salt, prefix, (m, t, p))?)
        } else {
            None
        };

        if len < header_len + FOOTER_LEN {
            return Err(Error::Corrupt("file is truncated".into()));
        }
        let mut foot = [0u8; FOOTER_LEN as usize];
        read_at(&file, len - FOOTER_LEN, &mut foot).map_err(at())?;
        if &foot[16..] != END_MAGIC {
            return Err(Error::Corrupt("missing end marker (incomplete download?)".into()));
        }
        let index_off = u64::from_le_bytes(foot[..8].try_into().expect("sized"));
        let index_len = u64::from_le_bytes(foot[8..16].try_into().expect("sized"));
        if index_off < header_len || index_off.checked_add(index_len) != Some(len - FOOTER_LEN) {
            return Err(Error::Corrupt("index location is invalid".into()));
        }
        let mut index = vec![0u8; index_len as usize];
        read_at(&file, index_off, &mut index).map_err(at())?;
        if let Some(c) = &cipher {
            index = c.open(INDEX_COUNTER, &index).ok_or(Error::WrongPassword)?;
        }
        let index = zstd::stream::decode_all(&index[..])
            .map_err(|_| Error::Corrupt("index does not decompress".into()))?;
        let (blocks, entries) = decode_index(&index, index_off)?;
        Ok(Self { file, cipher, blocks, entries, encrypted })
    }

    pub(crate) fn info(&self) -> ArchiveInfo {
        let entries: Vec<ListEntry> = self
            .entries
            .iter()
            .map(|e| ListEntry {
                path: e.path.clone(),
                is_dir: e.is_dir,
                size: e.size,
                mtime: from_unix(e.mtime, 0),
            })
            .collect();
        ArchiveInfo {
            format: "czip",
            encrypted: self.encrypted,
            unpacked_bytes: entries.iter().map(|e| e.size).sum(),
            entries,
        }
    }

    fn decode_block(&self, i: u32) -> Result<Vec<u8>> {
        let b = self.blocks[i as usize];
        let mut data = vec![0u8; b.stored as usize];
        read_at(&self.file, b.offset, &mut data)?;
        if let Some(c) = &self.cipher {
            data = c
                .open(u64::from(i), &data)
                .ok_or_else(|| Error::Corrupt(format!("block {i} failed authentication")))?;
        }
        let raw = match b.method {
            M_ZSTD => zstd_decompress(&data, b.raw as usize)
                .map_err(|e| Error::Corrupt(format!("block {i}: {e}")))?,
            _ => data,
        };
        if raw.len() as u64 != b.raw {
            return Err(Error::Corrupt(format!("block {i} has the wrong size")));
        }
        Ok(raw)
    }

    pub(crate) fn extract(
        &self,
        dest: &Path,
        only: Option<&[String]>,
        threads: usize,
        progress: &Progress,
    ) -> Result<(u64, u64, u64)> {
        let par = threads.clamp(1, DECODE_PARALLEL);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(par)
            .build()
            .map_err(|e| Error::Io(io::Error::other(e.to_string())))?;

        let mut dirs = Vec::new();
        let mut files = Vec::new();
        for e in self.entries.iter().filter(|e| selected(only, &e.path)) {
            let path = safe_join(dest, &e.path)?;
            if e.is_dir {
                std::fs::create_dir_all(&path).map_err(Error::at(&path))?;
                dirs.push((path, e));
            } else {
                files.push((path, e));
            }
        }
        files.sort_by_key(|(_, e)| e.extents.first().map(|x| (x.block, x.offset)));
        progress.start(files.iter().map(|(_, e)| e.size).sum());

        // Order in which blocks are consumed, and the last point each is needed.
        let mut order: Vec<u32> = Vec::new();
        for (_, e) in &files {
            for x in &e.extents {
                if order.last() != Some(&x.block) {
                    order.push(x.block);
                }
            }
        }
        let mut last_use: HashMap<u32, usize> = HashMap::new();
        for (i, b) in order.iter().enumerate() {
            last_use.insert(*b, i);
        }

        let mut cache: HashMap<u32, Arc<Vec<u8>>> = HashMap::new();
        let mut pos = 0usize;
        let mut bytes = 0u64;
        for (path, e) in &files {
            progress.check()?;
            progress.set_current(&e.path);
            // Extents are large slices already; no buffering needed.
            let mut out = create_file(path)?;
            let mut hasher = blake3::Hasher::new();
            for x in &e.extents {
                if order[pos] != x.block {
                    pos += 1;
                }
                if !cache.contains_key(&x.block) {
                    // Decode this block and the next few needed ones together.
                    cache.retain(|b, _| last_use[b] >= pos);
                    let mut seen = HashSet::new();
                    let wave: Vec<u32> = order[pos..]
                        .iter()
                        .copied()
                        .filter(|b| !cache.contains_key(b) && seen.insert(*b))
                        .take(par)
                        .collect();
                    let decoded: Vec<Result<(u32, Vec<u8>)>> =
                        pool.install(|| wave.par_iter().map(|&b| Ok((b, self.decode_block(b)?))).collect());
                    for d in decoded {
                        let (b, data) = d?;
                        cache.insert(b, Arc::new(data));
                    }
                }
                let slice = &cache[&x.block][x.offset as usize..(x.offset + x.len) as usize];
                out.write_all(slice).map_err(Error::at(path))?;
                hasher.update(slice);
                progress.add(u64::from(x.len));
            }
            drop(out);
            if short(hasher.finalize().as_bytes()) != e.hash {
                return Err(Error::Corrupt(format!("{}: checksum mismatch", e.path)));
            }
            apply_meta(path, from_unix(e.mtime, 0), e.mode);
            bytes += e.size;
            progress.file_done();
        }
        for (path, e) in dirs.iter().rev() {
            apply_meta(path, from_unix(e.mtime, 0), e.mode);
        }
        Ok((files.len() as u64, dirs.len() as u64, bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tiny segments force files across block boundaries and interleave the
    /// compressed and stored streams.
    #[test]
    fn files_spanning_many_small_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mut x: u32 = 7;
        let mut noise = || {
            (0..300_000)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    x as u8
                })
                .collect::<Vec<u8>>()
        };
        let text: Vec<u8> = (0..40_000).flat_map(|i| format!("{i} lorem\n").into_bytes()).collect();
        let files: Vec<(String, Vec<u8>)> = vec![
            ("a.txt".into(), text.clone()),
            ("b.bin".into(), noise()),
            ("c.txt".into(), text.iter().rev().copied().collect()),
            ("d.bin".into(), noise()),
            ("e.txt".into(), text),
        ];
        for (n, d) in &files {
            std::fs::write(src.join(n), d).unwrap();
        }
        let s = crate::scan::scan(&[src]).unwrap();
        let archive = tmp.path().join("a.czip");
        let p = Progress::new();
        write_with(&s, File::create(&archive).unwrap(), Level::Fast, Some("pw"), 2, &p, [64 << 10, 16 << 10])
            .unwrap();

        let a = Archive::open(&archive, Some("pw")).unwrap();
        assert!(a.blocks.len() > 20, "expected many blocks, got {}", a.blocks.len());
        assert!(a.blocks.iter().any(|b| b.method == M_STORE));
        assert!(a.blocks.iter().any(|b| b.method == M_ZSTD));
        let dest = tmp.path().join("x");
        a.extract(&dest, None, 3, &p).unwrap();
        for (n, d) in &files {
            assert_eq!(&std::fs::read(dest.join("src").join(n)).unwrap(), d, "{n}");
        }
        let only = vec!["src/d.bin".to_string()];
        let dest2 = tmp.path().join("y");
        a.extract(&dest2, Some(&only), 3, &p).unwrap();
        assert_eq!(std::fs::read(dest2.join("src/d.bin")).unwrap(), files[3].1);
        assert!(!dest2.join("src/a.txt").exists());
    }
}
