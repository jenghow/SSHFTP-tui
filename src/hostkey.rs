//! Strict host-key verification against `known_hosts`.
//!
//! Policy:
//! * key present and matching  -> connect
//! * key absent                -> ask the user, append on acceptance (TOFU)
//! * key present but different -> refuse, no override from the UI
//!
//! `SSHFTP_TUI_KNOWN_HOSTS` can point at an alternate file; this exists so the
//! integration tests never touch the real `~/.ssh/known_hosts`.

use std::path::{Path, PathBuf};

use russh::keys::known_hosts::{check_known_hosts_path, learn_known_hosts_path};
use russh::keys::{HashAlg, PublicKey};

#[derive(Debug, PartialEq, Eq)]
pub enum HostKeyStatus {
    Match,
    Unknown,
    Error(String),
}

pub fn known_hosts_path() -> PathBuf {
    if let Some(path) = std::env::var_os("SSHFTP_TUI_KNOWN_HOSTS") {
        return PathBuf::from(path);
    }
    home_dir()
        .map(|home| home.join(".ssh").join("known_hosts"))
        .unwrap_or_else(|| PathBuf::from("known_hosts"))
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

pub fn check(host: &str, port: u16, key: &PublicKey) -> HostKeyStatus {
    check_at(&known_hosts_path(), host, port, key)
}

pub fn learn(host: &str, port: u16, key: &PublicKey) -> anyhow::Result<()> {
    learn_at(&known_hosts_path(), host, port, key)
}

pub fn check_at(path: &Path, host: &str, port: u16, key: &PublicKey) -> HostKeyStatus {
    match check_known_hosts_path(host, port, key, path) {
        Ok(true) => HostKeyStatus::Match,
        Ok(false) => HostKeyStatus::Unknown,
        Err(err) => HostKeyStatus::Error(format!("{err}")),
    }
}

pub fn learn_at(path: &Path, host: &str, port: u16, key: &PublicKey) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    learn_known_hosts_path(host, port, key, path)?;
    Ok(())
}

pub fn fingerprint(key: &PublicKey) -> String {
    format!("{}", key.fingerprint(HashAlg::Sha256))
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    // Two unrelated ed25519 keys, used to exercise the "key changed" path.
    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHRv+JWCwyhJX+91foln7rMosF8iQcP6jCkusldz6lLO";
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIObNYhxzK5ayt0Cjj3i4hL9NgFq2CcWTIsLHwzLXB/uA";

    fn parse(key: &str) -> PublicKey {
        key.parse::<PublicKey>().expect("valid ed25519 key")
    }

    fn scratch_file(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("sshftp-tui-{tag}-{nanos}"))
    }

    #[test]
    fn unknown_host_is_prompted_once_then_remembered() {
        let path = scratch_file("hostkey");
        let key_a = parse(KEY_A);
        let key_b = parse(KEY_B);

        assert_eq!(
            check_at(&path, "example.com", 22, &key_a),
            HostKeyStatus::Unknown
        );

        learn_at(&path, "example.com", 22, &key_a).expect("learning the key succeeds");
        assert_eq!(
            check_at(&path, "example.com", 22, &key_a),
            HostKeyStatus::Match
        );

        // Same host, different key: hard failure, never a prompt.
        assert!(matches!(
            check_at(&path, "example.com", 22, &key_b),
            HostKeyStatus::Error(_)
        ));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ports_are_tracked_separately_and_fingerprints_are_sha256() {
        let path = scratch_file("hostkey-port");
        let key_a = parse(KEY_A);

        learn_at(&path, "example.com", 2222, &key_a).expect("learning the key succeeds");
        assert_eq!(
            check_at(&path, "example.com", 2222, &key_a),
            HostKeyStatus::Match
        );
        // The default port has no entry yet.
        assert_eq!(
            check_at(&path, "example.com", 22, &key_a),
            HostKeyStatus::Unknown
        );

        assert!(fingerprint(&key_a).starts_with("SHA256:"));

        let _ = std::fs::remove_file(&path);
    }
}
