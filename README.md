# sshftp-tui

A cross-platform (Windows / Linux / macOS) **TUI SSH + SFTP client** written in
pure Rust. One window, three tabs: an interactive terminal, a Midnight
Commander style dual-pane file manager, and a log view.

No external system libraries: no `libssh2`, no OpenSSL, no C toolchain beyond
what the Rust toolchain already ships.

```text
┌ sshftp-tui ─────────────────────────────────────────────────────────────┐
│ [1] Terminal │ [2] SFTP │ [3] Logs                                       │
├─────────────────────────────────────────────────────────────────────────┤
│  user@example.com:~$ ls -la                                             │
│  drwxr-xr-x  4 user user  4096 Oct 19 09:12 .                           │
├─────────────────────────────────────────────────────────────────────────┤
│ [Ctrl+T] Tab Change   [F10] Exit                    Connected to host   │
└─────────────────────────────────────────────────────────────────────────┘
```

## Highlights

* **Real PTY.** `xterm-256color` is requested from the server; output is parsed
  by `vt100` and rendered cell by cell, so colours, attributes, wide glyphs and
  cursor placement behave like a normal terminal. The caret is drawn as a
  blinking block by the app itself, so it looks the same in every emulator.
  Terminal resizes propagate to the remote shell (SIGWINCH) via a channel
  window-change request.
* **SFTP strictly on demand.** Connecting opens an SSH session and a shell
  channel and *nothing else*. The SFTP subsystem channel is opened only when you
  press `F3`; until then the right pane shows
  `Press [F3] to initialize SFTP session`.
* **Strict host key checking.** `known_hosts` is authoritative: a match connects,
  an unknown key produces a fingerprint prompt (accept = trust on first use),
  and a *changed* key is refused outright with no way to click through it.
* **Non-blocking.** The UI never performs I/O. Every directory walk, upload and
  download runs in its own task holding an `Arc<SftpSession>`, and progress is
  reported back over a channel, so a multi-gigabyte copy cannot freeze the
  terminal or the foreground shell.
* **Recursive transfers** in both directions, chunked with 32 KiB buffers.

## Requirements

* **Rust 1.85 or newer** (edition 2024). Install it from <https://rustup.rs>; on
  Windows accept the MSVC prerequisites it offers, and nothing else is needed.
* **A 256-colour terminal.** On Windows use Windows Terminal; the legacy
  `conhost` console renders the TUI poorly.
* **Nothing else.** No C toolchain, no `pkg-config`, no system OpenSSL, no
  `libssh2`. `russh` is pinned to
  `default-features = false, features = ["ring", "flate2", "rsa"]`, which drops
  `aws-lc-rs` (CMake/NASM) while keeping RSA via the pure-Rust `rsa` crates.

## Build

```sh
cd sshftp-tui
cargo build --release
```

The binary is written to `target/release/sshftp-tui`
(`target\release\sshftp-tui.exe` on Windows). To put it on your `PATH`:

```sh
cargo install --path .
```

A debug build is the same command without `--release`: it compiles much faster
and runs noticeably slower. The commands are identical on Windows, Linux and
macOS — there is no platform-specific setup step.

## Run

```sh
cargo run --release                       # empty connect form
cargo run --release -- user@host          # prefill username and host
cargo run --release -- user@host:2222     # ... and the port
cargo run --release -- [::1]:2222         # IPv6 literals go in brackets
target/release/sshftp-tui                 # or run the built binary directly
```

The optional argument only **prefills** the form; it never connects by itself.
`Enter` connects, `Esc` quits, and everything else happens inside the TUI.

| Environment variable     | Effect                                                       |
| ------------------------ | ------------------------------------------------------------ |
| `SSHFTP_TUI_KNOWN_HOSTS` | Override the `known_hosts` path (default `~/.ssh/known_hosts`) |

## Logging in

The connect form has two authentication modes. Move between rows with
`Tab`/`Shift+Tab`/`Up`/`Down`, and switch the mode with `Left`/`Right` while the
`Auth` row is selected.

### Password

Type the password into the `Password` row and press `Enter`.

### Private key (public key authentication)

1. Move to the `Auth` row and press `Left`/`Right` until it reads `Key file`.
   Two more rows appear: `Key file` and `Passphrase`.
2. Type the path of the private key. The default is `~/.ssh/id_ed25519`.
3. If the key is encrypted, fill in `Passphrase`; leave it empty otherwise.
4. Press `Enter`.

Path rules:

* `~/...` expands to `$HOME` on Linux/macOS and `%USERPROFILE%` on Windows, so
  the same `~/.ssh/id_ed25519` works on every platform.
