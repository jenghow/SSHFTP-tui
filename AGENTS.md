# AGENTS.md

Cross-platform TUI SSH + SFTP client: pure Rust (`russh`, `ratatui`, `vt100`),
no `libssh2`, no C library. The build still needs the platform's linker and
`ar` (MSVC on Windows, `build-essential` or `clang` + `binutils` on Linux),
because `rustc` cannot link without them and `ring` compiles a little C.
`README.md` covers building, running, logging in (password or private key),
features and keybindings; this file is the working context for changing the
code.

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
| Release (Linux/macOS) | `scripts/release.sh` |
| Release (Windows) | `powershell -ExecutionPolicy Bypass -File scripts/release.ps1` |
| Install (Linux, /opt + menu) | `scripts/install.sh` |

Order: `cargo fmt` → `cargo clippy --all-targets -- -D warnings` → `cargo test`.

On Linux the same cargo commands work; the only extra package is a C toolchain
(`build-essential`, or `clang` + `binutils`), because `rustc` invokes `cc` to
link and `ring`'s build script needs `ar`. A missing linker shows up as
``linker `cc` not found``, so install it before blaming the code.

Not a git repository: no commits, branches or history. Don't reach for `git`.

MSRV is `rust-version = "1.85"` with edition 2024 and no newer features
(let-chains arrive in 1.88). `std::env::set_var` is `unsafe` under edition 2024;
the e2e test uses it once, before any thread reads the variable.

## Don't "fix" these dependency pins

* `russh = { default-features = false, features = ["ring", "flate2", "rsa"] }`.
  The defaults pull `aws-lc-rs`, which needs CMake/NASM and a C toolchain. `rsa`
  is deliberately kept and is *not* an excuse to re-enable the defaults: it is
  the pure-Rust `rsa`/`pkcs1` pair, and without it RSA host keys and the RSA
  client keys the clouds hand out (EC2 `.pem`, `gcloud compute ssh`, Azure)
  fail to load or verify.
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
* **`~/.ssh/config` is parsed once, before the UI loop.** `sshconfig.rs` builds
  an in-memory `SshConfig`; `main::run` loads it and passes it to
  `App::set_configs`, so the UI task never reads the file (and a network
  `$HOME` cannot stall a frame). `App::apply_config` fills only fields still
  at their defaults (empty username, port `22`, `DEFAULT_KEY_PATH`), always
  replaces `Host` with the resolved `HostName` — so the form shows what will be
  dialled and `known_hosts` uses the hostname — and refuses
  `ProxyJump`/`ProxyCommand` with an error instead of dialling directly. The
  note under the fields names the file that was actually read.
* **Saved connections are app-owned and secret-free.** `saved.rs` stores
  `~/.config/sshftp-tui/connections` (`%APPDATA%` / Application Support on
  Windows/macOS, override `SSHFTP_TUI_CONNECTIONS`) in OpenSSH syntax.
  `main::run` loads it into a second `SshConfig`; `apply_config` checks it
  *before* `~/.ssh/config`, so an explicit save wins over defaults, and the
  source note names the saved file. The UI only reads memory:
  `Cmd::SaveConnection` runs `saved::append_at` in a spawned `spawn_blocking`
  and answers `Evt::ConnectionSaved` / `Evt::ConnectionSaveFailed`.
  `event::SavedConnection` has **no password or passphrase field**; the writer
  is append-only, creates the file 0600 (directory 0700) and refuses an exact
  alias literal (wildcards like `Host *` do not count, so they never block a
  save). Login arms `App::save_offer` only when `form.config` is `None` (the
  host is otherwise already described); F2 is intercepted only while that offer
  is `Some` and otherwise reaches the remote. `Esc` declines and clears it,
  `Evt::ConnectionSaved` clears it, and a failed write keeps it for a retry.
  `Evt::Connected`/`ConnectFailed`/`PtyClosed` also clear the pending profile.
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
  remote shell. `F10` quits and is checked *before* the modal dispatch, so a
  dialog can never trap the user; `Esc` quits on the connect form. `Shift+PgUp`/
  `PgDn`/`Home`/`End` are the other local exception — see the scrollback bullet.
  On the SFTP tab `F3`–`F9` are local: `F9` opens the chdir dialog, which is the
  only way to jump to a path that is not a child of the current listing. It
  resolves the typed value with `local_join`/`remote_join` and then
  `local_normalize`/`remote_normalize` (pure string folding, so `..` and
  `sub/./x` work and the remote path never grows a backslash), and sends the
  usual `Cmd::LocalList`/`Cmd::SftpList` — no new command, no filesystem probe
  on the UI task. `Ctrl+U` clears a dialog field. Also local to the SFTP tab:
  `/` opens the inline find field, `s` cycles Name→Size and `S` reverses the
  order. Those two are pure UI over the listing already delivered: `Pane` keeps
  the full `entries` and a `view` of indices (filter, then sort), so toggling
  them must never send a `Cmd`. Directories stay first in both sort keys, and a
  new directory clears the filter while refreshing the same one keeps it.
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

* `cargo test` must stay green on Linux as well as Windows. The path tests have
  `cfg(windows)` and `cfg(not(windows))` halves, and only the Windows one runs
  on the original dev box, so a wrong expectation there is easy to miss — run
  the suite on both before touching `event::local_*`.
* Unit tests cover key encoding, POSIX path helpers, size formatting,
  form/`TextField` (including bracketed IPv6), `sshconfig` parsing (patterns and
  negation, first-value-wins, `Include`, tokens, proxy detection; fixtures are
  `SshConfig::parse`/`load_at`, never the real file), saved-connection rendering,
  append/duplicate handling, alias validation and the no-secrets invariant
  (`saved::append_at` with scratch paths; never set the process-global env var),
  host-key policy using two
  embedded ed25519 keys, RSA private-key loading (an EC2-style PKCS#1 PEM in
  `network::tests`, which is the guard that `russh`'s `rsa` feature is still
  enabled — it fails with `UnsupportedKeyType` if it is dropped), and the render
  layer via `ratatui::backend::TestBackend` (asserts on `Buffer.content`
  symbols).
* `app.rs` has its own `mod tests` for key routing, because that logic is
  invisible to the render tests: the F6/F7 dialogs must survive a keystroke
  (`on_modal_key` takes the modal out of `App` and has to put it back), and
  `Shift+PgUp` must scroll locally without sending anything. Drive them through
  the public `on_key`/private `on_modal_key` with `futures::executor::block_on`
  plus a real `mpsc` channel, and `drain()` the channel first — `on_resize`
  queues a `Cmd::Resize`. The same module covers the session lifecycle:
  `Evt::PtyClosed` must land on a blank login form, `Evt::Connected` must clear
  the previous SFTP state, and a close with no live session must change nothing.
  It also covers the config prefill: leaving the Host field fills only untouched
  values, a typed user/port wins, and a `ProxyJump` alias must put an error in
  the form without queueing a `Cmd::Connect`. The save flow is here too: a
  successful key/password login captures a `SavedConnection` with no secret,
  `form.config` suppresses the offer, F2 is forwarded when no offer exists,
  Esc declines, a duplicate alias keeps the dialog open, and
  `ConnectionSaved`/`ConnectionSaveFailed` clear or keep the retry hint.
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
