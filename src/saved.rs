//! Connections the user explicitly saved, in an app-owned OpenSSH-syntax file.
//!
//! The UI task never touches this file:
//! * `main::run` loads it once, before the first frame, so a lookup is pure;
//! * `network::run` appends to it on [`crate::event::Cmd::SaveConnection`].
//!
//! [`crate::event::SavedConnection`] has no password or passphrase field, so
//! the writer can only ever store the host, port, user and key path.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::event::SavedConnection;
use crate::hostkey;
use crate::sshconfig::SshConfig;

/// Points somewhere other than the default file. Tests use [`append_at`]
/// with a scratch path instead of mutating this process-global variable.
pub const CONNECTIONS_ENV: &str = "SSHFTP_TUI_CONNECTIONS";

const APP_DIR: &str = "sshftp-tui";
const FILE_NAME: &str = "connections";
const DEFAULT_LABEL: &str = "~/.config/sshftp-tui/connections";
const FALLBACK_FILE: &str = "sshftp-tui-connections";

const HEADER: &str = "\
# Saved connections for sshftp-tui. No passwords or passphrases are stored.
# OpenSSH configuration syntax; Include this file from ~/.ssh/config if you
# want ssh to see the same aliases.

";

/// `SSHFTP_TUI_CONNECTIONS`, else the platform config directory. The file does
/// not have to exist yet.
pub fn path() -> PathBuf {
    let override_value = std::env::var_os(CONNECTIONS_ENV);
    resolve_path(override_value.as_deref(), base_dir())
}

/// The pure half of [`path`], so the rules can be tested without touching the
/// environment.
fn resolve_path(override_value: Option<&OsStr>, base: Option<PathBuf>) -> PathBuf {
    if let Some(value) = override_value {
        return PathBuf::from(value);
    }
    match base {
        Some(dir) => dir.join(APP_DIR).join(FILE_NAME),
        None => PathBuf::from(FALLBACK_FILE),
    }
}

fn base_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| hostkey::home_dir().map(|home| home.join(".config")))
    }
    #[cfg(target_os = "macos")]
    {
        hostkey::home_dir().map(|home| home.join("Library").join("Application Support"))
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| hostkey::home_dir().map(|home| home.join(".config")))
    }
}

/// The friendly name shown on the connect screen. It is `~/.config/...` only
/// for the real default, never for an override that happens to resolve there.
pub fn label(path: &Path) -> String {
    let default = base_dir().map(|dir| dir.join(APP_DIR).join(FILE_NAME));
    if std::env::var_os(CONNECTIONS_ENV).is_none() && default.as_deref() == Some(path) {
        DEFAULT_LABEL.to_string()
    } else {
        path.display().to_string()
    }
}

/// Loads the saved-connections file for `main::run`. A missing file is simply
/// an empty config, exactly like `~/.ssh/config`.
pub fn load() -> (SshConfig, PathBuf) {
    let path = path();
    let mut config = SshConfig::load_at(&path);
    config.set_label(label(&path));
    (config, path)
}

/// Appends a parsed block to the in-memory config. Used after a successful
/// save so the alias resolves without re-reading the file.
pub fn add_to_config(config: &mut SshConfig, entry: &SavedConnection) -> Result<()> {
    config.add_text(&render(entry)?);
    Ok(())
}

