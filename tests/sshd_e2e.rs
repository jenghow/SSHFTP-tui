//! End-to-end test against a real SSH/SFTP server.
//!
//! It drives the exact same code paths the TUI uses: `Cmd` values go into
//! `network::run`, `Evt` values come back out. Nothing is mocked.
//!
//! Two profiles are supported:
//!
//! * a public, read-only server such as `test.rebex.net` (RebexSSH + SFTP):
//!
//!   ```text
//!   $env:SSHFTP_TUI_TEST_HOST = 'test.rebex.net'
//!   $env:SSHFTP_TUI_TEST_USER = 'demo'
//!   $env:SSHFTP_TUI_TEST_PASSWORD = 'password'
//!   $env:SSHFTP_TUI_TEST_READONLY = '1'
//!   cargo test --test sshd_e2e -- --ignored --nocapture
//!   ```
//!
//! * the local OpenSSH container, which also exercises uploads, recursive
//!   copies, mkdir, rename and delete:
//!
//!   ```text
//!   pwsh -File scripts/test-server.ps1 test
//!   ```
//!
//! Environment variables:
//!
//! | Variable                   | Default             |
//! | -------------------------- | ------------------- |
//! | `SSHFTP_TUI_TEST_HOST`     | `127.0.0.1`         |
//! | `SSHFTP_TUI_TEST_PORT`     | `2222`              |
//! | `SSHFTP_TUI_TEST_USER`     | `tester`            |
//! | `SSHFTP_TUI_TEST_PASSWORD` | `testpass`          |
//! | `SSHFTP_TUI_TEST_READONLY` | unset (read-write)  |
//!
//! The test is `#[ignore]`d so `cargo test` stays green without a server.

use std::path::PathBuf;
use std::time::Duration;

use sshftp_tui::event::{
    AuthMethod, Cmd, ConnectParams, Endpoint, Evt, FsEntry, PaneKind, remote_join, remote_parent,
};
use sshftp_tui::network;
use tokio::sync::mpsc;
use tokio::time::Instant;

const COLS: u16 = 120;
const ROWS: u16 = 40;
const EVENT_TIMEOUT: Duration = Duration::from_secs(60);

/// Unrelated ed25519 key: writing it into known_hosts forces the
/// "host key changed" refusal.
const WRONG_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIObNYhxzK5ayt0Cjj3i4hL9NgFq2CcWTIsLHwzLXB/uA";

