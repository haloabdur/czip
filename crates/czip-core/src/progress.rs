use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::{Error, Result};

/// Shared progress + cancellation handle.
///
/// The engine updates it from worker threads; a UI polls [`Progress::snapshot`]
/// on its own timer and calls [`Progress::cancel`] to stop a running job.
#[derive(Clone, Default)]
pub struct Progress {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    total: AtomicU64,
    done: AtomicU64,
    files_done: AtomicU64,
    cancelled: AtomicBool,
    current: Mutex<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub total_bytes: u64,
    pub done_bytes: u64,
    pub files_done: u64,
    pub current: String,
}

impl Progress {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            total_bytes: self.inner.total.load(Ordering::Relaxed),
            done_bytes: self.inner.done.load(Ordering::Relaxed),
            files_done: self.inner.files_done.load(Ordering::Relaxed),
            current: self.inner.current.lock().map(|c| c.clone()).unwrap_or_default(),
        }
    }

    pub(crate) fn start(&self, total: u64) {
        self.inner.total.store(total, Ordering::Relaxed);
        self.inner.done.store(0, Ordering::Relaxed);
        self.inner.files_done.store(0, Ordering::Relaxed);
    }

    pub(crate) fn add(&self, bytes: u64) {
        self.inner.done.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn file_done(&self) {
        self.inner.files_done.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn set_current(&self, name: &str) {
        if let Ok(mut c) = self.inner.current.lock() {
            c.clear();
            c.push_str(name);
        }
    }

    pub(crate) fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Turns any failure into [`Error::Cancelled`] once the user cancelled, so
    /// the I/O error used to unwind out of a library call is not shown.
    pub(crate) fn settle<T>(&self, r: Result<T>) -> Result<T> {
        match r {
            Err(_) if self.is_cancelled() => Err(Error::Cancelled),
            other => other,
        }
    }
}

/// Reader that reports bytes to a [`Progress`] and aborts once cancelled.
pub(crate) struct Counting<'p, R> {
    inner: R,
    progress: &'p Progress,
}

impl<'p, R> Counting<'p, R> {
    pub(crate) fn new(inner: R, progress: &'p Progress) -> Self {
        Self { inner, progress }
    }
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.progress.is_cancelled() {
            return Err(io::Error::other("cancelled"));
        }
        let n = self.inner.read(buf)?;
        self.progress.add(n as u64);
        Ok(n)
    }
}
