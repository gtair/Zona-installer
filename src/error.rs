//! Central error type for the Zona Installer.
//!
//! Every fallible function in the library returns [`Result<T>`]. Errors bubble
//! up to `app`, which records a `Phase::Fatal` carrying the message, [`ErrorId`]
//! and (where one exists) the log path; the GUI only renders that state.
//! `ConfigError` is defined here and re-exported by `config`.

use std::fmt;
use std::path::{Path, PathBuf};

/// The library-wide result alias. See [`AppError`].
pub type Result<T> = std::result::Result<T, AppError>;

/// A stable, human-facing error identifier shown in the support popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ErrorId(&'static str);

impl ErrorId {
    pub const fn new(id: &'static str) -> Self {
        Self(id)
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for ErrorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl From<ErrorId> for String {
    fn from(id: ErrorId) -> Self {
        id.0.to_string()
    }
}

/// Error raised while loading or validating `config.yaml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    message: String,
}

impl ConfigError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

/// The single error type for the whole library.
#[derive(Debug)]
pub enum AppError {
    /// `config.yaml` could not be parsed or validated.
    Config { source: ConfigError },
    /// A filesystem operation failed.
    Io {
        context: String,
        path: Option<PathBuf>,
        source: std::io::Error,
    },
    /// A bundled child tool (`7za.exe` / `aria2c.exe`) failed.
    Tool {
        tool: &'static str,
        context: String,
        exit_code: Option<i32>,
        log_path: Option<PathBuf>,
    },
    /// A downloaded asset did not match its expected SHA-256.
    HashMismatch {
        asset_id: String,
        path: PathBuf,
        expected: String,
        actual: String,
    },
    /// A Win32 API call failed.
    Win32 { context: String, code: u32 },
    /// An already-classified fatal error.
    Fatal {
        message: String,
        error_id: ErrorId,
        log_path: Option<PathBuf>,
    },
}

impl AppError {
    pub fn error_id(&self) -> ErrorId {
        match self {
            AppError::Config { .. } => ErrorId::new("E_CONFIG"),
            AppError::Io { .. } => ErrorId::new("E_IO"),
            AppError::Tool { .. } => ErrorId::new("E_TOOL"),
            AppError::HashMismatch { .. } => ErrorId::new("E_HASH_MISMATCH"),
            AppError::Win32 { .. } => ErrorId::new("E_WIN32"),
            AppError::Fatal { error_id, .. } => *error_id,
        }
    }

