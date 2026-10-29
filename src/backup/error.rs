//! Error type of the backup module.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Errors that abort a backup operation as a whole.
///
/// Problems with single files during an import are not errors of this type; they are collected in
/// [`super::ImportReport`] (BAK-5).
#[derive(Debug)]
pub enum BackupError {
    /// An I/O error, with a short description of what was being done.
    Io { context: String, source: io::Error },
    /// The archive is not a valid XorOS backup (corrupt gzip/tar data).
    Format(String),
    /// `xoros-backup.json` is missing, unreadable or of an unsupported format; the text says
    /// what was found (BAK-4a).
    Metadata(String),
    /// The `.sha256` sidecar file is missing (the path it was expected at).
    MissingChecksumFile(PathBuf),
    /// Verification against the `.sha256` file failed (BAK-4a). `file` is the archive part, or
    /// the checksum file itself for [`ChecksumProblem::Malformed`].
    Checksum {
        file: PathBuf,
        problem: ChecksumProblem,
    },
    /// The caller passed something that cannot work (e.g. a partition that is not in the archive).
    InvalidArgument(String),
    /// The operation was cancelled through the [`super::CancelToken`].
    Cancelled,
}

pub type Result<T> = std::result::Result<T, BackupError>;

/// What exactly is wrong with a backup's checksums (BAK-4a).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChecksumProblem {
    /// A line of the checksum file is not `<64 hex>  <name>`; 1-based line number and content.
    Malformed { line: usize, content: String },
    /// The part is listed in the checksum file but not on the stick.
    ListedButMissing,
    /// The part is on the stick but not listed in the checksum file.
    NotListed,
    /// The part's SHA-256 differs (full lower-case hex values).
    Mismatch { expected: String, actual: String },
}

/// First 16 characters of a hex digest followed by an ellipsis.
fn short_hex(h: &str) -> String {
    let n = h.char_indices().nth(16).map_or(h.len(), |(i, _)| i);
    format!("{}\u{2026}", &h[..n])
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

impl fmt::Display for BackupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackupError::Io { context, source } => write!(f, "{context}: {source}"),
            BackupError::Format(msg) => write!(f, "invalid backup archive: {msg}"),
            BackupError::Metadata(msg) => write!(f, "{}: {msg}", super::meta::METADATA_NAME),
            BackupError::MissingChecksumFile(p) => write!(
                f,
                "Checksum file `{}` not found next to the backup",
                file_name(p)
            ),
            BackupError::Checksum { file, problem } => {
                let name = file_name(file);
                match problem {
                    ChecksumProblem::Malformed { line, content } => write!(
                        f,
                        "Checksum file `{name}`: line {line} is malformed: `{content}`"
                    ),
                    ChecksumProblem::ListedButMissing => write!(
                        f,
                        "Part `{name}` is listed in the checksum file but missing"
                    ),
                    ChecksumProblem::NotListed => write!(
                        f,
                        "Part `{name}` is present on the stick but not listed in the checksum file"
                    ),
                    ChecksumProblem::Mismatch { expected, actual } => write!(
                        f,
                        "Part `{name}`: expected SHA-256 `{}`, got `{}` (file damaged or changed)",
                        short_hex(expected),
                        short_hex(actual)
                    ),
                }
            }
            BackupError::InvalidArgument(msg) => write!(f, "{msg}"),
            BackupError::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for BackupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BackupError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl BackupError {
    pub(crate) fn io(context: impl Into<String>, source: io::Error) -> Self {
        BackupError::Io {
            context: context.into(),
            source,
        }
    }

    pub(crate) fn io_path(what: &str, path: &Path, source: io::Error) -> Self {
        BackupError::Io {
            context: format!("{what} {}", path.display()),
            source,
        }
    }
}

/// Adds context to `io::Result`s.
pub(crate) trait IoContext<T> {
    fn ctx(self, context: impl FnOnce() -> String) -> Result<T>;
}

impl<T> IoContext<T> for io::Result<T> {
    fn ctx(self, context: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|e| BackupError::io(context(), e))
    }
}
