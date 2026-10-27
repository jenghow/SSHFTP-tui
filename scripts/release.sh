#!/usr/bin/env bash
#
# Build the release binary and package it, with the docs, into target/dist.
#
# Bash counterpart of scripts/release.ps1 for Linux and macOS. Runs the quality
# gate (fmt, clippy, tests), builds `cargo build --release`, stages the binary
# next to README.md and LICENSE, and writes
# `sshftp-tui-<version>-<triple>.tar.gz` (a `.zip` for a Windows target)
# containing a single `sshftp-tui-<version>-<triple>/` folder.
#
# The version comes from the [package] section of Cargo.toml and the triple
# from `rustc -vV`, so the archive name always matches the build.

set -euo pipefail

NAME=sshftp-tui
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)

VERSION=
TARGET=
OUTPUT_DIR=
SKIP_BUILD=0
SKIP_CHECKS=0

usage() {
    cat <<EOF
Usage: $(basename "$0") [VERSION] [options]

Build $NAME, package it with README.md and LICENSE, and write the archive to
target/dist.

Arguments:
  VERSION              Override the version from Cargo.toml

Options:
  --target TRIPLE      Build for TRIPLE (default: the host triple)
  --output-dir DIR     Directory for the archive (default: target/dist)
  --skip-build         Re-package the binary already in target[/TRIPLE]/release
  --skip-checks        Skip cargo fmt, clippy and test
  -h, --help           Show this help
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

while [ "$#" -gt 0 ]; do
    case "$1" in
        -h | --help)
            usage
            exit 0
            ;;
        --target)
            [ "$#" -ge 2 ] || die "--target needs a triple"
            TARGET=$2
            shift 2
            ;;
        --target=*)
            TARGET=${1#*=}
            shift
            ;;
        --output-dir)
            [ "$#" -ge 2 ] || die "--output-dir needs a path"
            OUTPUT_DIR=$2
            shift 2
            ;;
        --output-dir=*)
            OUTPUT_DIR=${1#*=}
            shift
            ;;
        --skip-build)
            SKIP_BUILD=1
            shift
            ;;
        --skip-checks)
            SKIP_CHECKS=1
            shift
            ;;
        -*)
            die "unknown option: $1 (try --help)"
            ;;
        *)
            [ -z "$VERSION" ] || die "unexpected argument: $1"
            VERSION=$1
            shift
            ;;
    esac
done

# Read `version` from the [package] section only: the dependency tables are full
# of `version = "..."` lines that must not win.
crate_version() {
    awk '
        /^[[:space:]]*\[/ {
            in_package = ($0 ~ /^[[:space:]]*\[package\][[:space:]]*$/)
            next
        }
        in_package && /^[[:space:]]*version[[:space:]]*=/ {
            if (match($0, /"[^"]*"/)) {
                print substr($0, RSTART + 1, RLENGTH - 2)
                exit
            }
        }
    ' "$ROOT/Cargo.toml"
}

# `rustc -vV` reports the exact host triple, which is what cargo uses for an
# untargeted build. Fall back to uname if rustc is unavailable.
host_triple() {
    local triple arch os
    triple=$(rustc -vV 2>/dev/null | awk '/^host:/ { print $2; exit }') || true
    if [ -n "$triple" ]; then
        printf '%s\n' "$triple"
        return
    fi
    arch=$(uname -m 2>/dev/null) || arch=unknown
    case "$arch" in
        amd64) arch=x86_64 ;;
        arm64) arch=aarch64 ;;
    esac
    os=$(uname -s 2>/dev/null) || os=unknown
    case "$os" in
        Linux) os=unknown-linux-gnu ;;
        Darwin) os=apple-darwin ;;
        *) os=unknown ;;
    esac
    printf '%s-%s\n' "$arch" "$os"
}

human_size() {
    awk -v bytes="$1" 'BEGIN {
        split("B KiB MiB GiB", units, " ")
        value = bytes
        i = 1
        while (value >= 1024 && i < 4) {
            value /= 1024
            i++
        }
        if (i == 1) {
            printf "%d B\n", bytes
        } else {
            printf "%.1f %s\n", value, units[i]
        }
    }'
}

cd -- "$ROOT"

