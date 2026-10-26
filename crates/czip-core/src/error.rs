use std::io;
use std::path::PathBuf;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{}: {source}", path.display())]
    IoPath { path: PathBuf, source: io::Error },
    #[error("unrecognised archive format")]
    UnknownFormat,
    #[error("archive is corrupt: {0}")]
    Corrupt(String),
    #[error("wrong password")]
    WrongPassword,
    #[error("this archive is encrypted; a password is required")]
    PasswordRequired,
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Unsupported(String),
    #[error("unsafe path in archive: {0}")]
    UnsafePath(String),
    #[error("nothing to compress")]
    Empty,
    #[error("zip: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("7z: {0}")]
    SevenZ(#[from] sevenz_rust2::Error),
}

impl Error {
    /// Stable machine-readable code, so front-ends can show their own wording.
    pub fn code(&self) -> &'static str {
        match self {
            Error::Io(_) | Error::IoPath { .. } => "io",
            Error::UnknownFormat => "unknown_format",
            Error::Corrupt(_) => "corrupt",
            Error::WrongPassword => "wrong_password",
            Error::PasswordRequired => "password_required",
            Error::Cancelled => "cancelled",
            Error::Unsupported(_) => "unsupported",
            Error::UnsafePath(_) => "unsafe_path",
            Error::Empty => "empty",
            Error::Zip(zip::result::ZipError::InvalidPassword) => "wrong_password",
            Error::Zip(_) => "zip",
            Error::SevenZ(sevenz_rust2::Error::PasswordRequired) => "password_required",
            Error::SevenZ(sevenz_rust2::Error::MaybeBadPassword(_)) => "wrong_password",
            Error::SevenZ(_) => "7z",
        }
    }

    pub(crate) fn at(path: impl Into<PathBuf>) -> impl FnOnce(io::Error) -> Error {
        let path = path.into();
        move |source| Error::IoPath { path, source }
    }
}
