//! Sections of the main window.

pub mod admin;
pub mod ai;
pub mod ai_chat;
pub mod ai_settings;
pub mod forwards;
pub mod host_editor;
pub mod host_picker;
pub mod hosts;
pub mod import;
pub mod keychain;
pub mod known_hosts;
pub mod serial;
pub mod server_sessions;
pub mod settings;
pub mod sftp;
pub mod share;
pub mod snippets;
pub mod teams;
pub mod two_factor;

use std::sync::Arc;

use termoak_core::Id;
use termoak_ssh::Connection;

/// Requests from the sections to the main window (open tabs).
#[derive(Clone)]
pub enum OpenRequest {
    /// SSH terminal from this computer.
    Local { host_id: Id },
    /// Terminal that lives on the server.
    Server { host_id: Id },
    /// Attach to a server session.
    Attach { session_id: Id, title: String },
    /// SFTP browser (reusing a connection if there is one).
    Sftp {
        host_id: Id,
        conn: Option<Arc<Connection>>,
    },
    /// Several hosts in a split view (one tab, a grid of terminals): added
    /// to the current workspace (`current`) or in a new tab.
    Split { hosts: Vec<Id>, current: bool },
    /// Local terminal: a shell on this computer.
    Shell,
    /// Serial terminal: a port on this computer.
    Serial(crate::terminal::serial::SerialParams),
    /// Settings → AI (where the AI runs, API keys, AI credit).
    AiSettings,
}