    /// The log file to point the user at, when the error carries one.
    ///
    /// Only [`AppError::Tool`] and [`AppError::Fatal`] carry a log; the paths on
    /// [`AppError::Io`] and [`AppError::HashMismatch`] are files, not logs.
    pub fn log_path(&self) -> Option<&Path> {
        match self {
            AppError::Tool { log_path, .. } | AppError::Fatal { log_path, .. } => {
                log_path.as_deref()
            }
            _ => None,
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::Config { source } => write!(f, "configuration error: {source}"),
            AppError::Io {
                context,
                path,
                source,
            } => match path {
                Some(path) => write!(f, "I/O error ({context}) at {}: {source}", path.display()),
                None => write!(f, "I/O error ({context}): {source}"),
            },
            AppError::Tool {
                tool,
                context,
                exit_code,
                log_path,
            } => {
                write!(f, "tool '{tool}' failed ({context})")?;
                if let Some(code) = exit_code {
                    write!(f, " with exit code {code}")?;
                }
                if let Some(log_path) = log_path {
                    write!(f, "; see log {}", log_path.display())?;
                }
                Ok(())
            }
            AppError::HashMismatch {
                asset_id,
                path,
                expected,
                actual,
            } => write!(
                f,
                "hash mismatch for asset '{asset_id}' at {}: expected {expected}, got {actual}",
                path.display()
            ),
            AppError::Win32 { context, code } => {
                write!(f, "Win32 error ({context}): code {code}")
            }
            AppError::Fatal {
                message,
                error_id,
                log_path,
            } => {
                write!(f, "{message} (error id: {error_id})")?;
                if let Some(log_path) = log_path {
                    write!(f, "; see log {}", log_path.display())?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for AppError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AppError::Config { source } => Some(source),
            AppError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    fn sample_errors() -> Vec<AppError> {
        vec![
            AppError::Config {
                source: ConfigError::new("unsupported version 2"),
            },
            AppError::Io {
                context: "read config".to_string(),
                path: Some(PathBuf::from(r"C:\Zona\installer\dependencies\config.yaml")),
                source: io::Error::new(io::ErrorKind::NotFound, "no such file"),
            },
            AppError::Tool {
                tool: "7za.exe",
                context: "extract archive".to_string(),
                exit_code: Some(2),
                log_path: Some(PathBuf::from(r"C:\Zona\installer\logs\run.log")),
            },
            AppError::HashMismatch {
                asset_id: "zona_multipart".to_string(),
                path: PathBuf::from(r"C:\Zona\installer\downloads\zona.7z.001"),
                expected: "aa".to_string(),
                actual: "bb".to_string(),
            },
            AppError::Win32 {
                context: "CreateJobObjectW".to_string(),
                code: 5,
            },
            AppError::Fatal {
                message: "installation failed".to_string(),
                error_id: ErrorId::new("E_FATAL"),
                log_path: None,
            },
        ]
    }

    #[test]
    fn display_is_non_empty_for_every_variant() {
        for error in sample_errors() {
            assert!(
                !error.to_string().trim().is_empty(),
                "empty Display output for {error:?}"
            );
        }
    }

    #[test]
    fn error_id_is_stable_per_variant() {
        let ids: Vec<&str> = sample_errors()
            .iter()
            .map(|error| error.error_id().as_str())
            .collect();
        assert_eq!(
            ids,
            vec![
                "E_CONFIG",
                "E_IO",
                "E_TOOL",
                "E_HASH_MISMATCH",
                "E_WIN32",
                "E_FATAL",
            ]
        );
    }

    #[test]
    fn fatal_error_id_is_the_carried_id() {
        let error = AppError::Fatal {
            message: "boom".to_string(),
            error_id: ErrorId::new("E_CUSTOM"),
            log_path: None,
        };
        assert_eq!(error.error_id(), ErrorId::new("E_CUSTOM"));
        assert_eq!(error.error_id().as_str(), "E_CUSTOM");
        assert_eq!(String::from(error.error_id()), "E_CUSTOM");
    }

    #[test]
    fn log_path_is_exposed_for_tool_and_fatal_only() {
        let log = PathBuf::from(r"C:\Zona\installer\logs\aria2c.log");

        let tool = AppError::Tool {
            tool: "aria2c.exe",
            context: "download".to_string(),
            exit_code: Some(1),
            log_path: Some(log.clone()),
        };
        assert_eq!(tool.log_path(), Some(log.as_path()));

        let fatal = AppError::Fatal {
            message: "boom".to_string(),
            error_id: ErrorId::new("E_FATAL"),
            log_path: Some(log.clone()),
        };
        assert_eq!(fatal.log_path(), Some(log.as_path()));

        let tool_without_log = AppError::Tool {
            tool: "7za.exe",
            context: "extract".to_string(),
            exit_code: None,
            log_path: None,
        };
        assert_eq!(tool_without_log.log_path(), None);

        // An Io path is the file being handled, not a run log.
        let io = AppError::Io {
            context: "write".to_string(),
            path: Some(log.clone()),
            source: io::Error::new(io::ErrorKind::PermissionDenied, "denied"),
        };
        assert_eq!(io.log_path(), None);

        let hash = AppError::HashMismatch {
            asset_id: "asset".to_string(),
            path: log.clone(),
            expected: "aa".to_string(),
            actual: "bb".to_string(),
        };
        assert_eq!(hash.log_path(), None);
    }

    #[test]
    fn source_is_exposed_for_wrapped_errors_only() {
        use std::error::Error;

        let io = AppError::Io {
            context: "read".to_string(),
            path: None,
            source: io::Error::other("kaboom"),
        };
        assert!(io.source().is_some());

        let config = AppError::Config {
            source: ConfigError::new("bad"),
        };
        assert!(config.source().is_some());
        assert_eq!(
            config.source().map(ToString::to_string).as_deref(),
            Some("bad")
        );

        let win32 = AppError::Win32 {
            context: "GetDriveTypeW".to_string(),
            code: 6,
        };
        assert!(win32.source().is_none());
    }

    #[test]
    fn wrapped_error_display_is_non_empty() {
        assert!(!ConfigError::new("x").to_string().trim().is_empty());
        assert_eq!(ConfigError::new("x").message(), "x");
    }
}
