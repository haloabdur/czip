use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use clap::{Parser, Subcommand};
use czip_core::{CompressOptions, ExtractOptions, Format, Level, Progress};
use indicatif::{HumanBytes, ProgressBar, ProgressStyle};

#[derive(Parser)]
#[command(name = "czip", version, about = "Fast, strong compression for files and folders")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Compress files and folders into one archive
    #[command(visible_alias = "c")]
    Compress {
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        /// Output archive (default: next to the first input)
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// czip, zip, 7z or tar.zst
        #[arg(short, long, default_value = "czip", value_parser = parse_format)]
        format: Format,
        /// fast, balanced or max
        #[arg(short, long, default_value = "balanced", value_parser = parse_level)]
        level: Level,
        /// Protect with a password (asked for if no value is given)
        #[arg(short, long, num_args = 0..=1, default_missing_value = "")]
        password: Option<String>,
        /// Worker threads (default: all cores)
        #[arg(short = 't', long, default_value_t = 0)]
        threads: usize,
    },
    /// Extract an archive (.czip, .zip, .7z, .tar.zst/.gz/.xz, .gz, .xz, .zst)
    #[command(visible_alias = "x")]
    Extract {
        archive: PathBuf,
        /// Destination folder (default: a folder named after the archive)
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Only extract these paths inside the archive
        #[arg(long, num_args = 1..)]
        only: Option<Vec<String>>,
        #[arg(short, long, num_args = 0..=1, default_missing_value = "")]
        password: Option<String>,
        #[arg(short = 't', long, default_value_t = 0)]
        threads: usize,
    },
    /// List what is inside an archive
    #[command(visible_alias = "l")]
    List {
        archive: PathBuf,
        #[arg(short, long, num_args = 0..=1, default_missing_value = "")]
        password: Option<String>,
    },
}

fn parse_format(s: &str) -> Result<Format, String> {
    Format::from_name(s).ok_or_else(|| format!("unknown format '{s}' (czip, zip, 7z, tar.zst)"))
}

fn parse_level(s: &str) -> Result<Level, String> {
    Level::from_name(s).ok_or_else(|| format!("unknown level '{s}' (fast, balanced, max)"))
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("czip: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), czip_core::Error> {
    let progress = Progress::new();
    {
        let p = progress.clone();
        let _ = ctrlc::set_handler(move || p.cancel());
    }

    match cli.cmd {
        Cmd::Compress { inputs, output, format, level, password, threads } => {
            let password = resolve_password(password, true)?;
            let output = output.unwrap_or_else(|| czip_core::suggest_output(&inputs, format));
            let opts = CompressOptions { format, level, password, threads };
            let job = {
                let (p, output) = (progress.clone(), output.clone());
                thread::spawn(move || czip_core::compress(&inputs, &output, &opts, &p))
            };
            let s = watch(job, &progress, "Compressing")?;
            println!(
                "{}  {} → {}  (saved {:.0}%, {} files, {:.1}s)",
                s.output.display(),
                HumanBytes(s.input_bytes),
                HumanBytes(s.output_bytes),
                s.saved() * 100.0,
                s.files,
                s.elapsed.as_secs_f64()
            );
            if !s.skipped.is_empty() {
                eprintln!("skipped {} symlink(s) or unreadable entries", s.skipped.len());
            }
        }
        Cmd::Extract { archive, output, only, password, threads } => {
            let password = resolve_password(password, false)?;
            let dest = output.unwrap_or_else(|| default_dest(&archive));
            let opts = ExtractOptions { password, only: only.clone(), threads };
            let job = {
                let (p, dest, archive) = (progress.clone(), dest.clone(), archive.clone());
                thread::spawn(move || czip_core::extract(&archive, &dest, &opts, &p))
            };
            let s = match watch(job, &progress, "Extracting") {
                Err(czip_core::Error::PasswordRequired) => {
                    return run(Cli {
                        cmd: Cmd::Extract { archive, output: Some(dest), only, password: Some(String::new()), threads },
                    })
                }
                r => r?,
            };
            println!(
                "{}  {} files, {}  ({:.1}s)",
                dest.display(),
                s.files,
                HumanBytes(s.bytes),
                s.elapsed.as_secs_f64()
            );
        }
        Cmd::List { archive, password } => {
            let password = resolve_password(password, false)?;
            let info = match czip_core::list(&archive, password.as_deref()) {
                Err(czip_core::Error::PasswordRequired) => {
                    let pw = resolve_password(Some(String::new()), false)?;
                    czip_core::list(&archive, pw.as_deref())?
                }
                r => r?,
            };
            for e in &info.entries {
                if e.is_dir {
                    println!("{:>12}  {}/", "", e.path);
                } else {
                    println!("{:>12}  {}", HumanBytes(e.size).to_string(), e.path);
                }
            }
            let files = info.entries.iter().filter(|e| !e.is_dir).count();
            println!(
                "{} · {} files · {}{}",
                info.format,
                files,
                HumanBytes(info.unpacked_bytes),
                if info.encrypted { " · encrypted" } else { "" }
            );
        }
    }
    Ok(())
}

/// `Some("")` means the flag was given without a value: ask on the terminal.
fn resolve_password(p: Option<String>, confirm: bool) -> Result<Option<String>, czip_core::Error> {
    match p {
        Some(p) if p.is_empty() => {
            let pw = rpassword::prompt_password("Password: ")?;
            if confirm && rpassword::prompt_password("Repeat password: ")? != pw {
                return Err(czip_core::Error::Unsupported("passwords do not match".into()));
            }
            Ok(Some(pw).filter(|p| !p.is_empty()))
        }
        other => Ok(other.or_else(|| std::env::var("CZIP_PASSWORD").ok())),
    }
}

fn default_dest(archive: &Path) -> PathBuf {
    let name = archive.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let lower = name.to_lowercase();
    let stem = [".tar.zst", ".tar.gz", ".tar.xz", ".czip", ".zip", ".7z", ".tgz", ".tar", ".zst", ".gz", ".xz"]
        .iter()
        .find(|ext| lower.ends_with(*ext))
        .map(|ext| &name[..name.len() - ext.len()])
        .unwrap_or(&name);
    archive.with_file_name(if stem.is_empty() { "extracted" } else { stem })
}

fn watch<T: Send + 'static>(
    job: thread::JoinHandle<Result<T, czip_core::Error>>,
    progress: &Progress,
    verb: &str,
) -> Result<T, czip_core::Error> {
    let bar = ProgressBar::new(0);
    bar.set_style(
        ProgressStyle::with_template("{prefix} [{bar:32}] {percent:>3}% {bytes}/{total_bytes} {binary_bytes_per_sec}  {wide_msg}")
            .expect("valid template")
            .progress_chars("━━─"),
    );
    bar.set_prefix(verb.to_string());
    while !job.is_finished() {
        let s = progress.snapshot();
        bar.set_length(s.total_bytes);
        bar.set_position(s.done_bytes.min(s.total_bytes));
        bar.set_message(s.current);
        thread::sleep(Duration::from_millis(50));
    }
    bar.finish_and_clear();
    job.join().expect("worker thread panicked")
}
