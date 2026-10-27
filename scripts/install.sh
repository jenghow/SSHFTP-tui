#!/usr/bin/env bash
#
# Install sshftp-tui system-wide on Linux.
#
# Copies the release binary (plus README.md, LICENSE and app-icon.jpg) into
# /opt/sshftp-tui, links /usr/local/bin/sshftp-tui, and writes a desktop entry
# to /usr/local/share/applications so the app appears in the GNOME (and other
# freedesktop) application menu. The entry launches the TUI inside whichever
# terminal emulator is installed, because this is a terminal application, and
# uses the installed app-icon.jpg for the grid/dash/dock icon.
#
# Run it as the owner of this checkout: the script builds as your user and only
# uses sudo for the paths outside the repository. `--uninstall` removes what it
# installed.

set -euo pipefail

NAME=sshftp-tui
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)

PREFIX=/opt/$NAME
BIN_DIR=/usr/local/bin
APPLICATIONS_DIR=/usr/local/share/applications
BINARY=
BUILD=1
UNINSTALL=0
TMP_DESKTOP=

usage() {
    cat <<EOF
Usage: $(basename "$0") [options]

Build $NAME, install it into $PREFIX, link it into \$PATH and add it to the
application menu.

Options:
  --prefix DIR            Install directory (default: /opt/$NAME)
  --bin-dir DIR           Directory for the command symlink (default: /usr/local/bin)
  --applications-dir DIR  Directory for the .desktop file (default: /usr/local/share/applications)
  --binary PATH           Install PATH instead of building
  --no-build              Use target/release/$NAME instead of building
  --uninstall             Remove the binary, symlink and menu entry
  -h, --help              Show this help

The menu entry opens the TUI in gnome-terminal, GNOME Console, Ptyxis, Konsole,
Xfce Terminal, or any other terminal that is found; if none is found, the
entry falls back to Terminal=true and lets the desktop pick one.
EOF
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

# rustc links through `cc`, and ring's build script needs `ar`. Without them
# cargo fails deep inside unrelated build scripts with a confusing error, so
# check once up front.
require_toolchain() {
    local missing=0
    if ! command -v cc >/dev/null 2>&1; then
        printf 'error: no C compiler/linker found: `cc` is missing.\n' >&2
        printf '  Debian/Ubuntu: sudo apt install build-essential\n' >&2
        printf '  Fedora:        sudo dnf install gcc binutils\n' >&2
        printf '  Arch:          sudo pacman -S base-devel\n' >&2
        printf '  macOS:         xcode-select --install\n' >&2
        missing=1
    fi
    if ! command -v ar >/dev/null 2>&1; then
        printf 'error: `ar` is missing; install binutils (Linux) or the Xcode command line tools (macOS).\n' >&2
        missing=1
    fi
    [ "$missing" -eq 0 ] || exit 1
}

cleanup() {
    [ -z "$TMP_DESKTOP" ] || rm -f "$TMP_DESKTOP"
}
trap cleanup EXIT

while [ "$#" -gt 0 ]; do
    case "$1" in
        -h | --help)
            usage
            exit 0
            ;;
        --prefix)
            [ "$#" -ge 2 ] || die "--prefix needs a directory"
            PREFIX=$2
            shift 2
            ;;
        --prefix=*)
            PREFIX=${1#*=}
            shift
            ;;
        --bin-dir)
            [ "$#" -ge 2 ] || die "--bin-dir needs a directory"
            BIN_DIR=$2
            shift 2
            ;;
        --bin-dir=*)
            BIN_DIR=${1#*=}
            shift
            ;;
        --applications-dir)
            [ "$#" -ge 2 ] || die "--applications-dir needs a directory"
            APPLICATIONS_DIR=$2
            shift 2
            ;;
        --applications-dir=*)
            APPLICATIONS_DIR=${1#*=}
            shift
            ;;
        --binary)
            [ "$#" -ge 2 ] || die "--binary needs a path"
            BINARY=$2
            BUILD=0
            shift 2
            ;;
        --binary=*)
            BINARY=${1#*=}
            BUILD=0
            shift
            ;;
        --no-build)
            BUILD=0
            shift
            ;;
        --uninstall)
            UNINSTALL=1
            shift
            ;;
        -*)
            die "unknown option: $1 (try --help)"
            ;;
        *)
            die "unexpected argument: $1"
            ;;
    esac
done

