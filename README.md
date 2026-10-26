# czip

Aplikasi kompresi file & folder yang cepat, kuat, dan sederhana — ditulis dengan Rust.
Tarik banyak file dan folder sekaligus, pilih tingkat kompresi, selesai.

- **Format `.czip` sendiri**: zstd multi-thread, deduplikasi file kembar, checksum per file,
  enkripsi opsional, dan bisa mengekstrak satu file tanpa membongkar seluruh arsip.
- **Tetap kompatibel**: membuat `.zip` (bisa dibuka di mana saja), `.7z`, dan `.tar.zst`;
  membuka `.czip`, `.zip`, `.7z`, `.tar`, `.tar.zst/.gz/.xz`, serta `.gz`/`.xz`/`.zst` tunggal.
- **Aplikasi desktop** (Tauri 2, Windows/macOS/Linux) dan **CLI**.

## Benchmark

96,6 MB source code (6.893 file, salinan registry cargo), 4 core, rata-rata satu kali jalan.
Semua hasil diekstrak ulang dan dicocokkan byte per byte.

| Alat | Waktu kompres | Ukuran |
|---|---:|---:|
| **czip `.czip` cepat** | **0,5 dtk** | 13,95 MB |
| **czip `.czip` seimbang** | **0,7 dtk** | **11,54 MB** |
| **czip `.czip` maksimal** | 13,6 dtk | **9,98 MB** |
| tar + zstd 9 (multi-thread) | 0,6 dtk | 11,72 MB |
| tar + zstd 19 --long=27 | 20,3 dtk | 10,07 MB |
| xz -6 -T0 | 8,2 dtk | 10,38 MB |
| xz -9 -T0 | 38,0 dtk | 9,73 MB |
| zip -9 (Info-ZIP) | 5,7 dtk | 21,85 MB |
| czip → `.zip` seimbang | 1,5 dtk | 21,84 MB |
| czip → `.7z` seimbang | 19,2 dtk | 10,17 MB |

Ringkasnya: mode **seimbang** ~12× lebih cepat dari xz -6 dengan ukuran ~11% lebih besar, dan
hampir separuh ukuran zip. Mode **maksimal** lebih kecil dari xz -6 dan 2,8× lebih cepat dari
xz -9; xz -9 masih unggul ~2,5% di ukuran. Ekstraksi zstd jauh lebih cepat dari LZMA/xz.
Hasil akan berbeda untuk data lain — foto dan video memang hampir tidak bisa dikecilkan lagi;
czip mendeteksinya dan menyimpannya apa adanya supaya tidak membuang waktu.

## Struktur

```
crates/czip-core   engine: format .czip, adapter zip/7z/tar, scan, ekstraksi aman
crates/czip-cli    perintah `czip`
app/src-tauri      aplikasi desktop (backend Rust, perintah & event progres)
app/ui             tampilan (HTML/CSS/JS tanpa framework, mode gelap otomatis)
```

### Library

| Kebutuhan | Crate | Alasan |
|---|---|---|
| Kompresi utama | `zstd` (fitur `zstdmt`) | rasio/kecepatan terbaik saat ini, multi-thread, dekompresi > 1 GB/dtk |
| ZIP | `zip` + `flate2` (backend `zlib-rs`) | Deflate cepat, AES-256, Zip64 |
| 7z | `sevenz-rust2` | LZMA2 multi-thread + AES-256, Rust murni |
| tar / xz / gzip | `tar`, `lzma-rust2`, `flate2` | membuka arsip Linux/macOS |
| Paralel | `rayon` | dekode segmen paralel |
| Integritas | `blake3` | hash tercepat, juga untuk deteksi file kembar |
| Enkripsi | `chacha20poly1305` + `argon2` | XChaCha20-Poly1305 per blok, kunci dari Argon2id |
| Telusur folder | `walkdir` | |
| Desktop | `tauri` 2 + plugin dialog/opener | biner kecil, UI web, native di 3 OS |
| CLI | `clap`, `indicatif` | |

## Format `.czip` (v1)

```
header   "CZIP" | versi u16 | flags u16 | [salt 16 | prefix nonce 16 | parameter Argon2]
blok     segmen zstd (≤128 MiB data mentah) atau blok tersimpan (≤32 MiB)
index    zstd(tabel blok + tabel entri berbentuk kolom)
footer   offset index u64 | panjang index u64 | "CZIPEND!"
```

- Isi tiap file dicek dulu (sampel 256 KB dikompres zstd-1). Yang bisa dikompres masuk ke
  stream zstd multi-thread — membaca file dan mengompres berjalan bersamaan; yang tidak
  (JPEG, MP4, arsip lain) disimpan apa adanya. Ekstensi file tidak dipercaya begitu saja.
- Stream dipotong per segmen 128 MiB: satu file bisa diekstrak tanpa membongkar semuanya,
  dan segmen didekode paralel.
- Mode maksimal: zstd 19 + long-distance matching dengan jendela 128 MiB, job 32 MiB.
- File yang isinya sama persis disimpan sekali.
- Index berbentuk kolom (path berkode-depan, varint, delta) supaya kecil — ~15 byte per file.
- Checksum BLAKE3 64-bit per file diverifikasi saat ekstraksi. Dengan kata sandi, setiap
  blok dan index juga diautentikasi (AEAD), dan nama file ikut terenkripsi.
- Ekstraksi menolak path berbahaya (`..`, path absolut) di semua format; symlink di tar dilewati.
- Kompresi ditulis ke `nama.partial` lalu di-rename — gagal atau batal tidak meninggalkan arsip setengah jadi.

## Menjalankan

CLI:

```sh
cargo build --release
./target/release/czip c Foto/ laporan.docx -l max          # → Foto.czip
./target/release/czip c proyek/ -f zip -p                  # zip ber-AES, kata sandi ditanya
./target/release/czip l Foto.czip
./target/release/czip x Foto.czip                          # → Foto/
./target/release/czip x Foto.czip --only Foto/2026 -o out/
```

Aplikasi desktop — butuh dependensi WebView sesuai OS
(lihat [prasyarat Tauri](https://v2.tauri.app/start/prerequisites/); di Debian/Ubuntu:
`libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev`):

```sh
cargo run -p czip-app                          # mode pengembangan
cargo install tauri-cli --version "^2"
cd app/src-tauri && cargo tauri build          # installer .msi/.exe, .dmg/.app, .deb/.AppImage
```

UI juga bisa dibuka langsung di browser (`app/ui/index.html`) dengan data contoh untuk
mengutak-atik tampilan.

Tes:

```sh
cargo test
```

## Belum ada

- Ekstrak `.rar` (butuh pustaka unrar berlisensi non-bebas).
- Arsip terpecah (multi-volume) dan menu klik-kanan di Explorer/Finder.
- Tanda tangan kode untuk installer.
