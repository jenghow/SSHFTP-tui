# AGENTS.md

Cross-platform TUI SSH + SFTP client: pure Rust (`russh`, `ratatui`, `vt100`),
no `libssh2`, no C toolchain. `README.md` covers building, running, logging in
(password or private key), features and keybindings; this file is the working
context for changing the code.

## Commands

The shell here is Windows PowerShell 5.1 — `pwsh` is *not* installed. Native
stderr becomes a terminating `NativeCommandError`, so wrap cargo to see both
output and exit code:

```powershell
cmd /c "cargo test 2>&1" | Select-Object -Last 40; "EXIT=$LASTEXITCODE"
```

| Task | Command |
| ---- | ------- |
| Build | `cargo build --release` |
| Unit tests (no server needed) | `cargo test` |
| One test | `cargo test hostkey::tests::` |
| Lint gate (currently clean) | `cargo clippy --all-targets -- -D warnings` |
| Format | `cargo fmt` |
| End-to-end (ignored by default) | `cargo test --test sshd_e2e -- --ignored --nocapture` |

Order: `cargo fmt` → `cargo clippy --all-targets -- -D warnings` → `cargo test`.

Not a git repository: no commits, branches or history. Don't reach for `git`.

MSRV is `rust-version = "1.85"` with edition 2024 and no newer features
(let-chains arrive in 1.88). `std::env::set_var` is `unsafe` under edition 2024;
the e2e test uses it once, before any thread reads the variable.

## Don't "fix" these dependency pins

* `russh = { default-features = false, features = ["ring", "flate2"] }`. The
  defaults pull `aws-lc-rs`, which needs CMake/NASM and a C toolchain.
* `crossterm = "0.29"` must stay in step with the crossterm `ratatui 0.30`
  depends on; bumping one alone puts two versions in the graph.
* No `tui-input`: `form::TextField` (char-index cursor) is hand-written.
* `russh-sftp`: `SftpSession::read_dir(path)` is `await`ed once and then iterated
  **synchronously** (`for entry in reader`), so a huge directory blocks inside
  that loop — which is why listings run in spawned tasks.
  `SftpSession::new(channel.into_stream())` is transport-agnostic. In russh 0.64
  the resize call is `window_change` (`channel_window_change_request` is gone).
* known_hosts/key helpers come from `russh::keys::{PublicKey, known_hosts}`
  (`check_known_hosts_path`, `learn_known_hosts_path`).

## Architecture

* **lib + thin bin.** The crate is `src/lib.rs` (`pub mod …`); `src/main.rs`
  only installs the terminal guard/panic hook and wires channels. Add new
  modules to `lib.rs` so `tests/` can reach them.
* **Two I/O owners, MPSC only.** The UI task (`main.rs`, `app.rs`, `ui.rs`) and
  `network::run` exchange `event::{Cmd, Evt}` and nothing else. Never do
  network/disk I/O on the UI task, never draw from the network task.
  Integration tests drive `network::run` directly over channels.
* `app.rs` holds all state and key routing; `ui.rs` is pure rendering.
* **Remote paths are POSIX strings** — `event::{remote_join, remote_parent,
  remote_file_name}`. `PathBuf` is for local paths only, so a Windows client
  can never send `C:\…` to a server.
* **SFTP is on demand.** `Cmd::SftpInit` is sent only from the F3 handler;
  `connect()` must never open the subsystem. The right pane keeps showing
  `Press [F3] to initialize SFTP session` until `SftpState::Ready`, and the e2e
  test fails if any `SftpReady`/`SftpListing` arrives before `SftpInit`.
* **`Cmd::Disconnect` closes the session but must not end `network::run`**; that
  task exits when the command channel closes. Reconnect (and the e2e reconnect
  step) depends on this.
* **A session that ends drops back to the login form.** `Evt::PtyClosed` (remote
  `exit`/`logout`, dropped transport) runs `App::back_to_login` when
  `App::connected` is set: blank `ConnectForm`, the reason in `form.error` (the
  connect screen has no status footer), and the session-scoped state — `sftp`,
  `remote`, `transfer`, `host_label` — cleared. The `connected` guard matters: a
  late `PtyClosed` from an old reader task must not eject the session that
  replaced it. `Evt::Connected` clears the same state, because the SFTP subsystem
  belongs to the connection, and `submit_connect` rebuilds the `vt100::Parser`
  so a new session starts on a blank grid with no history.
* PTY: a reader task owns `ChannelReadHalf::wait()` while the supervisor keeps
  the write half. `SftpSession` sits behind `Arc` and each request is spawned so
  I/O never blocks the UI. Transfers use 32 KiB chunks, report progress every
  1 MiB, and cap recursion at depth 64.
* Geometry: `App::on_resize` derives the PTY as window −2 cols / −6 rows
  (floors 20×5); `ui::draw_terminal` passes the tab's inner area to the same
  `fit_pty`. Change one and the other must follow.
