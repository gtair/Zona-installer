//! The GUI screens.
//!
//! Each screen is a data-only renderer: it reads [`ViewModel`] and pushes
//! [`Event`]s back to the shell, which forwards them to `zona_installer::app::App`.
//!
//! [`ViewModel`]: zona_installer::app::ViewModel
//! [`Event`]: zona_installer::app::Event

pub mod choice;
pub mod download;
pub mod finish;
pub mod install;
pub mod relocation;