[ -n "$PREFIX" ] && [ "$PREFIX" != "/" ] ||
    die "refusing to use $PREFIX as the install prefix"

# Relative overrides are relative to the repository, like the other scripts.
abspath() {
    case "$1" in
        /*) printf '%s\n' "$1" ;;
        *) printf '%s\n' "$ROOT/$1" ;;
    esac
}
PREFIX=$(abspath "$PREFIX")
BIN_DIR=$(abspath "$BIN_DIR")
APPLICATIONS_DIR=$(abspath "$APPLICATIONS_DIR")

INSTALLED_BINARY=$PREFIX/$NAME
BIN_LINK=$BIN_DIR/$NAME
DESKTOP_FILE=$APPLICATIONS_DIR/$NAME.desktop
APP_ICON_SOURCE=$ROOT/app-icon.jpg
INSTALLED_ICON=$PREFIX/app-icon.jpg
if [ -f "$APP_ICON_SOURCE" ]; then
    ICON_VALUE=$INSTALLED_ICON
else
    ICON_VALUE=utilities-terminal
fi

SUDO=
if [ "$EUID" -ne 0 ]; then
    command -v sudo >/dev/null 2>&1 ||
        die "sudo is not installed; run this script as root"
    SUDO=sudo
fi

as_root() {
    if [ -n "$SUDO" ]; then
        "$SUDO" "$@"
    else
        "$@"
    fi
}

# Quote one argument for the Exec key of a desktop entry: backslash, double
# quote and percent have special meaning there.
desktop_quote() {
    local value=$1
    value=${value//\\/\\\\}
    value=${value//\"/\\\"}
    value=${value//%/%%}
    printf '"%s"' "$value"
}

# Populate TERMINAL_CMD with the right "run this in a terminal" invocation.
# The emulators disagree on how the command is separated, so each gets its own
# form. An empty array means no emulator was found.
TERMINAL_CMD=()
detect_terminal() {
    if command -v gnome-terminal >/dev/null 2>&1; then
        # The class lets GNOME associate the terminal window with this entry,
        # so the dash/dock keeps showing app-icon.jpg while the app runs.
        TERMINAL_CMD=(gnome-terminal --class=$NAME -- "$INSTALLED_BINARY")
    elif command -v ptyxis >/dev/null 2>&1; then
        TERMINAL_CMD=(ptyxis -- "$INSTALLED_BINARY")
    elif command -v kgx >/dev/null 2>&1; then
        TERMINAL_CMD=(kgx -- "$INSTALLED_BINARY")
    elif command -v x-terminal-emulator >/dev/null 2>&1; then
        TERMINAL_CMD=(x-terminal-emulator -e "$INSTALLED_BINARY")
    elif command -v konsole >/dev/null 2>&1; then
        TERMINAL_CMD=(konsole -e "$INSTALLED_BINARY")
    elif command -v xfce4-terminal >/dev/null 2>&1; then
        TERMINAL_CMD=(xfce4-terminal -x "$INSTALLED_BINARY")
    elif command -v mate-terminal >/dev/null 2>&1; then
        TERMINAL_CMD=(mate-terminal -- "$INSTALLED_BINARY")
    elif command -v tilix >/dev/null 2>&1; then
        TERMINAL_CMD=(tilix -e "$INSTALLED_BINARY")
    elif command -v alacritty >/dev/null 2>&1; then
        TERMINAL_CMD=(alacritty -e "$INSTALLED_BINARY")
    elif command -v kitty >/dev/null 2>&1; then
        TERMINAL_CMD=(kitty "$INSTALLED_BINARY")
    elif command -v wezterm >/dev/null 2>&1; then
        TERMINAL_CMD=(wezterm start -- "$INSTALLED_BINARY")
    elif command -v foot >/dev/null 2>&1; then
        TERMINAL_CMD=(foot "$INSTALLED_BINARY")
    elif command -v urxvt >/dev/null 2>&1; then
        TERMINAL_CMD=(urxvt -e "$INSTALLED_BINARY")
    elif command -v xterm >/dev/null 2>&1; then
        TERMINAL_CMD=(xterm -e "$INSTALLED_BINARY")
    else
        TERMINAL_CMD=()
    fi
}

write_desktop_file() {
    detect_terminal

    local exec_line= terminal_value=false arg
    if [ "${#TERMINAL_CMD[@]}" -gt 0 ]; then
        for arg in "${TERMINAL_CMD[@]}"; do
            exec_line+="$(desktop_quote "$arg") "
        done
        exec_line=${exec_line% }
    else
        # No emulator found: let the desktop environment supply one.
        exec_line=$(desktop_quote "$INSTALLED_BINARY")
        terminal_value=true
    fi

    # desktop-file-validate insists on the .desktop extension.
    TMP_DESKTOP=$(mktemp "${TMPDIR:-/tmp}/$NAME-desktop.XXXXXX.desktop")
    cat >"$TMP_DESKTOP" <<EOF
[Desktop Entry]
Type=Application
Version=1.0
Name=SSHFTP TUI
GenericName=SSH + SFTP Client
Comment=Terminal SSH and SFTP client
Exec=$exec_line
TryExec=$INSTALLED_BINARY
Icon=$ICON_VALUE
Terminal=$terminal_value
StartupWMClass=$NAME
Categories=Network;FileTransfer;RemoteAccess;
Keywords=ssh;sftp;scp;terminal;remote;
StartupNotify=false
EOF

    if command -v desktop-file-validate >/dev/null 2>&1; then
        desktop-file-validate "$TMP_DESKTOP"
    fi
}

refresh_menu() {
    if command -v update-desktop-database >/dev/null 2>&1; then
        as_root update-desktop-database "$APPLICATIONS_DIR" >/dev/null 2>&1 || true
    fi
}

uninstall() {
    echo "==> removing $NAME"
    as_root rm -f "$DESKTOP_FILE" "$BIN_LINK" "$INSTALLED_BINARY"
    as_root rm -f "$PREFIX/README.md" "$PREFIX/LICENSE" "$INSTALLED_ICON"
    # Only succeeds when nothing else lives there.
    as_root rmdir "$PREFIX" 2>/dev/null || true
    refresh_menu
    echo "Removed the binary, $BIN_LINK and $DESKTOP_FILE."
}

if [ "$UNINSTALL" -eq 1 ]; then
    uninstall
    exit 0
fi

if [ "$BUILD" -eq 1 ]; then
    command -v cargo >/dev/null 2>&1 ||
        die "cargo not found in PATH; install Rust from https://rustup.rs"
    require_toolchain
    if [ "$EUID" -eq 0 ] && [ -n "${SUDO_USER:-}" ]; then
        echo "warning: running under sudo, so cargo's target/ will be owned by root" >&2
    fi
    echo '==> cargo build --release'
    cargo build --release
fi

# Honor cargo's own override, like scripts/release.sh.
[ -n "$BINARY" ] || BINARY=${CARGO_TARGET_DIR:-$ROOT/target}/release/$NAME
[ -f "$BINARY" ] ||
    die "release binary not found at $BINARY (drop --no-build or pass --binary)"
[ -x "$BINARY" ] || die "$BINARY is not executable"
BINARY=$(cd -- "$(dirname -- "$BINARY")" && pwd)/$(basename -- "$BINARY")

# Build and validate the menu entry before touching any system path.
write_desktop_file

echo "==> installing to $PREFIX"
as_root install -d -m 0755 "$PREFIX"
if [ "$BINARY" != "$INSTALLED_BINARY" ]; then
    as_root install -m 0755 "$BINARY" "$INSTALLED_BINARY"
fi
for doc in README.md LICENSE; do
    if [ -f "$ROOT/$doc" ]; then
        as_root install -m 0644 "$ROOT/$doc" "$PREFIX/$doc"
    else
        echo "warning: skipping $doc (not found)" >&2
    fi
done

if [ -f "$APP_ICON_SOURCE" ]; then
    as_root install -m 0644 "$APP_ICON_SOURCE" "$INSTALLED_ICON"
else
    echo "warning: app-icon.jpg not found; the menu entry keeps the theme icon" >&2
fi

as_root install -d -m 0755 "$BIN_DIR"
as_root ln -sfn "$INSTALLED_BINARY" "$BIN_LINK"

as_root install -d -m 0755 "$APPLICATIONS_DIR"
as_root install -m 0644 "$TMP_DESKTOP" "$DESKTOP_FILE"
refresh_menu

cat <<EOF

Installed:
  binary  $INSTALLED_BINARY
  command $BIN_LINK
  menu    $DESKTOP_FILE

Run "sshftp-tui" in a terminal, or open "SSHFTP TUI" from the application menu.
EOF
