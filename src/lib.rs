//! Zona Installer library crate.
//!
//! All installer logic lives here; the companion binary (`src/main.rs`) is a
//! thin GUI shell that renders state and forwards events. No GUI-toolkit type
//! appears in this crate.

pub mod app;
pub mod config;
pub mod error;
pub mod finish;
pub mod gating;
pub mod hashing;
pub mod logging;
pub mod paths;
pub mod pipeline;
pub mod preflight;
pub mod run;
pub mod template;
pub mod toolout;
pub mod win32;
