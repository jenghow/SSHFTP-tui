//! sshftp-tui: a cross-platform TUI SSH + SFTP client.
//!
//! The crate is exposed as a library so the network/frontend layers can be
//! driven directly by integration tests; [`main`](../main/index.html) is a thin
//! wrapper around [`app::App`] plus [`ui`].

pub mod app;
pub mod event;
pub mod form;
pub mod hostkey;
pub mod keys;
pub mod network;
pub mod saved;
pub mod sftp;
pub mod sshconfig;
pub mod ui;