[ -n "$VERSION" ] || VERSION=$(crate_version)
[ -n "$VERSION" ] || die "could not find the [package] version in Cargo.toml"

if [ -n "$TARGET" ]; then
    TRIPLE=$TARGET
else
    TRIPLE=$(host_triple)
fi
[ -n "$TRIPLE" ] || die "could not determine the host triple (is rustc installed?)"

[ -n "$OUTPUT_DIR" ] || OUTPUT_DIR="$ROOT/target/dist"
# `zip` runs from the staging directory, so a relative output path would land
# there instead of under the repository.
case "$OUTPUT_DIR" in
    /*) ;;
    *) OUTPUT_DIR="$ROOT/$OUTPUT_DIR" ;;
esac

# Honor cargo's own override so a build tree outside the repository still works.
TARGET_ROOT=${CARGO_TARGET_DIR:-$ROOT/target}
if [ -n "$TARGET" ]; then
    RELEASE_DIR="$TARGET_ROOT/$TARGET/release"
else
    RELEASE_DIR="$TARGET_ROOT/release"
fi

case "$TRIPLE" in
    *windows*) EXT=.exe ;;
    *) EXT= ;;
esac
BINARY="$RELEASE_DIR/$NAME$EXT"

PACKAGE="$NAME-$VERSION-$TRIPLE"
case "$TRIPLE" in
    *windows*) ARCHIVE="$OUTPUT_DIR/$PACKAGE.zip" ;;
    *) ARCHIVE="$OUTPUT_DIR/$PACKAGE.tar.gz" ;;
esac

printf '%s %s  (%s)\n' "$NAME" "$VERSION" "$TRIPLE"

if [ "$SKIP_CHECKS" -eq 0 ] || [ "$SKIP_BUILD" -eq 0 ]; then
    require_toolchain
fi

if [ "$SKIP_CHECKS" -eq 0 ]; then
    command -v cargo >/dev/null 2>&1 ||
        die "cargo not found in PATH; install Rust from https://rustup.rs"
    echo '==> cargo fmt --check'
    cargo fmt --check
    echo '==> cargo clippy --all-targets -- -D warnings'
    cargo clippy --all-targets -- -D warnings
    echo '==> cargo test'
    cargo test
fi

if [ "$SKIP_BUILD" -eq 0 ]; then
    command -v cargo >/dev/null 2>&1 ||
        die "cargo not found in PATH; install Rust from https://rustup.rs"
    build_args=(build --release)
    if [ -n "$TARGET" ]; then
        build_args+=(--target "$TARGET")
    fi
    echo "==> cargo ${build_args[*]}"
    cargo "${build_args[@]}"
fi

[ -f "$BINARY" ] ||
    die "release binary not found at $BINARY. Build it first (drop --skip-build)."

TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/$NAME-pack.XXXXXX")
trap 'rm -rf "$TMP_ROOT"' EXIT

PACKAGE_DIR="$TMP_ROOT/$PACKAGE"
mkdir -p "$PACKAGE_DIR"
cp -p "$BINARY" "$PACKAGE_DIR/"

for file in README.md LICENSE; do
    if [ -f "$ROOT/$file" ]; then
        cp -p "$ROOT/$file" "$PACKAGE_DIR/"
    else
        echo "warning: skipping $file (not found)" >&2
    fi
done

mkdir -p "$OUTPUT_DIR"
rm -f "$ARCHIVE"

case "$TRIPLE" in
    *windows*)
        command -v zip >/dev/null 2>&1 ||
            die "zip is required to package a Windows target (install zip, or use scripts/release.ps1)"
        (cd "$TMP_ROOT" && zip -q -r "$ARCHIVE" "$PACKAGE")
        ;;
    *)
        tar -czf "$ARCHIVE" -C "$TMP_ROOT" "$PACKAGE"
        ;;
esac

echo 'Packaged:'
for file in "$PACKAGE_DIR"/*; do
    [ -e "$file" ] || continue
    bytes=$(wc -c <"$file" | tr -d '[:space:]')
    printf '  %-52s %s\n' "$PACKAGE/$(basename "$file")" "$(human_size "$bytes")"
done

archive_bytes=$(wc -c <"$ARCHIVE" | tr -d '[:space:]')
printf '\nWrote %s (%s)\n' "$ARCHIVE" "$(human_size "$archive_bytes")"
