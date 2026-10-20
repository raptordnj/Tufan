use std::fmt;
use std::io;
use std::path::PathBuf;

#[derive(Debug)]
pub enum Error {
    Io { path: PathBuf, source: io::Error },
    SourceChanged(PathBuf),
    VerifyFailed(PathBuf),
    DestinationInsideSource { source: PathBuf, dest: PathBuf },
    Conflict(PathBuf),
    InvalidPath(PathBuf),
    Cancelled,
    Journal(rusqlite::Error),
    Corrupt(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Error::Io { path: path.into(), source }
    }

    /// Transient failures worth retrying with backoff. Permanent errors
    /// (permission denied, not found, invalid path) are never retried.
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::SourceChanged(_) => true,
            Error::Io { source, .. } => is_retryable_io(source),
            _ => false,
        }
    }
}

pub(crate) fn is_retryable_io(e: &io::Error) -> bool {
    use io::ErrorKind::*;
    if matches!(e.kind(), Interrupted | TimedOut | WouldBlock | ResourceBusy) {
        return true;
    }
    // ERROR_SHARING_VIOLATION (32) / ERROR_LOCK_VIOLATION (33)
    #[cfg(windows)]
    if matches!(e.raw_os_error(), Some(32) | Some(33)) {
        return true;
    }
    false
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { path, source } => write!(f, "{}: {}", path.display(), source),
            Error::SourceChanged(p) => write!(f, "{}: source changed during transfer", p.display()),
            Error::VerifyFailed(p) => write!(f, "{}: content verification failed", p.display()),
            Error::DestinationInsideSource { source, dest } => write!(
                f,
                "destination {} is inside source {}",
                dest.display(),
                source.display()
            ),
            Error::Conflict(p) => write!(f, "{}: destination already exists", p.display()),
            Error::InvalidPath(p) => write!(f, "{}: invalid path", p.display()),
            Error::Cancelled => write!(f, "cancelled"),
            Error::Journal(e) => write!(f, "journal: {e}"),
            Error::Corrupt(m) => write!(f, "journal corrupt: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Journal(e)
    }
}