* Keys: `Ctrl+T` is intercepted globally; `Ctrl+C` and everything else go to the
  remote shell. Only `F10` (and `Esc` on the connect form) quit. `Shift+PgUp`/
  `PgDn`/`Home`/`End` are the other local exception — see the scrollback bullet.
  On the SFTP tab `F3`–`F9` are local: `F9` opens the chdir dialog, which is the
  only way to jump to a path that is not a child of the current listing. It
  resolves the typed value with `local_join`/`remote_join` and then
  `local_normalize`/`remote_normalize` (pure string folding, so `..` and
  `sub/./x` work and the remote path never grows a backslash), and sends the
  usual `Cmd::LocalList`/`Cmd::SftpList` — no new command, no filesystem probe
  on the UI task. `Ctrl+U` clears a dialog field.
* **Terminal scrollback lives in the parser, not in `App`.** `vt100::Parser` is
  built with 2000 lines of history and the view offset is driven by
  `parser.screen_mut().set_scrollback(n)`, which clamps to what exists (use
  `usize::MAX` for "oldest", never `scrollback_len()` — that is the *capacity*).
  `ui::draw_terminal` renders whatever offset is active, so it must only drop
  the caret and relabel the title; plain `PgUp`/`PgDn` stay forwarded to the
  remote. The alternate grid has no scrollback, and `submit_connect` clears the
  history so sessions don't share it.
* **The SSH caret is drawn, not requested.** `draw_terminal` adds
  `Modifier::REVERSED` to the cell under `screen.cursor_position()` and never
  calls `frame.set_cursor_position`, so the hardware cursor is hidden on that
  tab. `App::blink_on` is the visible half of the blink: `main::ui_loop` flips it
  from a 500 ms interval and keystrokes and `Evt::PtyData` set it back to `true`.
  Use the modifier, not a fg/bg swap — two `Reset` colors would render nothing.
  The block is skipped when the remote hid the cursor or the view is scrolled
  back. The connect form and dialogs still use the hardware cursor, which
  `TerminalGuard` asks to be a blinking block.

## Host keys

* Policy (`hostkey.rs`): present and matching → connect; absent → prompt, append
  on accept; different → refuse via `Evt::HostKeyChanged` with no override. Any
  `Err` from `check_known_hosts_path` counts as changed (fail closed).
* Path: `SSHFTP_TUI_KNOWN_HOSTS`, else `~/.ssh/known_hosts`.
* Unit tests must call `check_at`/`learn_at` with an explicit path — never set
  the env var there, it is process-global and racy.
* **The spelling depends on the port**: bare `host` for 22, `[host]:port`
  otherwise. A test that hardcodes one form silently misses the lookup; the e2e
  suite reuses the pattern from the learned line for that reason.

## Tests

* Unit tests cover key encoding, POSIX path helpers, size formatting,
  form/`TextField` (including bracketed IPv6), host-key policy using two
  embedded ed25519 keys, and the render layer via `ratatui::backend::TestBackend`
  (asserts on `Buffer.content` symbols).
* `app.rs` has its own `mod tests` for key routing, because that logic is
  invisible to the render tests: the F6/F7 dialogs must survive a keystroke
  (`on_modal_key` takes the modal out of `App` and has to put it back), and
  `Shift+PgUp` must scroll locally without sending anything. Drive them through
  the public `on_key`/private `on_modal_key` with `futures::executor::block_on`
  plus a real `mpsc` channel, and `drain()` the channel first — `on_resize`
  queues a `Cmd::Resize`. The same module covers the session lifecycle:
  `Evt::PtyClosed` must land on a blank login form, `Evt::Connected` must clear
  the previous SFTP state, and a close with no live session must change nothing.
* `tests/sshd_e2e.rs` is `#[ignore]`d and needs a server. Env:
  `SSHFTP_TUI_TEST_HOST` / `_PORT` / `_USER` / `_PASSWORD` (defaults
  `127.0.0.1:2222`, `tester`/`testpass`) and `_READONLY`.
  * Read-only against the public demo — `test.rebex.net:22`, `demo`/`password`,
    `SSHFTP_TUI_TEST_READONLY=1` — no container. That account is a restricted
    `-sh` that rejects commands like `printf`, so PTY assertions are relaxed in
    this mode.
  * Read-write against the local container:
    `powershell -NoProfile -ExecutionPolicy Bypass -File scripts/test-server.ps1 test`
    (starts it, resolves the endpoint, runs the suite).
* On Windows the container's published port is **not** on `127.0.0.1`. Get the
  machine address from `podman machine ssh "ip route get 1.1.1.1"` and read
  `src <ip>`. Do not parse `ip addr`: the WSL machine's loopback
  (`10.255.255.254`) is a global address and is listed first.
* `tests/sshd` is a Debian-based OpenSSH image (`ARG BASE_IMAGE`, defaulted to
  the local `.NET` runtime image because `docker.io` pulls are TLS-intercepted
  here). It uses `Subsystem sftp internal-sftp` and `LogLevel VERBOSE`, so
  `podman logs` shows the subsystem being opened on the *same* connection after
  the shell — the proof that SFTP is on demand.

## This machine (corporate TLS interception)

* HTTPS is MITM'd: `docker.io` pulls and `sh.rustup.rs` fail with certificate
  errors, while `static.rust-lang.org` and `static.crates.io` work only once the
  corporate CA is trusted. A containerized Linux build needs that CA plus
  `CARGO_HTTP_CAINFO=<ca.pem>`.
* `cargo --offline` is not a shortcut here: the Windows host never downloads
  Linux-only crates (`time` pulls `num_threads`), so Linux builds must be
  allowed to fetch.
