//! Error type shared by the library surface.

use std::fmt;
use std::path::PathBuf;

#[derive(Debug)]
pub enum Error {
    /// `OSError` from reading or writing a file, with the path attached.
    Io { path: Option<PathBuf>, source: std::io::Error },
    /// `ValueError`-style rejection with a message the CLI prints verbatim.
    Msg(String),
    /// `FurEditError` from `edit.py`.
    FurEdit(String),
    /// `FurnaceError` from `furnace.py`.
    Furnace(String),
    /// `_Usage(int)` from `__main__.py`.
    Exit(i32),
    /// KeyboardInterrupt.
    Interrupt,
}

impl Error {
    pub fn msg(text: impl Into<String>) -> Self {
        Error::Msg(text.into())
    }

    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io { path: Some(path.into()), source }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { path: Some(p), source } => write!(f, "{}: {}", p.display(), source),
            Error::Io { path: None, source } => write!(f, "{}", source),
            Error::Msg(m) | Error::FurEdit(m) | Error::Furnace(m) => write!(f, "{}", m),
            Error::Exit(code) => write!(f, "exit {}", code),
            Error::Interrupt => write!(f, "interrupted"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Self {
        Error::Io { path: None, source }
    }
}

impl From<String> for Error {
    fn from(m: String) -> Self {
        Error::Msg(m)
    }
}

impl From<&str> for Error {
    fn from(m: &str) -> Self {
        Error::Msg(m.to_string())
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
