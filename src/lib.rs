//! `fm`, a terminal file manager. The interface is a plain state machine ([`app::App`]) that
//! renders to data, with previews, file operations and searches on background threads.

pub mod app;
pub mod config;
pub mod entry;
pub mod find;
pub mod ops;
pub mod preview;
pub mod term;
