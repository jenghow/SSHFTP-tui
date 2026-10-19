//! Filesystem operations and chunked transfers.
//!
//! Everything here runs inside tasks spawned by the network supervisor, so a
//! long directory walk or a multi-gigabyte copy can never block the UI or the
//! interactive PTY.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::event::{
    Endpoint, Evt, FsEntry, human_size, remote_file_name, remote_join, sort_entries,
};

const CHUNK_SIZE: usize = 32 * 1024;
const PROGRESS_STEP: u64 = 1024 * 1024;
const MAX_DEPTH: usize = 64;

// ---------------------------------------------------------------------------
// Directory listings
// ---------------------------------------------------------------------------

pub async fn local_list(path: &str) -> Result<Vec<FsEntry>> {
    let mut reader = tokio::fs::read_dir(path)
        .await
        .with_context(|| format!("reading local directory {path}"))?;
    let mut entries = Vec::new();

    while let Some(entry) = reader.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let file_type = entry.file_type().await?;
        let metadata = entry.metadata().await.ok();
        let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
        let modified = metadata
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
        entries.push(FsEntry {
            name,
            is_dir: file_type.is_dir(),
            is_symlink: file_type.is_symlink(),
            size,
            modified,
        });
    }

    sort_entries(&mut entries);
    Ok(entries)
}

pub async fn list_remote(sftp: &SftpSession, path: &str) -> Result<Vec<FsEntry>> {
    let reader = sftp
        .read_dir(path)
        .await
        .with_context(|| format!("reading remote directory {path}"))?;
    let mut entries = Vec::new();

    for entry in reader {
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        let metadata = entry.metadata();
        entries.push(FsEntry {
            name,
            is_dir: metadata.is_dir(),
            is_symlink: metadata.is_symlink(),
            size: metadata.size.unwrap_or(0),
            modified: metadata.mtime.map(i64::from),
        });
    }

    sort_entries(&mut entries);
    Ok(entries)
}

// ---------------------------------------------------------------------------
// Mutations
// ---------------------------------------------------------------------------

pub async fn mkdir_local(parent: &str, name: &str) -> Result<String> {
    let path = Path::new(parent).join(name);
    tokio::fs::create_dir(&path)
        .await
        .with_context(|| format!("creating directory {}", path.display()))?;
    Ok(path.to_string_lossy().into_owned())
}

pub async fn mkdir_remote(sftp: &SftpSession, parent: &str, name: &str) -> Result<String> {
    let path = remote_join(parent, name);
    sftp.create_dir(&path)
        .await
        .with_context(|| format!("creating remote directory {path}"))?;
    Ok(path)
}

pub async fn rename_local(from: &str, new_name: &str) -> Result<String> {
    let from_path = Path::new(from);
    let parent = from_path
        .parent()
        .ok_or_else(|| anyhow!("source has no parent directory"))?;
    let to = parent.join(new_name);
    tokio::fs::rename(from_path, &to)
        .await
        .with_context(|| format!("renaming {} to {}", from_path.display(), to.display()))?;
    Ok(to.to_string_lossy().into_owned())
}

pub async fn rename_remote(sftp: &SftpSession, from: &str, new_name: &str) -> Result<String> {
    let to = remote_join(&crate::event::remote_parent(from), new_name);
    sftp.rename(from, &to)
        .await
        .with_context(|| format!("renaming {from} to {to}"))?;
    Ok(to)
}

pub async fn delete_local(path: &str, is_dir: bool) -> Result<()> {
    if is_dir {
        tokio::fs::remove_dir_all(path)
            .await
            .with_context(|| format!("removing {path}"))?;
    } else {
        tokio::fs::remove_file(path)
            .await
            .with_context(|| format!("removing {path}"))?;
    }
    Ok(())
}

