//! Parsers for the stdout of the external tools the installer drives.
//!
//! Each submodule turns the raw bytes captured from a child process into plain,
//! testable data. The parsers are pure: no OS, process or network access, so they
//! are unit-tested directly against the byte-exact fixtures under
//! `tests/fixtures/`.

pub mod aria2;
pub mod sevenzip;
