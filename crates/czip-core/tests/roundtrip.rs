use std::fs;
use std::path::{Path, PathBuf};

use czip_core::{
    compress, extract, list, CompressOptions, Error, ExtractOptions, Format, Level, Progress,
};

/// Builds a small tree with text, binary noise, a duplicate, an empty file and
/// an empty folder, plus a loose file next to it.
fn fixture(root: &Path) -> Vec<PathBuf> {
    let proj = root.join("proyek");
    fs::create_dir_all(proj.join("src/nested")).unwrap();
    fs::create_dir_all(proj.join("kosong")).unwrap();
    let text: String = (0..20_000)
        .map(|i| format!("baris {i}: lorem ipsum dolor\n"))
        .collect();
    fs::write(proj.join("src/main.rs"), &text).unwrap();
    fs::write(proj.join("src/nested/copy.rs"), &text).unwrap();
    fs::write(proj.join("empty.txt"), b"").unwrap();
    let mut noise = vec![0u8; 3 << 20];
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for b in noise.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = x as u8;
    }
    fs::write(proj.join("foto.jpg"), &noise).unwrap();
    // Bigger than the dedup threshold so the streaming path is exercised.
    let big: Vec<u8> = text
        .as_bytes()
        .iter()
        .cycle()
        .take(20 << 20)
        .copied()
        .collect();
    fs::write(proj.join("big.log"), &big).unwrap();
    let loose = root.join("catatan.md");
    fs::write(&loose, "# catatan\nhalo\n").unwrap();
    vec![proj, loose]
}

fn assert_same(a: &Path, b: &Path) {
    for entry in walkdir(a) {
        let rel = entry.strip_prefix(a).unwrap();
        let other = b.join(rel);
        if entry.is_dir() {
            assert!(other.is_dir(), "missing dir {}", other.display());
        } else {
            assert_eq!(
                fs::read(&entry).unwrap(),
                fs::read(&other).unwrap(),
                "{}",
                rel.display()
            );
        }
    }
}

fn walkdir(p: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    for e in fs::read_dir(p).unwrap() {
        let path = e.unwrap().path();
        out.push(path.clone());
        if path.is_dir() {
            out.extend(walkdir(&path));
        }
    }
    out
}

fn roundtrip(format: Format, level: Level, password: Option<&str>) {
    let tmp = tempfile::tempdir().unwrap();
    let inputs = fixture(&tmp.path().join("in"));
    let archive = tmp.path().join(format!("out.{}", format.extension()));
    let opts = CompressOptions {
        format,
        level,
        password: password.map(String::from),
        threads: 4,
    };
    let summary = compress(&inputs, &archive, &opts, &Progress::new()).unwrap();
    assert_eq!(summary.files, 6);
    assert!(
        summary.output_bytes < summary.input_bytes,
        "{format:?} did not shrink"
    );
    assert!(!tmp
        .path()
        .join(format!("out.{}.partial", format.extension()))
        .exists());

    let info = list(&archive, password).unwrap();
    assert!(info
        .entries
        .iter()
        .any(|e| e.path == "proyek/src/main.rs" && e.size > 0));
    assert!(info.entries.iter().any(|e| e.path == "catatan.md"));

    let dest = tmp.path().join("x");
    let eo = ExtractOptions {
        password: password.map(String::from),
        ..Default::default()
    };
    let s = extract(&archive, &dest, &eo, &Progress::new()).unwrap();
    assert_eq!(s.files, 6);
    assert_same(&tmp.path().join("in/proyek"), &dest.join("proyek"));
    assert_eq!(
        fs::read(dest.join("catatan.md")).unwrap(),
        b"# catatan\nhalo\n"
    );
}

#[test]
fn czip_all_levels() {
    roundtrip(Format::Czip, Level::Fast, None);
    roundtrip(Format::Czip, Level::Balanced, None);
    roundtrip(Format::Czip, Level::Max, None);
}

#[test]
fn czip_encrypted() {
    roundtrip(Format::Czip, Level::Fast, Some("rahasia"));
}

#[test]
fn zip_plain_and_aes() {
    roundtrip(Format::Zip, Level::Balanced, None);
    roundtrip(Format::Zip, Level::Fast, Some("rahasia"));
}

#[test]
fn sevenz_plain_and_aes() {
    roundtrip(Format::SevenZ, Level::Fast, None);
    roundtrip(Format::SevenZ, Level::Fast, Some("rahasia"));
}

#[test]
fn tar_zst() {
    roundtrip(Format::TarZst, Level::Balanced, None);
}

