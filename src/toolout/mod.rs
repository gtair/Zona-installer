//! Parsers for the stdout of the external tools the installer drives.
//!
//! Each submodule turns the raw bytes captured from a child process into plain,
//! testable data. The parsers are pure: no OS, process or network access.

pub mod aria2;
pub mod sevenzip;