pub async fn delete_remote(sftp: &SftpSession, path: &str, is_dir: bool) -> Result<()> {
    if !is_dir {
        sftp.remove_file(path)
            .await
            .with_context(|| format!("removing {path}"))?;
        return Ok(());
    }

    // Iterative post-order traversal: push a "visited" marker, then children.
    let mut stack = vec![(path.to_string(), false)];
    while let Some((current, visited)) = stack.pop() {
        if visited {
            sftp.remove_dir(&current)
                .await
                .with_context(|| format!("removing directory {current}"))?;
            continue;
        }
        stack.push((current.clone(), true));
        let reader = sftp
            .read_dir(&current)
            .await
            .with_context(|| format!("listing {current}"))?;
        for entry in reader {
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let child = remote_join(&current, &name);
            let metadata = entry.metadata();
            if metadata.is_dir() && !metadata.is_symlink() {
                stack.push((child, false));
            } else {
                sftp.remove_file(&child)
                    .await
                    .with_context(|| format!("removing {child}"))?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Transfers
// ---------------------------------------------------------------------------

pub async fn transfer(
    sftp: Option<Arc<SftpSession>>,
    id: u64,
    src: Endpoint,
    dst_dir: Endpoint,
    tx: mpsc::Sender<Evt>,
) -> Result<String> {
    match (src, dst_dir) {
        (Endpoint::Local(src), Endpoint::Remote(dst)) => {
            let sftp = sftp.ok_or_else(|| anyhow!("SFTP session is not initialized"))?;
            local_to_remote(&sftp, id, &src, &dst, &tx).await
        }
        (Endpoint::Remote(src), Endpoint::Local(dst)) => {
            let sftp = sftp.ok_or_else(|| anyhow!("SFTP session is not initialized"))?;
            remote_to_local(&sftp, id, &src, &dst, &tx).await
        }
        _ => bail!("source and destination must be on different panes"),
    }
}

struct LocalItem {
    src: PathBuf,
    /// POSIX-style path relative to the destination root (includes the source
    /// directory name).
    rel: String,
    is_dir: bool,
    size: u64,
}

struct RemoteItem {
    remote: String,
    rel: String,
    is_dir: bool,
    size: u64,
}

async fn local_to_remote(
    sftp: &SftpSession,
    id: u64,
    src: &Path,
    dst_dir: &str,
    tx: &mpsc::Sender<Evt>,
) -> Result<String> {
    let metadata = tokio::fs::metadata(src)
        .await
        .with_context(|| format!("stat {}", src.display()))?;
    let root_name = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("invalid source path: {}", src.display()))?;

    let items = if metadata.is_dir() {
        collect_local(src).await?
    } else {
        vec![LocalItem {
            src: src.to_path_buf(),
            rel: root_name.clone(),
            is_dir: false,
            size: metadata.len(),
        }]
    };

    let files_total = items.iter().filter(|i| !i.is_dir).count();
    let bytes_total = items.iter().filter(|i| !i.is_dir).map(|i| i.size).sum();
    let _ = tx
        .send(Evt::TransferStarted {
            id,
            files: files_total,
            bytes: bytes_total,
        })
        .await;

    // Directories first so file creation never races the parent mkdir.
    for item in items.iter().filter(|i| i.is_dir) {
        let remote = remote_join(dst_dir, &item.rel);
        if !sftp.try_exists(&remote).await.unwrap_or(false) {
            sftp.create_dir(&remote)
                .await
                .with_context(|| format!("creating {remote}"))?;
        }
    }

    let mut progress = Progress::new(id, files_total, bytes_total, tx.clone());
    let mut overwritten = 0usize;

    for item in items.iter().filter(|i| !i.is_dir) {
        let remote = remote_join(dst_dir, &item.rel);
        if sftp.try_exists(&remote).await.unwrap_or(false) {
            overwritten += 1;
        }
        let mut source = tokio::fs::File::open(&item.src)
            .await
            .with_context(|| format!("opening {}", item.src.display()))?;
        let mut target = sftp
            .open_with_flags(
                &remote,
                OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
            )
            .await
            .with_context(|| format!("opening remote file {remote}"))?;

        let mut buffer = vec![0u8; CHUNK_SIZE];
        loop {
            let read = source.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            target.write_all(&buffer[..read]).await?;
            progress.add_bytes(read as u64).await;
        }
        target.shutdown().await?;
        progress.file_done().await;
    }

    let mut message = format!(
        "{} file(s), {} → {dst_dir}",
        files_total,
        human_size(bytes_total)
    );
    if overwritten > 0 {
        message.push_str(&format!(" ({overwritten} overwritten)"));
    }
    Ok(message)
}

async fn remote_to_local(
    sftp: &SftpSession,
    id: u64,
    src: &str,
    dst_dir: &Path,
    tx: &mpsc::Sender<Evt>,
) -> Result<String> {
    let metadata = sftp
        .metadata(src)
        .await
        .with_context(|| format!("stat {src}"))?;
    let root_name = remote_file_name(src);

    let items = if metadata.is_dir() {
        collect_remote(sftp, src).await?
    } else {
        vec![RemoteItem {
            remote: src.to_string(),
            rel: root_name.clone(),
            is_dir: false,
            size: metadata.size.unwrap_or(0),
        }]
    };

    let files_total = items.iter().filter(|i| !i.is_dir).count();
    let bytes_total = items.iter().filter(|i| !i.is_dir).map(|i| i.size).sum();
    let _ = tx
        .send(Evt::TransferStarted {
            id,
            files: files_total,
            bytes: bytes_total,
        })
        .await;

    for item in items.iter().filter(|i| i.is_dir) {
        let local = dst_dir.join(posix_to_relative(&item.rel));
        tokio::fs::create_dir_all(&local)
            .await
            .with_context(|| format!("creating {}", local.display()))?;
    }

    let mut progress = Progress::new(id, files_total, bytes_total, tx.clone());
    let mut overwritten = 0usize;

    for item in items.iter().filter(|i| !i.is_dir) {
        let local = dst_dir.join(posix_to_relative(&item.rel));
        if let Some(parent) = local.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        if local.exists() {
            overwritten += 1;
        }
        let mut source = sftp
            .open(&item.remote)
            .await
            .with_context(|| format!("opening {}", item.remote))?;
        let mut target = tokio::fs::File::create(&local)
            .await
            .with_context(|| format!("creating {}", local.display()))?;

        let mut buffer = vec![0u8; CHUNK_SIZE];
        loop {
            let read = source.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            target.write_all(&buffer[..read]).await?;
            progress.add_bytes(read as u64).await;
        }
        target.flush().await?;
        progress.file_done().await;
    }

    let mut message = format!(
        "{} file(s), {} → {}",
        files_total,
        human_size(bytes_total),
        dst_dir.display()
    );
    if overwritten > 0 {
        message.push_str(&format!(" ({overwritten} overwritten)"));
    }
    Ok(message)
}

async fn collect_local(root: &Path) -> Result<Vec<LocalItem>> {
    let root_name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("invalid source path: {}", root.display()))?;

    let mut items = Vec::new();
    let mut stack = vec![(root.to_path_buf(), root_name, 0usize)];

    while let Some((path, rel, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            continue;
        }
        // `metadata` follows symlinks; the depth cap guards against cycles.
        let metadata = tokio::fs::metadata(&path)
            .await
            .with_context(|| format!("stat {}", path.display()))?;
        if metadata.is_dir() {
            items.push(LocalItem {
                src: path.clone(),
                rel: rel.clone(),
                is_dir: true,
                size: 0,
            });
            let mut reader = tokio::fs::read_dir(&path)
                .await
                .with_context(|| format!("reading {}", path.display()))?;
            let mut children = Vec::new();
            while let Some(entry) = reader.next_entry().await? {
                let name = entry.file_name().to_string_lossy().into_owned();
                children.push((entry.path(), format!("{rel}/{name}"), depth + 1));
            }
            children.sort_by(|a, b| a.1.cmp(&b.1));
            for child in children.into_iter().rev() {
                stack.push(child);
            }
        } else {
            items.push(LocalItem {
                src: path,
                rel,
                is_dir: false,
                size: metadata.len(),
            });
        }
    }
    Ok(items)
}

async fn collect_remote(sftp: &SftpSession, root: &str) -> Result<Vec<RemoteItem>> {
    let mut items = Vec::new();
    let mut stack = vec![(root.to_string(), remote_file_name(root), 0usize)];

    while let Some((path, rel, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            continue;
        }
        let metadata = sftp
            .metadata(&path)
            .await
            .with_context(|| format!("stat {path}"))?;
        if metadata.is_dir() {
            items.push(RemoteItem {
                remote: path.clone(),
                rel: rel.clone(),
                is_dir: true,
                size: 0,
            });
            let reader = sftp
                .read_dir(&path)
                .await
                .with_context(|| format!("listing {path}"))?;
            let mut children = Vec::new();
            for entry in reader {
                let name = entry.file_name();
                if name == "." || name == ".." {
                    continue;
                }
                children.push((
                    remote_join(&path, &name),
                    format!("{rel}/{name}"),
                    depth + 1,
                ));
            }
            children.sort_by(|a, b| a.1.cmp(&b.1));
            for child in children.into_iter().rev() {
                stack.push(child);
            }
        } else {
            items.push(RemoteItem {
                remote: path,
                rel,
                is_dir: false,
                size: metadata.size.unwrap_or(0),
            });
        }
    }
    Ok(items)
}

/// Convert a POSIX relative path into a host-native relative path.
fn posix_to_relative(rel: &str) -> PathBuf {
    rel.split('/')
        .filter(|part| !part.is_empty())
        .fold(PathBuf::new(), |path, part| path.join(part))
}

struct Progress {
    id: u64,
    files_total: usize,
    files_done: usize,
    bytes_total: u64,
    bytes_done: u64,
    last_sent: u64,
    tx: mpsc::Sender<Evt>,
}

impl Progress {
    fn new(id: u64, files_total: usize, bytes_total: u64, tx: mpsc::Sender<Evt>) -> Self {
        Self {
            id,
            files_total,
            files_done: 0,
            bytes_total,
            bytes_done: 0,
            last_sent: 0,
            tx,
        }
    }

    async fn add_bytes(&mut self, bytes: u64) {
        self.bytes_done += bytes;
        if self.bytes_done - self.last_sent >= PROGRESS_STEP {
            self.last_sent = self.bytes_done;
            self.emit().await;
        }
    }

    async fn file_done(&mut self) {
        self.files_done += 1;
        if self.files_done % 4 == 0 || self.files_done == self.files_total {
            self.emit().await;
        }
    }

    async fn emit(&self) {
        let _ = self
            .tx
            .send(Evt::TransferProgress {
                id: self.id,
                files_done: self.files_done,
                files_total: self.files_total,
                bytes_done: self.bytes_done,
                bytes_total: self.bytes_total,
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_relative_paths_are_host_native() {
        let path = posix_to_relative("demo/sub/file.txt");
        assert_eq!(path, PathBuf::from("demo").join("sub").join("file.txt"));
    }
}
