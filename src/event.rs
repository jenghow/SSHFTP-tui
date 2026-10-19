//! Message types exchanged between the UI task and the network task.

use std::path::{Path, PathBuf};

use tokio::sync::oneshot;

/// Which of the two SFTP panes an operation targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneKind {
    Local,
    Remote,
}

impl PaneKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Local => "LOCAL",
            Self::Remote => "REMOTE",
        }
    }
}

/// A fully resolved path on either side of the transfer.
#[derive(Debug, Clone)]
pub enum Endpoint {
    Local(PathBuf),
    Remote(String),
}

pub enum AuthMethod {
    Password(String),
    KeyFile {
        path: PathBuf,
        passphrase: Option<String>,
    },
}

pub struct ConnectParams {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
    /// Initial PTY size so the remote shell starts with the right geometry.
    pub cols: u16,
    pub rows: u16,
}

/// Commands sent from the UI task to the network task.
pub enum Cmd {
    Connect(ConnectParams),
    PtyInput(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
    },
    LocalList {
        path: String,
    },
    SftpInit,
    SftpList {
        path: String,
    },
    Transfer {
        id: u64,
        src: Endpoint,
        dst_dir: Endpoint,
    },
    Rename {
        pane: PaneKind,
        from: String,
        to: String,
    },
    Mkdir {
        pane: PaneKind,
        parent: String,
        name: String,
    },
    Delete {
        pane: PaneKind,
        path: String,
        is_dir: bool,
    },
    Disconnect,
}

/// Events sent from the network task back to the UI task.
pub enum Evt {
    Connected {
        host: String,
        port: u16,
        user: String,
    },
    ConnectFailed(String),
    /// The remote key is not in known_hosts. `answer` is used to reply to the
    /// blocked SSH handshake.
    HostKeyPrompt {
        host: String,
        port: u16,
        key_type: String,
        fingerprint: String,
        answer: oneshot::Sender<bool>,
    },
    HostKeyChanged {
        host: String,
        detail: String,
    },
    PtyData(Vec<u8>),
    PtyClosed(Option<u32>),
    LocalListing {
        path: String,
        entries: Vec<FsEntry>,
    },
    SftpListing {
        path: String,
        entries: Vec<FsEntry>,
    },
    SftpReady {
        cwd: String,
    },
    SftpFailed(String),
    Status(String),
    Error(String),
    Log(LogLevel, String),
    TransferStarted {
        id: u64,
        files: usize,
        bytes: u64,
    },
    TransferProgress {
        id: u64,
        files_done: usize,
        files_total: usize,
        bytes_done: u64,
        bytes_total: u64,
    },
    TransferDone {
        id: u64,
        ok: bool,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERR ",
        }
    }
}

/// A single directory entry, normalised for both local and remote panes.
#[derive(Debug, Clone)]
pub struct FsEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    /// Unix timestamp in seconds.
    pub modified: Option<i64>,
}

impl FsEntry {
    pub fn display_name(&self) -> String {
        if self.is_dir {
            format!("{}/", self.name)
        } else {
            self.name.clone()
        }
    }

    pub fn size_text(&self) -> String {
        if self.is_dir {
            "<DIR>".to_string()
        } else {
            human_size(self.size)
        }
    }

    pub fn date_text(&self) -> String {
        match self.modified {
            Some(ts) => chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
                .map(|d| d.format("%b %d %Y").to_string())
                .unwrap_or_else(|| "-".to_string()),
            None => "-".to_string(),
        }
    }
}

/// Directories first, then case-insensitive name.
pub fn sort_entries(entries: &mut [FsEntry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
}

pub fn human_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    if n < 1024 {
        return format!("{n}B");
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1}{}", UNITS[unit])
}

/// Join a POSIX remote path. Never uses [`PathBuf`]: on Windows that would
/// produce backslashes, which a POSIX server does not understand.
pub fn remote_join(base: &str, name: &str) -> String {
    if name.starts_with('/') {
        return name.to_string();
    }
    let base = if base.is_empty() { "/" } else { base };
    if base == "/" {
        format!("/{name}")
    } else if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

pub fn remote_parent(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => trimmed[..i].to_string(),
    }
}

pub fn remote_file_name(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(i) => trimmed[i + 1..].to_string(),
        None => trimmed.to_string(),
    }
}

pub fn local_join(cwd: &str, name: &str) -> String {
    Path::new(cwd).join(name).to_string_lossy().into_owned()
}

pub fn local_parent(path: &str) -> Option<String> {
    Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|p| !p.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_join_handles_root_and_nested() {
        assert_eq!(remote_join("/", "etc"), "/etc");
        assert_eq!(remote_join("/var", "www"), "/var/www");
        assert_eq!(remote_join("/var/", "www"), "/var/www");
        assert_eq!(remote_join("/var", "/abs"), "/abs");
        assert_eq!(remote_join("", "home"), "/home");
    }

    #[test]
    fn remote_parent_walks_to_root() {
        assert_eq!(remote_parent("/var/www/html"), "/var/www");
        assert_eq!(remote_parent("/var"), "/");
        assert_eq!(remote_parent("/"), "/");
        assert_eq!(remote_parent("/var/"), "/");
    }

    #[test]
    fn remote_file_name_extracts_last_segment() {
        assert_eq!(remote_file_name("/var/www/index.html"), "index.html");
        assert_eq!(remote_file_name("/var/www/"), "www");
        assert_eq!(remote_file_name("file.txt"), "file.txt");
    }

    #[test]
    fn human_sizes_are_readable() {
        assert_eq!(human_size(47), "47B");
        assert_eq!(human_size(2048), "2.0K");
        assert_eq!(human_size(1024 * 1024 * 3), "3.0M");
    }
}