* Absolute paths are used as typed, with either separator on Windows
  (`C:\Users\me\.ssh\id_ed25519` and `C:/Users/me/.ssh/id_ed25519` both work).
* A bare `~` is *not* expanded — always write `~/...` or an absolute path.
* The file must already exist, otherwise the form refuses to connect with
  `Key file not found: <path>`.
* There is no command-line flag for the key; it is entered in the form.

Accepted key formats:

| Key format                                                                    | Supported |
| ----------------------------------------------------------------------------- | --------- |
| OpenSSH format (`-----BEGIN OPENSSH PRIVATE KEY-----`), ed25519 / ECDSA / RSA | yes       |
| Legacy PEM (PKCS#8, EC `SEC1`, RSA `PKCS#1`)                                  | yes       |
| PuTTY `.ppk` (including RSA)                                                  | yes       |
| Any of the above, passphrase-encrypted                                        | yes       |

RSA is enabled by the `rsa` feature on `russh` (e.g. `~/.ssh/id_rsa`, an EC2
`.pem`, the key `gcloud compute ssh` writes). It adds only the pure-Rust `rsa`
and `pkcs1` crates, so there is still no C toolchain, and the modern
`rsa-sha2-256/512` signatures are used — the ones today's OpenSSH servers
negotiate. RSA *host* keys work too, so servers that offer no ed25519/ECDSA key
are reachable.

A newer ed25519 key is still a good idea where you get to choose:

```sh
ssh-keygen -t ed25519 -C "me@example.com"       # writes ~/.ssh/id_ed25519{,.pub}
```

The public half of the key must be in the account's `authorized_keys` on the
server. Verify the pair outside the TUI first, so any failure is unambiguous:

```sh
ssh-copy-id -i ~/.ssh/id_ed25519.pub user@host    # or append it by hand
ssh -i ~/.ssh/id_ed25519 -p 2222 user@host
```

On the server `~/.ssh` must be `700` and `authorized_keys` `600`, or `sshd`
ignores them. The client only needs read access to the private key file.

What the app does and does not do:

* **No ssh-agent.** The key must be a real file on disk; `SSH_AUTH_SOCK` is
  never contacted.
* `~/.ssh/config` is read for `Host` aliases — see below.
* The key, password and passphrase are read when you connect and kept in memory
  for the session; they are never written to disk. Your session is not persisted
  either — the only file the app writes, besides the files you transfer, is
  `known_hosts`, and only when you accept a new host key.

If it fails, the form shows one of these:

| Message                                          | Meaning                                                                                                                                  |
| ------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------- |
| `Key file not found: <path>`                     | wrong path, or a bare `~` without a `/`                                                                                                   |
| `loading private key <path>: ...`                | unreadable file, encrypted key without the right passphrase, or a key type the build does not support                                  |
| `authentication failed for user <name> ...`      | the key loaded but the server refused it — normally `authorized_keys` or its permissions on the server                                   |

### `~/.ssh/config`

`Host` aliases are resolved, so `sshftp-tui prod` — or typing `prod` in the
`Host` field and pressing `Tab` — fills the rest of the form:

```
Host prod
    HostName 10.0.0.5
    User deploy
    Port 2222
    IdentityFile ~/.ssh/id_ed25519_prod
```

becomes `10.0.0.5`, `deploy`, `2222` and that key path, with a line under the
fields saying which file and alias supplied them. The `Host` field is replaced
by the resolved `HostName`, so the form still shows exactly what will be
dialled, and `known_hosts` is checked against that hostname, as OpenSSH does.

* **Read once at startup** from `SSHFTP_TUI_SSH_CONFIG` if set, otherwise
  `~/.ssh/config` (`%USERPROFILE%\.ssh\config` on Windows). Edits need a
  restart.
* **Only untouched values are filled**: an empty username, port `22`, and the
  default `~/.ssh/id_ed25519` key path. Anything you typed wins; the `Host`
  field itself is always replaced by `HostName`.
* **Supported**: `Host` with whitespace/comma separated patterns, `*`, `?` and
  `!` negation; `HostName`, `User`, `Port`, `IdentityFile` (the first one
  only); `Include` (a wildcard in the file name works: `Include config.d/*`);
  and the `%% %d %h %n %p %r` tokens. OpenSSH's "first value obtained wins"
  rule applies, so several matching blocks each contribute — a `Host *` block
  can supply a default user or port.
* **`ProxyJump` / `ProxyCommand` are refused with an error**, not ignored: this
  client has no jump-host transport, and dialling the target directly would not
  be the connection the config describes.
* Everything else (`Match`, `CanonicalizeHostname`, forwarding options,
  `UserKnownHostsFile`, agent forwarding, ...) is ignored. The config is never
  written to.

### When the session ends

If the remote shell exits (`exit`, `Ctrl+D`, a `logout`) or the connection drops,
the app returns to the connect form instead of leaving a dead terminal on screen.
The reason is shown on the form — `Remote shell exited with status 0 — sign in
again` — and it starts blank apart from the default port and the key file path,
so you type the details for the new session; nothing from the old one, the
password included, is carried over. Press `F10` instead if you want to quit.

Everything the finished session owned is discarded: the terminal grid and its
scrollback, the SFTP subsystem (press `F3` to open a new one), the remote listing
and any transfer that was running. Reconnecting is a genuinely new SSH
connection — the previous one has already been torn down, so nothing is resumed.

## Key bindings

Global:

| Key      | Action                       |
| -------- | ---------------------------- |
| `Ctrl+T` | Cycle tabs (works everywhere)|
| `F10`    | Quit                         |

Connect form: `Tab`/`Shift+Tab`/`Up`/`Down` move between fields,
`Left`/`Right` switch the auth method, `Enter` connects, `Esc` quits.

Terminal tab: every key is forwarded to the remote shell as the matching xterm
byte sequence (`Ctrl+C`, arrow keys with modifiers, `F1`–`F12`, `Home`/`End`,
`PgUp`/`PgDn`, `Insert`/`Delete`, `Alt+<key>` as `ESC <key>`). Only `Ctrl+T`,
`F10` and the `Shift` combinations below are intercepted locally.

| Key                        | Action                                        |
| -------------------------- | --------------------------------------------- |
| `Shift+PgUp` / `Shift+PgDn`| Scroll the local history by one page          |
| `Shift+Home`               | Jump to the oldest retained line              |
| `Shift+End`                | Return to the live view                       |

The app keeps the last 2000 lines the remote scrolled off the top of the screen
(no history is kept while a remote full-screen program such as `vim` or `less`
is running, since that uses the alternate screen). While you are scrolled back
the pane title turns yellow, shows `SCROLLBACK +<lines>`, and the cursor is
hidden; new output does not move the view, and unmodified `PgUp`/`PgDn` keep
going to the remote so its own pagers still work.

SFTP tab:

| Key           | Action                                       |
| ------------- | -------------------------------------------- |
| `F3`          | Initialize the SFTP subsystem / refresh      |
| `Tab`         | Switch pane focus (yellow border)            |
| `Up`/`Down`   | Move the selection (`PgUp`/`PgDn`, `Home`/`End`) |
| `Enter`       | Enter the selected directory                 |
| `Backspace` / `Left` | Go to the parent directory            |
| `F4`          | Refresh the focused pane                     |
| `F5`          | Copy selection to the other pane (recursive) |
| `F6`          | Rename                                       |
| `F7`          | Create a directory                           |
| `F8`          | Delete (directories are removed recursively) |
| `F9`          | Change directory (type a path; `Ctrl+U` clears) |

Logs tab: `Up`/`Down`/`PgUp`/`PgDn` scroll, `End` resumes following.

## Architecture

Two tasks, connected only by MPSC channels — the UI can never be blocked by
network or disk I/O:

```text
        Cmd                                       Evt
UI task ──────────────────► network task ◄────────────────── UI task
(crossterm events,          (russh session,                 (PtyData,
 ratatui drawing)            SFTP, filesystem)               listings, progress)
                                  │
                                  ├── PTY reader task (ChannelReadHalf::wait loop)
                                  └── one task per SFTP/local filesystem request
```

| Module       | Responsibility                                                    |
| ------------ | ----------------------------------------------------------------- |
| `main.rs`    | Terminal guard + panic hook, channel wiring, event loop           |
| `app.rs`     | All state: tabs, panes, modals, routing, vt100 parser, log buffer  |
| `ui.rs`      | Pure rendering (tabs, terminal grid, dual-pane tables, modals)     |
| `network.rs` | SSH supervisor: connect/auth, host-key hook, PTY, on-demand SFTP   |
| `sftp.rs`    | Listings, mkdir/rename/delete, chunked and recursive transfers     |
| `hostkey.rs` | Strict `known_hosts` policy and fingerprint formatting             |
| `keys.rs`    | crossterm key press → VT100/xterm byte sequences                   |
| `form.rs`    | Connect form state and a hand-rolled single-line text field        |
| `event.rs`   | `Cmd`/`Evt` message types, `FsEntry`, POSIX path helpers           |

Remote paths are always handled as POSIX strings (`remote_join`,
`remote_parent`); `PathBuf` is used only for local paths, so a Windows client
never sends `C:\` style separators to a Linux server.

The terminal is put into raw mode through a guard that restores it on `Drop`,
plus a panic hook that restores it on crash, so a bug cannot leave the user's
shell unusable.

## Host key policy

1. key present and matching → connect
2. key absent → modal with the key type and `SHA256:` fingerprint
   (`y` = append to `known_hosts` and continue, `n`/`Esc`/60 s timeout = abort)
3. key present but *different* → log
   `HOST KEY CHANGED for <host>: <detail>` and abort; the UI offers no override

## Testing

Unit tests (no network, no container):

```sh
cargo test
```

They cover key-sequence encoding, POSIX path helpers, size formatting, the
connect form and text field (including bracketed IPv6 targets), the strict
`known_hosts` policy (unknown → match → changed) using two unrelated ed25519
keys, RSA private-key loading (the EC2 `.pem` shape, which is also the guard for
`russh`'s `rsa` feature), and the whole rendering layer drawn onto a
`ratatui` `TestBackend` (connect form, SFTP placeholder, SFTP listings, PTY
output, logs, modals, footer hints).

End-to-end tests drive the same `Cmd`/`Evt` interface the TUI uses against a
real SSH/SFTP server. Two profiles are supported.

**Public read-only server** — no container needed, this is the quickest way to
check a build against a foreign implementation (Rebex's demo service, which is
`RebexSSH`, not OpenSSH):

```powershell
$env:SSHFTP_TUI_TEST_HOST     = 'test.rebex.net'
$env:SSHFTP_TUI_TEST_PORT     = '22'
$env:SSHFTP_TUI_TEST_USER     = 'demo'
$env:SSHFTP_TUI_TEST_PASSWORD = 'password'
$env:SSHFTP_TUI_TEST_READONLY = '1'
cargo test --test sshd_e2e -- --ignored --nocapture
```

**Local OpenSSH container** — adds everything that writes to the server:

```powershell
pwsh -File scripts/test-server.ps1 build   # once
pwsh -File scripts/test-server.ps1 test    # start the server and run the suite
pwsh -File scripts/test-server.ps1 stop
```

`test` works out which address the client must use — on Linux and macOS podman
publishes the port on `localhost`, while on Windows it runs inside a WSL machine
and the port is only reachable through that machine's address — then exports
`SSHFTP_TUI_TEST_HOST` / `SSHFTP_TUI_TEST_PORT` and runs the suite. To drive it
by hand:

```powershell
pwsh -File scripts/test-server.ps1 start
$env:SSHFTP_TUI_TEST_HOST = '<address printed by start>'
cargo test --test sshd_e2e -- --ignored --nocapture
```

| Variable                   | Default            | Meaning                     |
| -------------------------- | ------------------ | --------------------------- |
| `SSHFTP_TUI_TEST_HOST`     | `127.0.0.1`        | Server address              |
| `SSHFTP_TUI_TEST_PORT`     | `2222`             | Server port                 |
| `SSHFTP_TUI_TEST_USER`     | `tester`           | Login name                  |
| `SSHFTP_TUI_TEST_PASSWORD` | `testpass`         | Password                    |
| `SSHFTP_TUI_TEST_READONLY` | unset              | `1` skips all writes        |

The suite asserts, against a real server:

* an unknown host key prompts exactly once and is written to `known_hosts` in
  the exact form the client looks it up again (bare hostname on port 22,
  `[host]:port` otherwise) — the recorded key is checked against the server's
  real key;
* a *changed* key is reported and the connection is refused, with no way to
  override it from the UI;
* the SFTP subsystem is **not** opened by connecting — only when `F3` is
  requested;
* a real file downloads with the size the listing reported;
* the PTY opens, forwards input and, on a normal shell, reports
  `TERM=xterm-256color` and the requested window size;
* (container profile) uploads, single-file and recursive round trips are
  byte-for-byte identical, and mkdir/rename/recursive-delete show up in the
  refreshed listing.

The complete suite is exercised on Windows (`x86_64-pc-windows-msvc`), and the
read-only profile plus all unit tests on Linux (`x86_64-unknown-linux-gnu`), so
PTY handling, host-key handling and path handling are covered on both platforms.

`scripts/test-server.ps1` also offers `reset-hosts` (fresh host keys),
`fingerprint` and `logs`. `podman logs` is the quickest way to see that the
server records `subsystem 'sftp'` on the *same* TCP connection, after the shell
session was started — i.e. on demand.

## Known limitations

* One connection at a time; no session tabs.
* Symlinked directories are followed during recursive copies, with a depth cap
  of 64 to stop cycles; symlinks themselves are not recreated as links.
* Existing files are overwritten on copy (the completion message reports how
  many were overwritten).
* Ctrl+Shift+V style paste is passed through to the remote as raw input.

## License

MIT