#[test]
fn czip_wrong_password_and_missing_password() {
    let tmp = tempfile::tempdir().unwrap();
    let inputs = fixture(&tmp.path().join("in"));
    let archive = tmp.path().join("a.czip");
    let opts = CompressOptions {
        password: Some("benar".into()),
        ..Default::default()
    };
    compress(&inputs, &archive, &opts, &Progress::new()).unwrap();
    assert!(matches!(
        list(&archive, Some("salah")),
        Err(Error::WrongPassword)
    ));
    assert!(matches!(list(&archive, None), Err(Error::PasswordRequired)));
}

#[test]
fn czip_dedups_identical_files() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("d");
    fs::create_dir_all(&dir).unwrap();
    let mut x: u64 = 1;
    let noise: Vec<u8> = (0..(2 << 20))
        .map(|_| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
            (x >> 33) as u8
        })
        .collect();
    for i in 0..5 {
        fs::write(dir.join(format!("salinan{i}.bin")), &noise).unwrap();
    }
    let archive = tmp.path().join("d.czip");
    let s = compress(
        &[dir],
        &archive,
        &CompressOptions::default(),
        &Progress::new(),
    )
    .unwrap();
    assert!(
        s.output_bytes < (noise.len() as u64) * 11 / 10,
        "dedup did not kick in: {}",
        s.output_bytes
    );
    let dest = tmp.path().join("x");
    extract(
        &archive,
        &dest,
        &ExtractOptions::default(),
        &Progress::new(),
    )
    .unwrap();
    for i in 0..5 {
        assert_eq!(
            fs::read(dest.join(format!("d/salinan{i}.bin"))).unwrap(),
            noise
        );
    }
}

#[test]
fn czip_selective_extract() {
    let tmp = tempfile::tempdir().unwrap();
    let inputs = fixture(&tmp.path().join("in"));
    let archive = tmp.path().join("a.czip");
    compress(
        &inputs,
        &archive,
        &CompressOptions::default(),
        &Progress::new(),
    )
    .unwrap();
    let dest = tmp.path().join("x");
    let eo = ExtractOptions {
        only: Some(vec!["proyek/src".into()]),
        ..Default::default()
    };
    let s = extract(&archive, &dest, &eo, &Progress::new()).unwrap();
    assert_eq!(s.files, 2);
    assert!(dest.join("proyek/src/nested/copy.rs").exists());
    assert!(!dest.join("proyek/big.log").exists());
    assert!(!dest.join("catatan.md").exists());
}

#[test]
fn czip_detects_corruption() {
    let tmp = tempfile::tempdir().unwrap();
    let inputs = fixture(&tmp.path().join("in"));
    let archive = tmp.path().join("a.czip");
    compress(
        &inputs,
        &archive,
        &CompressOptions {
            level: Level::Fast,
            ..Default::default()
        },
        &Progress::new(),
    )
    .unwrap();
    let mut bytes = fs::read(&archive).unwrap();
    bytes[200] ^= 0xFF;
    fs::write(&archive, &bytes).unwrap();
    let r = extract(
        &archive,
        &tmp.path().join("x"),
        &ExtractOptions::default(),
        &Progress::new(),
    );
    assert!(matches!(r, Err(Error::Corrupt(_))), "{r:?}");
}

#[test]
fn clashing_input_names_are_renamed() {
    let tmp = tempfile::tempdir().unwrap();
    for side in ["a", "b"] {
        let d = tmp.path().join(side).join("docs");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("f.txt"), side).unwrap();
    }
    let inputs = vec![tmp.path().join("a/docs"), tmp.path().join("b/docs")];
    let archive = tmp.path().join("a.czip");
    compress(
        &inputs,
        &archive,
        &CompressOptions::default(),
        &Progress::new(),
    )
    .unwrap();
    let names: Vec<_> = list(&archive, None)
        .unwrap()
        .entries
        .into_iter()
        .map(|e| e.path)
        .collect();
    assert!(names.contains(&"docs/f.txt".to_string()));
    assert!(names.contains(&"docs (2)/f.txt".to_string()));
}

#[test]
fn cancel_leaves_no_partial_file() {
    let tmp = tempfile::tempdir().unwrap();
    let inputs = fixture(&tmp.path().join("in"));
    let archive = tmp.path().join("a.czip");
    let p = Progress::new();
    p.cancel();
    let r = compress(&inputs, &archive, &CompressOptions::default(), &p);
    assert!(matches!(r, Err(Error::Cancelled)));
    assert!(!archive.exists());
    assert!(!tmp.path().join("a.czip.partial").exists());
}
