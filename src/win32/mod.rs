//! Raw Windows (Win32) wrappers used by the installer.
//!
//! Each submodule isolates the unsafe FFI behind a small surface so the pure
//! installer logic stays OS-free and unit-testable.

pub mod console;
pub mod drives;
pub mod job;
pub mod relocate;
pub mod shortcut;
pub mod single_instance;
pub mod taskbar;
