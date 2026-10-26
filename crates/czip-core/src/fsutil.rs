use std::fs::{File, Metadata};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{Error, Result};

#[cfg(unix)]
pub(crate) fn mode_of(meta: &Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
pub(crate) fn mode_of(_meta: &Metadata) -> u32 {
    0
}

/// Restores modification time and (on Unix) permission bits. Best effort:
/// failing to set metadata never fails an extraction.
pub(crate) fn apply_meta(path: &Path, mtime: Option<SystemTime>, mode: u32) {
    #[cfg(unix)]
    if mode != 0 {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o777));
    }
    #[cfg(not(unix))]
    let _ = mode;
    if let Some(t) = mtime {
        let _ = filetime::set_file_mtime(path, filetime::FileTime::from_system_time(t));
    }
}

pub(crate) fn to_unix(t: Option<SystemTime>) -> (i64, u32) {
    match t.map(|t| t.duration_since(UNIX_EPOCH)) {
        Some(Ok(d)) => (d.as_secs() as i64, d.subsec_nanos()),
        Some(Err(e)) => (-(e.duration().as_secs() as i64), 0),
        None => (0, 0),
    }
}

pub(crate) fn from_unix(secs: i64, nanos: u32) -> Option<SystemTime> {
    if secs == 0 && nanos == 0 {
        return None;
    }
    if secs >= 0 {
        UNIX_EPOCH.checked_add(Duration::new(secs as u64, nanos))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(secs.unsigned_abs()))
    }
}

/// Joins an archive path onto `dest`, refusing anything that could escape it
/// (absolute paths, `..`, drive prefixes).
pub(crate) fn safe_join(dest: &Path, name: &str) -> Result<PathBuf> {
    let cleaned = name.replace('\\', "/");
    let mut out = dest.to_path_buf();
    let mut depth = 0;
    for part in cleaned.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        let comp = Path::new(part).components().next();
        match comp {
            Some(Component::Normal(p)) if Path::new(part).components().count() == 1 => {
                out.push(p);
                depth += 1;
            }
            _ => return Err(Error::UnsafePath(name.to_string())),
        }
    }
    if depth == 0 {
        return Err(Error::UnsafePath(name.to_string()));
    }
    Ok(out)
}

/// Whether `name` is selected by `only` (exact match or inside a chosen folder).
pub(crate) fn selected(only: Option<&[String]>, name: &str) -> bool {
    let Some(only) = only else { return true };
    let name = name.trim_end_matches('/');
    only.iter().any(|s| {
        let s = s.trim_end_matches('/');
        name == s
            || (name.len() > s.len() && name.starts_with(s) && name.as_bytes()[s.len()] == b'/')
    })
}

pub(crate) fn create_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(Error::at(parent))?;
    }
    File::create(path).map_err(Error::at(path))
}

/// Reads exactly `buf.len()` bytes at `offset` without moving a shared cursor,
/// so several threads can read blocks from one open file.
#[cfg(unix)]
pub(crate) fn read_at(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(windows)]
pub(crate) fn read_at(file: &File, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Civil (UTC) date for a timestamp, used by formats that store calendar time.
pub(crate) fn civil(t: SystemTime) -> (i64, u8, u8, u8, u8, u8) {
    let secs = to_unix(Some(t)).0;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (
        y,
        m,
        d,
        (rem / 3600) as u8,
        (rem % 3600 / 60) as u8,
        (rem % 60) as u8,
    )
}

pub(crate) fn from_civil(y: i64, m: u8, d: u8, hh: u8, mm: u8, ss: u8) -> Option<SystemTime> {
    let (m, d) = (i64::from(m), i64::from(d));
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + i64::from(hh) * 3600 + i64::from(mm) * 60 + i64::from(ss);
    from_unix(secs, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_rejects_escapes() {
        let d = Path::new("/tmp/out");
        assert!(safe_join(d, "../x").is_err());
        assert!(safe_join(d, "a/../../x").is_err());
        assert!(safe_join(d, "/etc/passwd")
            .map(|p| p.starts_with(d))
            .unwrap_or(true));
        assert_eq!(safe_join(d, "a/b.txt").unwrap(), d.join("a").join("b.txt"));
        assert!(safe_join(d, "").is_err());
    }

    #[test]
    fn selection_matches_folders() {
        let only = vec!["docs".to_string()];
        assert!(selected(Some(&only), "docs"));
        assert!(selected(Some(&only), "docs/a.txt"));
        assert!(!selected(Some(&only), "docs2/a.txt"));
    }

    #[test]
    fn civil_roundtrip() {
        let t = UNIX_EPOCH + Duration::from_secs(1_793_000_000);
        let (y, m, d, hh, mm, ss) = civil(t);
        assert_eq!(from_civil(y, m, d, hh, mm, ss), Some(t));
    }
}