/// Writes the block at the end of `path`, creating the file (0600) and its
/// parent directory (0700) when needed. The file is append-only: existing
/// entries, comments and permissions are never rewritten.
pub fn append_at(path: &Path, entry: &SavedConnection) -> Result<()> {
    if !valid_alias(&entry.alias) {
        bail!("invalid Host alias {:?}", entry.alias);
    }

    let existing = match fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };

    if existing.is_some() && SshConfig::load_at(path).defines_alias(&entry.alias) {
        bail!(
            "Host {} is already defined in {}",
            entry.alias,
            path.display()
        );
    }

    let block = render(entry)?;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            create_private_dir(parent)?;
        }
    }

    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("opening {} for append", path.display()))?;

    let mut text = String::new();
    match existing.as_deref() {
        None | Some("") => text.push_str(HEADER),
        Some(existing) => {
            if !existing.ends_with('\n') {
                text.push('\n');
            }
            if !existing.ends_with("\n\n") {
                text.push('\n');
            }
        }
    }
    text.push_str(&block);

    file.write_all(text.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Renders one `Host` block. The alias is validated by the caller; values that
/// would confuse the config parser are rejected or quoted.
pub fn render(entry: &SavedConnection) -> Result<String> {
    if !valid_alias(&entry.alias) {
        bail!("invalid Host alias {:?}", entry.alias);
    }
    let mut out = String::new();
    out.push_str(&format!("Host {}\n", entry.alias));
    if entry.host != entry.alias {
        out.push_str(&format!("    HostName {}\n", value(&entry.host)?));
    }
    out.push_str(&format!("    User {}\n", value(&entry.user)?));
    if entry.port != 22 {
        out.push_str(&format!("    Port {}\n", entry.port));
    }
    if let Some(key) = &entry.identity_file {
        out.push_str(&format!("    IdentityFile {}\n", value(key)?));
    }
    Ok(out)
}

fn value(raw: &str) -> Result<String> {
    if raw.contains(['\n', '\r', '"']) {
        bail!("cannot save a value containing a newline or double quote: {raw:?}");
    }
    if raw.is_empty() || raw.chars().any(|c| c.is_whitespace() || c == '#') {
        Ok(format!("\"{raw}\""))
    } else {
        Ok(raw.to_string())
    }
}

/// A `Host` alias that is safe on the command line and unambiguous in the
/// config: no whitespace, commas, wildcards, negation, `@` or `:`.
pub fn valid_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias != "."
        && alias != ".."
        && !alias.starts_with('-')
        && alias
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The name proposed in the save dialog. A normal hostname doubles as the
/// alias, so typing the hostname later recalls the entry. An IPv6 literal or
/// another value that cannot be an alias gets a sanitized `user-host` name.
pub fn default_alias(user: &str, host: &str) -> String {
    if valid_alias(host) {
        return host.to_string();
    }
    let candidate = sanitize(&format!("{user}-{host}"));
    if valid_alias(&candidate) {
        candidate
    } else {
        "connection".to_string()
    }
}

fn sanitize(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_dash = false;
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
            last_dash = c == '-';
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn create_private_dir(path: &Path) -> Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .with_context(|| format!("creating {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn entry() -> SavedConnection {
        SavedConnection {
            alias: "prod".to_string(),
            host: "10.0.0.5".to_string(),
            port: 2222,
            user: "deploy".to_string(),
            identity_file: Some("~/.ssh/prod_ed25519".to_string()),
        }
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("sshftp-tui-saved-{tag}-{nanos}"));
        fs::create_dir_all(&dir).expect("scratch directory");
        dir
    }

    #[test]
    fn the_path_override_wins_and_the_default_is_per_platform() {
        assert_eq!(
            resolve_path(
                Some(OsStr::new("/tmp/custom-connections")),
                Some(PathBuf::from("/home/me/.config"))
            ),
            PathBuf::from("/tmp/custom-connections")
        );
        assert_eq!(
            resolve_path(None, Some(PathBuf::from("/home/me/.config"))),
            PathBuf::from("/home/me/.config/sshftp-tui/connections")
        );
        assert_eq!(
            resolve_path(None, None),
            PathBuf::from("sshftp-tui-connections")
        );
    }

    #[test]
    fn a_rendered_block_round_trips_through_the_ssh_config_parser() {
        let dir = scratch_dir("roundtrip");
        let path = dir.join("connections");

        append_at(&path, &entry()).expect("append succeeds");

        let config = SshConfig::load_at(&path);
        let resolved = config.lookup("prod").expect("the saved alias resolves");
        assert_eq!(resolved.alias, "prod");
        assert_eq!(resolved.hostname, "10.0.0.5");
        assert_eq!(resolved.user.as_deref(), Some("deploy"));
        assert_eq!(resolved.port, Some(2222));
        assert_eq!(
            resolved.identity_file.as_deref(),
            Some("~/.ssh/prod_ed25519")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_default_port_and_a_matching_hostname_are_left_out() {
        let mut simple = entry();
        simple.host = simple.alias.clone();
        simple.port = 22;

        let text = render(&simple).expect("valid block");
        assert!(!text.contains("HostName"), "{text}");
        assert!(!text.contains("Port"), "{text}");
        assert!(text.contains("IdentityFile ~/.ssh/prod_ed25519"), "{text}");
    }

    #[test]
    fn a_value_with_spaces_is_quoted_and_reads_back_exactly() {
        let dir = scratch_dir("quoted");
        let path = dir.join("connections");
        let mut spaced = entry();
        spaced.identity_file = Some("C:\\Users\\Me\\My Keys\\id_ed25519".to_string());

        let text = render(&spaced).expect("quotable value");
        assert!(text.contains("\"C:\\Users\\Me\\My Keys\\id_ed25519\""));

        append_at(&path, &spaced).expect("append succeeds");
        let config = SshConfig::load_at(&path);
        let resolved = config.lookup("prod").expect("the saved alias resolves");
        assert_eq!(
            resolved.identity_file.as_deref(),
            Some("C:\\Users\\Me\\My Keys\\id_ed25519")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn appending_preserves_existing_entries_and_repairs_a_missing_newline() {
        let dir = scratch_dir("preserve");
        let path = dir.join("connections");
        fs::write(&path, "Host old\n    User someone").expect("seed the file");

        append_at(&path, &entry()).expect("append succeeds");
        let text = fs::read_to_string(&path).expect("file is readable");
        assert!(text.starts_with("Host old\n    User someone\n"), "{text}");
        assert!(text.contains("Host prod"), "{text}");

        let config = SshConfig::load_at(&path);
        assert_eq!(
            config.lookup("old").unwrap().user.as_deref(),
            Some("someone")
        );
        assert!(config.lookup("prod").is_some());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_duplicate_alias_is_refused_without_touching_the_file() {
        let dir = scratch_dir("duplicate");
        let path = dir.join("connections");
        append_at(&path, &entry()).expect("first append");
        let before = fs::read_to_string(&path).expect("file is readable");

        let err = append_at(&path, &entry()).expect_err("the alias already exists");
        assert!(format!("{err:#}").contains("already defined"), "{err:#}");
        assert_eq!(before, fs::read_to_string(&path).expect("file is readable"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn password_connections_render_no_identity_file_and_no_secret_field() {
        let mut password = entry();
        password.identity_file = None;

        let text = render(&password).expect("valid block");
        assert!(!text.contains("IdentityFile"), "{text}");
        assert!(!text.contains("Password"), "{text}");
        assert!(!text.contains("passphrase"), "{text}");
    }

    #[test]
    fn aliases_are_restricted_to_config_safe_literals() {
        for good in ["prod", "prod-1.example_2", "10.0.0.5"] {
            assert!(valid_alias(good), "{good} should be accepted");
        }
        for bad in ["", ".", "..", "-x", "a b", "a@b", "a:b", "!x", "a,b", "a*b"] {
            assert!(!valid_alias(bad), "{bad} should be rejected");
        }
    }

    #[test]
    fn ipv6_hosts_get_a_sanitized_default_alias() {
        assert_eq!(default_alias("deploy", "example.com"), "example.com");
        assert_eq!(default_alias("deploy", "10.0.0.5"), "10.0.0.5");
        assert_eq!(default_alias("deploy", "::1"), "deploy-1");
    }

    #[test]
    fn add_to_config_makes_the_alias_resolve_in_memory() {
        let mut config = SshConfig::default();
        add_to_config(&mut config, &entry()).expect("renders cleanly");

        let resolved = config.lookup("prod").expect("the alias resolves");
        assert_eq!(resolved.user.as_deref(), Some("deploy"));
        assert_eq!(resolved.port, Some(2222));
    }

    #[cfg(unix)]
    #[test]
    fn a_new_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch_dir("mode");
        let path = dir.join("connections");
        append_at(&path, &entry()).expect("append succeeds");

        let mode = fs::metadata(&path)
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        let _ = fs::remove_dir_all(&dir);
    }
}