fn endpoint() -> (String, u16) {
    let host = std::env::var("SSHFTP_TUI_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = std::env::var("SSHFTP_TUI_TEST_PORT")
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(2222);
    (host, port)
}

fn credentials() -> (String, String) {
    let user = std::env::var("SSHFTP_TUI_TEST_USER").unwrap_or_else(|_| "tester".to_string());
    let password =
        std::env::var("SSHFTP_TUI_TEST_PASSWORD").unwrap_or_else(|_| "testpass".to_string());
    (user, password)
}

/// Read-only servers (public demo hosts) have no writable directory, so the
/// upload / mkdir / rename / delete phases are skipped.
fn read_only() -> bool {
    std::env::var("SSHFTP_TUI_TEST_READONLY")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

struct ConnectOutcome {
    ok: Result<(), String>,
    prompted: bool,
    key_changed: bool,
}

impl ConnectOutcome {
    fn new() -> Self {
        Self {
            ok: Err("the connection attempt never completed".to_string()),
            prompted: false,
            key_changed: false,
        }
    }

    fn expect_ok(&self) {
        if let Err(message) = &self.ok {
            panic!("connection failed: {message}");
        }
    }
}

struct Harness {
    tx: mpsc::Sender<Cmd>,
    rx: mpsc::Receiver<Evt>,
    task: Option<tokio::task::JoinHandle<()>>,
    sftp_ready_seen: bool,
    sftp_listing_seen: bool,
}

impl Harness {
    fn spawn() -> Self {
        let (tx, rx_cmd) = mpsc::channel(256);
        let (tx_evt, rx) = mpsc::channel(8192);
        let task = tokio::spawn(network::run(rx_cmd, tx_evt));
        Self {
            tx,
            rx,
            task: Some(task),
            sftp_ready_seen: false,
            sftp_listing_seen: false,
        }
    }

    /// Surface the network task's exit (including its panic) instead of a
    /// confusing `SendError`.
    async fn ensure_alive(&mut self) {
        let finished = self.task.as_ref().is_some_and(|task| task.is_finished());
        if !finished {
            return;
        }
        let task = self.task.take().expect("task handle present");
        match task.await {
            Ok(()) => panic!("the network task exited on its own"),
            Err(error) => std::panic::resume_unwind(error.into_panic()),
        }
    }

    async fn send(&mut self, cmd: Cmd) {
        self.ensure_alive().await;
        self.tx.send(cmd).await.expect("network task is alive");
    }

    async fn next_event(&mut self, what: &str) -> Evt {
        let event = match tokio::time::timeout(EVENT_TIMEOUT, self.rx.recv()).await {
            Ok(Some(event)) => event,
            Ok(None) => panic!("network task ended while waiting for {what}"),
            Err(_) => panic!("timed out waiting for {what}"),
        };
        match &event {
            Evt::SftpReady { .. } => self.sftp_ready_seen = true,
            Evt::SftpListing { .. } => self.sftp_listing_seen = true,
            _ => {}
        }
        event
    }

    async fn wait_for<F>(&mut self, what: &str, mut predicate: F) -> Evt
    where
        F: FnMut(&Evt) -> bool,
    {
        loop {
            let event = self.next_event(what).await;
            if predicate(&event) {
                return event;
            }
            match &event {
                Evt::Error(message) => {
                    panic!("unexpected error while waiting for {what}: {message}")
                }
                Evt::ConnectFailed(message) => {
                    panic!("connection failed while waiting for {what}: {message}")
                }
                _ => {}
            }
        }
    }

    async fn connect(&mut self, accept_host_key: bool) -> ConnectOutcome {
        let mut outcome = ConnectOutcome::new();
        let (host, port) = endpoint();
        let (username, password) = credentials();
        self.send(Cmd::Connect(ConnectParams {
            host: host.clone(),
            port,
            username,
            auth: AuthMethod::Password(password),
            cols: COLS,
            rows: ROWS,
        }))
        .await;

        loop {
            let event = self.next_event("connection attempt").await;
            match event {
                Evt::Connected { .. } => {
                    outcome.ok = Ok(());
                    return outcome;
                }
                Evt::ConnectFailed(message) => {
                    outcome.ok = Err(message);
                    return outcome;
                }
                Evt::HostKeyPrompt { answer, .. } => {
                    outcome.prompted = true;
                    let _ = answer.send(accept_host_key);
                }
                Evt::HostKeyChanged { .. } => outcome.key_changed = true,
                _ => {}
            }
        }
    }

    /// Accumulate PTY output until `needle` shows up or `timeout` expires.
    /// Returns the captured bytes and whether the needle was found.
    async fn collect_pty_until(&mut self, needle: &[u8], timeout: Duration) -> (Vec<u8>, bool) {
        let deadline = Instant::now() + timeout;
        let mut buffer = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let found = contains(&buffer, needle);
                return (buffer, found);
            }
            match tokio::time::timeout(remaining, self.rx.recv()).await {
                Ok(Some(Evt::PtyData(data))) => {
                    buffer.extend_from_slice(&data);
                    if contains(&buffer, needle) {
                        return (buffer, true);
                    }
                }
                Ok(Some(Evt::Error(message))) => {
                    panic!("error while reading PTY output: {message}")
                }
                Ok(Some(_)) => {}
                Ok(None) => panic!("network task ended while reading PTY output"),
                Err(_) => {
                    let found = contains(&buffer, needle);
                    return (buffer, found);
                }
            }
        }
    }

    async fn transfer(&mut self, id: u64, src: Endpoint, dst_dir: Endpoint) {
        self.send(Cmd::Transfer { id, src, dst_dir }).await;
        let event = self
            .wait_for(
                "transfer result",
                |event| matches!(event, Evt::TransferDone { id: got, .. } if *got == id),
            )
            .await;
        match event {
            Evt::TransferDone { ok, message, .. } => assert!(ok, "transfer failed: {message}"),
            _ => unreachable!(),
        }
    }

    async fn list_remote(&mut self, path: &str) -> Vec<FsEntry> {
        let path = path.to_string();
        self.send(Cmd::SftpList { path: path.clone() }).await;
        self.await_listing(&path).await
    }

    async fn mkdir_remote(&mut self, parent: &str, name: &str) -> Vec<FsEntry> {
        let parent = parent.to_string();
        self.send(Cmd::Mkdir {
            pane: PaneKind::Remote,
            parent: parent.clone(),
            name: name.to_string(),
        })
        .await;
        self.await_listing(&parent).await
    }

    async fn rename_remote(&mut self, from: &str, to: &str) -> Vec<FsEntry> {
        let parent = remote_parent(from);
        self.send(Cmd::Rename {
            pane: PaneKind::Remote,
            from: from.to_string(),
            to: to.to_string(),
        })
        .await;
        self.await_listing(&parent).await
    }

    async fn delete_remote(&mut self, path: &str, is_dir: bool) -> Vec<FsEntry> {
        let parent = remote_parent(path);
        self.send(Cmd::Delete {
            pane: PaneKind::Remote,
            path: path.to_string(),
            is_dir,
        })
        .await;
        self.await_listing(&parent).await
    }

    /// Mutations report success with a status message and then push a fresh
    /// listing; skip the status and return the listing.
    async fn await_listing(&mut self, path: &str) -> Vec<FsEntry> {
        let path = path.to_string();
        let event = self
            .wait_for(
                "sftp listing",
                |event| matches!(event, Evt::SftpListing { path: got, .. } if got.as_str() == path),
            )
            .await;
        match event {
            Evt::SftpListing { entries, .. } => entries,
            _ => unreachable!(),
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn has(entries: &[FsEntry], name: &str) -> bool {
    entries.iter().any(|entry| entry.name == name)
}

fn names(entries: &[FsEntry]) -> Vec<String> {
    entries.iter().map(|entry| entry.name.clone()).collect()
}

fn scratch_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("sshftp-tui-e2e-{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn pseudo_random(len: usize) -> Vec<u8> {
    let mut state: u32 = 0x1234_5678;
    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes.truncate(len);
    bytes
}

/// Change one character of a base64 key blob, keeping it the same key type but
/// a different key value. Used to fake "the server's key changed" for whatever
/// algorithm the server actually negotiated.
fn corrupt_base64(blob: &str) -> String {
    let mut chars: Vec<char> = blob.chars().collect();
    let index = chars.len() / 2;
    chars[index] = if chars[index] == 'A' { 'B' } else { 'A' };
    chars.into_iter().collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a reachable SSH/SFTP server"]
async fn full_client_flow_against_real_ssh_server() {
    let scratch = scratch_dir("run");
    let known_hosts = scratch.join("known_hosts");
    let (host, port) = endpoint();
    let read_only = read_only();
    println!("target {host}:{port} (read_only={read_only})");

    // SAFETY: single test in this binary; set before the network task performs
    // its first known_hosts lookup.
    unsafe {
        std::env::set_var("SSHFTP_TUI_KNOWN_HOSTS", &known_hosts);
    }

    let mut h = Harness::spawn();

    // --- 1. unknown host key -> prompt -> accept -> learned ------------------
    let first = h.connect(true).await;
    first.expect_ok();
    assert!(
        first.prompted,
        "first connection must prompt for the unknown host key"
    );
    assert!(!first.key_changed);
    assert!(
        known_hosts.exists(),
        "accepted key must be written to known_hosts"
    );

    // --- 2. SFTP is *not* initialised by connecting -------------------------
    assert!(
        !h.sftp_ready_seen && !h.sftp_listing_seen,
        "the SFTP subsystem must stay closed until explicitly requested (F3)"
    );

    // --- 3. PTY: term type, geometry and live output ------------------------
    tokio::time::sleep(Duration::from_millis(800)).await;
    let command = "printf 'TERM=%s\\n' \"$TERM\"; printf 'SIZE=%s\\n' \"$(stty size)\"; printf 'MARK=%s\\n' $((6*7))\n";
    h.send(Cmd::PtyInput(command.as_bytes().to_vec())).await;
    let pty_wait = if read_only {
        Duration::from_secs(12)
    } else {
        Duration::from_secs(45)
    };
    let (pty, got_marker) = h.collect_pty_until(b"MARK=42", pty_wait).await;
    let text = String::from_utf8_lossy(&pty).into_owned();
    if got_marker {
        assert!(
            text.contains("TERM=xterm-256color"),
            "PTY TERM was not xterm-256color:\n{text}"
        );
        assert!(
            text.contains(&format!("SIZE={ROWS} {COLS}")),
            "PTY geometry was not {ROWS}x{COLS}:\n{text}"
        );
    } else {
        // Public demo accounts may not run shell commands at all.
        assert!(
            read_only,
            "the server never ran the marker command:\n{text}"
        );
        println!("note: the demo account did not run shell commands; PTY output seen:\n{text}");
    }

    // --- 4. on-demand SFTP initialisation ----------------------------------
    h.send(Cmd::SftpInit).await;
    let ready = h
        .wait_for("SftpReady", |event| matches!(event, Evt::SftpReady { .. }))
        .await;
    let cwd = match ready {
        Evt::SftpReady { cwd } => cwd,
        _ => unreachable!(),
    };
    assert!(
        cwd.starts_with('/'),
        "canonicalised cwd should be absolute, got {cwd}"
    );
    let listing = h.list_remote(&cwd).await;
    println!("remote {cwd}: {:?}", names(&listing));

    // --- 5. download a real file -------------------------------------------
    let download_dir = scratch.join("download");
    std::fs::create_dir_all(&download_dir).expect("create download dir");
    let target = listing
        .iter()
        .find(|entry| !entry.is_dir && entry.size > 0)
        .cloned()
        .expect("the server exposes at least one non-empty file");
    h.transfer(
        1,
        Endpoint::Remote(remote_join(&cwd, &target.name)),
        Endpoint::Local(download_dir.clone()),
    )
    .await;
    let downloaded = std::fs::metadata(download_dir.join(&target.name))
        .expect("the downloaded file exists locally");
    assert_eq!(
        downloaded.len(),
        target.size,
        "downloaded size differs from the size reported by the listing"
    );
    assert!(downloaded.len() > 0, "the downloaded file is empty");

    // --- 6. upload / recursive copy / mkdir / rename / delete --------------
    if !read_only {
        let source = scratch.join("single.txt");
        let payload = pseudo_random(64 * 1024);
        std::fs::write(&source, &payload).expect("write source file");

        h.transfer(
            2,
            Endpoint::Local(source.clone()),
            Endpoint::Remote(cwd.clone()),
        )
        .await;
        let listing = h.list_remote(&cwd).await;
        assert!(
            has(&listing, "single.txt"),
            "uploaded file missing: {:?}",
            names(&listing)
        );

        let back = scratch.join("back");
        std::fs::create_dir_all(&back).expect("create back dir");
        h.transfer(
            3,
            Endpoint::Remote(remote_join(&cwd, "single.txt")),
            Endpoint::Local(back.clone()),
        )
        .await;
        let round_tripped = std::fs::read(back.join("single.txt")).expect("read downloaded file");
        assert_eq!(
            round_tripped, payload,
            "single-file round trip corrupted the payload"
        );

        // Recursive copy in both directions, with a payload large enough to
        // exercise chunking and progress reporting.
        let tree = scratch.join("tree");
        std::fs::create_dir_all(tree.join("nested/deeper")).expect("create tree");
        let big = pseudo_random(1_572_872); // 1.5 MiB
        std::fs::write(tree.join("top.txt"), b"top level\n").expect("write top.txt");
        std::fs::write(tree.join("nested/deeper/big.bin"), &big).expect("write big.bin");

        h.transfer(
            4,
            Endpoint::Local(tree.clone()),
            Endpoint::Remote(cwd.clone()),
        )
        .await;

        let listing = h.list_remote(&cwd).await;
        assert!(
            has(&listing, "tree"),
            "recursive upload missing: {:?}",
            names(&listing)
        );
        let tree_remote = remote_join(&cwd, "tree");
        let nested = h.list_remote(&tree_remote).await;
        assert!(
            has(&nested, "nested") && has(&nested, "top.txt"),
            "bad tree listing: {:?}",
            names(&nested)
        );
        let deeper = h
            .list_remote(&remote_join(&tree_remote, "nested/deeper"))
            .await;
        assert!(
            has(&deeper, "big.bin"),
            "bad nested listing: {:?}",
            names(&deeper)
        );

        let tree_back = scratch.join("tree-back");
        std::fs::create_dir_all(&tree_back).expect("create tree-back dir");
        h.transfer(
            5,
            Endpoint::Remote(tree_remote.clone()),
            Endpoint::Local(tree_back.clone()),
        )
        .await;
        let big_back =
            std::fs::read(tree_back.join("tree/nested/deeper/big.bin")).expect("read big.bin");
        assert_eq!(big_back, big, "recursive round trip corrupted big.bin");
        assert_eq!(
            std::fs::read(tree_back.join("tree/top.txt")).expect("read top.txt"),
            b"top level\n"
        );

        // mkdir / rename / delete on the remote side.
        let listing = h.mkdir_remote(&cwd, "fresh-dir").await;
        assert!(
            has(&listing, "fresh-dir"),
            "mkdir did not appear: {:?}",
            names(&listing)
        );

        let listing = h
            .rename_remote(&remote_join(&cwd, "fresh-dir"), "renamed-dir")
            .await;
        assert!(
            has(&listing, "renamed-dir") && !has(&listing, "fresh-dir"),
            "rename did not apply: {:?}",
            names(&listing)
        );

        let listing = h
            .delete_remote(&remote_join(&cwd, "renamed-dir"), true)
            .await;
        assert!(
            !has(&listing, "renamed-dir"),
            "delete did not apply: {:?}",
            names(&listing)
        );

        // Recursive delete, then the plain files.
        let listing = h.delete_remote(&tree_remote, true).await;
        assert!(
            !has(&listing, "tree"),
            "recursive delete did not apply: {:?}",
            names(&listing)
        );
        let listing = h
            .delete_remote(&remote_join(&cwd, "single.txt"), false)
            .await;
        assert!(
            !has(&listing, "single.txt"),
            "file delete did not apply: {:?}",
            names(&listing)
        );
    }

    // --- 7. reconnect: the remembered key means no prompt ------------------
    h.send(Cmd::Disconnect).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    h.sftp_ready_seen = false;
    h.sftp_listing_seen = false;

    let second = h.connect(true).await;
    second.expect_ok();
    assert!(
        !second.prompted,
        "known host key must not prompt a second time (strict host key checking)"
    );
    assert!(
        !h.sftp_ready_seen,
        "a fresh connection must not carry the old SFTP session"
    );

    // --- 8. a different key for the same host is refused -------------------
    // Reuse the host pattern the client wrote verbatim (port 22 is recorded as
    // a bare hostname, other ports as `[host]:port`) and keep the algorithm
    // that was negotiated, so the entry is really compared and reported as
    // changed rather than being treated as a new algorithm.
    let learned = std::fs::read_to_string(&known_hosts).expect("known_hosts is readable");
    println!("learned known_hosts:\n{}", learned.trim());
    let mut fields = learned
        .lines()
        .find(|line| !line.trim().is_empty())
        .expect("known_hosts records the accepted key")
        .split_whitespace();
    let pattern = fields.next().expect("a host pattern").to_string();
    let learned_type = fields.next().expect("a key type").to_string();
    let learned_blob = fields.next().expect("a key blob").to_string();
    let wrong_key = if learned_type == "ssh-ed25519" {
        WRONG_KEY.to_string()
    } else {
        format!("{learned_type} {}", corrupt_base64(&learned_blob))
    };
    println!("replacing {learned_type} for {pattern} with a different key");
    std::fs::write(&known_hosts, format!("{pattern} {wrong_key}\n")).expect("write known_hosts");
    h.send(Cmd::Disconnect).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let third = h.connect(true).await;
    assert!(
        third.key_changed,
        "a mismatched host key must be reported as changed"
    );
    assert!(
        third.ok.is_err(),
        "a mismatched host key must abort the connection"
    );
    assert!(
        !third.prompted,
        "a mismatched host key must never be overridable"
    );
    println!("changed-key refusal message: {:?}", third.ok);

    let _ = std::fs::remove_dir_all(&scratch);
}
