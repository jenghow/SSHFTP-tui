//! A tested subset of `ssh_config(5)`, so the `Host` aliases in
//! `~/.ssh/config` can prefill the connect form.
//!
//! Supported:
//! * `Host` blocks with whitespace/comma separated patterns, `*` and `?`
//!   wildcards and `!` negation, matched against the name the user typed;
//! * the OpenSSH rule that the **first value obtained** for an option wins, so
//!   one host can be assembled from several matching blocks (`Host prod`, then
//!   `Host *`);
//! * `HostName`, `User`, `Port`, `IdentityFile` and `Include`;
//! * the `%% %d %h %n %p %r` tokens.
//!
//! Everything else is ignored. `ProxyJump`/`ProxyCommand` are the exception:
//! they are reported, not ignored, because this client has no jump-host
//! transport and quietly dialling the target directly would not be the same
//! connection.
//!
//! The file is read **once**, from `main::run` before the UI loop starts, since
//! the UI task must not touch the disk. Lookups afterwards are pure.

use std::path::{Path, PathBuf};

use crate::hostkey;

/// Overrides the config path. Tests point this at a temp file; people who keep
/// their config outside `~/.ssh` can do the same.
pub const CONFIG_ENV: &str = "SSHFTP_TUI_SSH_CONFIG";

/// Include files are a tree; this stops a symlink loop from hanging startup.
const MAX_INCLUDE_DEPTH: usize = 8;

/// What the connect screen calls the file unless `SSHFTP_TUI_SSH_CONFIG`
/// points somewhere else.
const DEFAULT_LABEL: &str = "~/.ssh/config";

#[derive(Debug, Clone)]
pub struct SshConfig {
    blocks: Vec<Block>,
    label: String,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            blocks: Vec::new(),
            label: DEFAULT_LABEL.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
struct Block {
    patterns: Vec<String>,
    options: Vec<Entry>,
}

#[derive(Debug, Clone)]
struct Entry {
    /// Lowercased, so `Hostname` and `HostName` are the same keyword.
    key: String,
    value: String,
}

/// The values a matching block (or set of blocks) supplies. Each `Option` is
/// `None` when the config says nothing, so the caller can keep the form's own
/// value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedHost {
    pub alias: String,
    pub hostname: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<String>,
    /// `Some("ProxyJump bastion")` when the block cannot be honoured.
    pub proxy: Option<String>,
}

impl SshConfig {
    /// `SSHFTP_TUI_SSH_CONFIG`, else `~/.ssh/config`. A missing or unreadable
    /// file simply yields an empty config, exactly like `ssh`.
    pub fn load() -> Self {
        match config_path() {
            Some(path) => Self::load_at(&path),
            None => Self::default(),
        }
    }

    pub fn load_at(path: &Path) -> Self {
        let mut config = Self {
            blocks: Vec::new(),
            label: label_for(path),
        };
        config.read(path, 0);
        config
    }

    /// The name to show on screen when this config supplied a value.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Used for the app-owned saved-connections file, whose label is not the
    /// `~/.ssh/config` default detected by [`label_for`].
    pub fn set_label(&mut self, label: impl Into<String>) {
        self.label = label.into();
    }

    /// Whether a block declares `alias` literally. Wildcards do not count: a
    /// normal `Host *` block must not make every save look like a duplicate.
    pub fn defines_alias(&self, alias: &str) -> bool {
        self.blocks.iter().any(|block| {
            let mut positive = false;
            for pattern in &block.patterns {
                if let Some(negated) = pattern.strip_prefix('!') {
                    if glob_match(negated, alias) {
                        return false;
                    }
                } else if pattern.eq_ignore_ascii_case(alias) {
                    positive = true;
                }
            }
            positive
        })
    }

    /// Appends the blocks parsed from `text`. Used after a successful save so
    /// the alias resolves in this session without re-reading the disk.
    pub fn add_text(&mut self, text: &str) {
        let parsed = Self::parse(text);
        self.blocks.extend(parsed.blocks);
    }

    /// Parses config text directly; `Include` resolves relative to the current
    /// directory. Used by the tests.
    pub fn parse(text: &str) -> Self {
        let mut config = Self::default();
        config.parse_into(text, Path::new(""), 0);
        config
    }

    /// Resolves `host` as typed against every block, first value wins.
    pub fn lookup(&self, host: &str) -> Option<ResolvedHost> {
        let host = host.trim();
        if host.is_empty() {
            return None;
        }

        let mut hostname = None;
        let mut user = None;
        let mut port = None;
        let mut identity = None;
        let mut proxy = None;

        for block in &self.blocks {
            if !block.matches(host) {
                continue;
            }
            for entry in &block.options {
                match entry.key.as_str() {
                    "hostname" if hostname.is_none() => hostname = Some(entry.value.clone()),
                    "user" if user.is_none() => user = Some(entry.value.clone()),
                    "port" if port.is_none() => {
                        port = entry.value.parse::<u16>().ok().filter(|port| *port > 0);
                    }
                    "identityfile" if identity.is_none() => identity = Some(entry.value.clone()),
                    // The first proxy-related keyword is enough to refuse.
                    "proxyjump" | "proxycommand" if proxy.is_none() => {
                        proxy = Some(format!("{} {}", display_key(&entry.key), entry.value));
                    }
                    _ => {}
                }
            }
        }

        if hostname.is_none()
            && user.is_none()
            && port.is_none()
            && identity.is_none()
            && proxy.is_none()
        {
            return None;
        }

        // Two passes: `%h`/`%p`/`%r` are only meaningful once the hostname and
        // port are known, and `%r` only once the user is.
        let port_value = port.unwrap_or(22);
        let hostname = expand(
            hostname.as_deref().unwrap_or(host),
            &Tokens {
                alias: host,
                hostname: host,
                user: None,
                port: port_value,
            },
        );
        let user = user.map(|value| {
            expand(
                &value,
                &Tokens {
                    alias: host,
                    hostname: &hostname,
                    user: None,
                    port: port_value,
                },
            )
        });
        let identity = identity.map(|value| {
            expand(
                &value,
                &Tokens {
                    alias: host,
                    hostname: &hostname,
                    user: user.as_deref(),
                    port: port_value,
                },
            )
        });

        Some(ResolvedHost {
            alias: host.to_string(),
            hostname,
            user,
            port,
            identity_file: identity,
            proxy,
        })
    }

    fn read(&mut self, path: &Path, depth: usize) {
        if depth > MAX_INCLUDE_DEPTH {
            return;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let base = path.parent().unwrap_or(Path::new(""));
        self.parse_into(&text, base, depth);
    }

    fn parse_into(&mut self, text: &str, base: &Path, depth: usize) {
        let mut current: Option<Block> = None;

        for raw in text.lines() {
            let line = raw.trim();
            // Only whole-line comments are comments, like OpenSSH; a trailing
            // `# ...` is dropped by `first_value` because it reads one token.
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((keyword, rest)) = split_keyword(line) else {
                continue;
            };

            match keyword.to_ascii_lowercase().as_str() {
                "host" => {
                    self.flush(&mut current);
                    current = Some(Block {
                        patterns: split_patterns(rest),
                        options: Vec::new(),
                    });
                }
                // `Match` blocks are not supported. Keeping a block that
                // matches nothing stops their options leaking into the next
                // `Host`.
                "match" => {
                    self.flush(&mut current);
                    current = Some(Block {
                        patterns: Vec::new(),
                        options: Vec::new(),
                    });
                }
                "include" => {
                    // Textual inclusion: the blocks land here, in file order.
                    self.flush(&mut current);
                    for path in include_paths(rest, base) {
                        self.read(&path, depth + 1);
                    }
                }
                _ => {
                    let Some(block) = current.as_mut() else {
                        continue;
                    };
                    let Some(value) = first_value(rest) else {
                        continue;
                    };
                    let key = keyword.to_ascii_lowercase();
                    // First value wins, including repeats inside one block.
                    if block.options.iter().any(|entry| entry.key == key) {
                        continue;
                    }
                    block.options.push(Entry { key, value });
                }
            }
        }

        self.flush(&mut current);
    }

    fn flush(&mut self, current: &mut Option<Block>) {
        if let Some(block) = current.take() {
            self.blocks.push(block);
        }
    }
}

impl Block {
    fn matches(&self, host: &str) -> bool {
        if self.patterns.is_empty() {
            return false;
        }
        let mut positive = false;
        for pattern in &self.patterns {
            match pattern.strip_prefix('!') {
                Some(negated) => {
                    if glob_match(negated, host) {
                        return false;
                    }
                }
                None => positive |= glob_match(pattern, host),
            }
        }
        positive
    }
}

fn config_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(CONFIG_ENV) {
        return Some(PathBuf::from(path));
    }
    hostkey::home_dir().map(|home| home.join(".ssh").join("config"))
}

/// `~/.ssh/config` for the default location, the path as given otherwise, so
/// the connect screen never names a file that was not the one read.
fn label_for(path: &Path) -> String {
    let default = hostkey::home_dir().map(|home| home.join(".ssh").join("config"));
    if default.as_deref() == Some(path) {
        return DEFAULT_LABEL.to_string();
    }
    path.display().to_string()
}

/// `Keyword value`, `Keyword=value` or `Keyword = value`.
fn split_keyword(line: &str) -> Option<(&str, &str)> {
    let end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    if end == 0 {
        return None;
    }
    let rest = line[end..]
        .trim_start_matches(|c: char| c.is_whitespace() || c == '=')
        .trim();
    Some((&line[..end], rest))
}

/// One value, without the trailing `# comment` or extra arguments.
fn first_value(rest: &str) -> Option<String> {
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    if let Some(inner) = rest.strip_prefix('"') {
        if let Some(end) = inner.find('"') {
            return Some(inner[..end].to_string());
        }
    }
    Some(rest.split_whitespace().next().unwrap_or(rest).to_string())
}

fn split_patterns(rest: &str) -> Vec<String> {
    rest.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|pattern| !pattern.is_empty())
        .map(str::to_string)
        .collect()
}

fn display_key(key: &str) -> &'static str {
    match key {
        "proxyjump" => "ProxyJump",
        _ => "ProxyCommand",
    }
}

fn include_paths(value: &str, base: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for token in value.split_whitespace() {
        paths.extend(expand_glob(resolve_include(token, base)));
    }
    paths.sort();
    paths
}

fn resolve_include(token: &str, base: &Path) -> PathBuf {
    let expanded = expand_home(token);
    if expanded.is_absolute() || base.as_os_str().is_empty() {
        expanded
    } else {
        base.join(expanded)
    }
}

/// `~`/`~/x`, with the same home rule as the known_hosts path (`USERPROFILE`
/// on Windows, `HOME` elsewhere).
fn expand_home(value: &str) -> PathBuf {
    if let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        if let Some(home) = hostkey::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(value)
}

/// Globs only the file name of an `Include` (`config.d/*`), which is how these
/// are written in practice; a wildcard in a parent directory is not expanded.
fn expand_glob(path: PathBuf) -> Vec<PathBuf> {
    let Some(pattern) = path.file_name().and_then(|name| name.to_str()) else {
        return vec![path];
    };
    if !pattern.contains('*') && !pattern.contains('?') {
        return vec![path];
    }
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| glob_match(pattern, name))
        })
        .map(|entry| entry.path())
        .collect();
    found.sort();
    found
}

struct Tokens<'a> {
    alias: &'a str,
    hostname: &'a str,
    user: Option<&'a str>,
    port: u16,
}

fn expand(value: &str, tokens: &Tokens<'_>) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some('d') => {
                if let Some(home) = hostkey::home_dir() {
                    out.push_str(&home.to_string_lossy());
                }
            }
            Some('h') => out.push_str(tokens.hostname),
            Some('n') => out.push_str(tokens.alias),
            Some('p') => out.push_str(&tokens.port.to_string()),
            Some('r') => out.push_str(tokens.user.unwrap_or("")),
            // Unknown tokens stay as written rather than silently vanishing.
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

/// `*` and `?` only, case-insensitively; hostnames are not case-sensitive and
/// neither is OpenSSH's matcher.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.to_lowercase().chars().collect();
    let text: Vec<char> = text.to_lowercase().chars().collect();

    let (mut p, mut t) = (0, 0);
    let mut star: Option<usize> = None;
    let mut resume = 0;

    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            resume = t;
            p += 1;
        } else if let Some(position) = star {
            p = position + 1;
            resume += 1;
            t = resume;
        } else {
            return false;
        }
    }

    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn lookup(config: &str, host: &str) -> Option<ResolvedHost> {
        SshConfig::parse(config).lookup(host)
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("sshftp-tui-sshconfig-{tag}-{nanos}"))
    }

    #[test]
    fn an_alias_resolves_to_its_endpoint() {
        let resolved = lookup(
            "\
Host prod
    HostName 10.0.0.5
    User deploy
    Port 2222
    IdentityFile ~/.ssh/prod_ed25519
",
            "prod",
        )
        .expect("prod is configured");

        assert_eq!(resolved.alias, "prod");
        assert_eq!(resolved.hostname, "10.0.0.5");
        assert_eq!(resolved.user.as_deref(), Some("deploy"));
        assert_eq!(resolved.port, Some(2222));
        assert_eq!(
            resolved.identity_file.as_deref(),
            Some("~/.ssh/prod_ed25519")
        );
        assert_eq!(resolved.proxy, None);
    }

    #[test]
    fn keywords_are_case_insensitive_and_accept_equals() {
        let resolved = lookup(
            "host Box\nhostname=box.example.com\nUSER=root\nPORT = 2222\n",
            "Box",
        )
        .expect("box is configured");

        assert_eq!(resolved.hostname, "box.example.com");
        assert_eq!(resolved.user.as_deref(), Some("root"));
        assert_eq!(resolved.port, Some(2222));
    }

    #[test]
    fn host_patterns_support_wildcards_and_negation() {
        let config = "\
Host *.example.com !db.example.com
    User web
Host *
    User fallback
    Port 2200
";

        assert_eq!(
            lookup(config, "web.example.com").unwrap().user.as_deref(),
            Some("web")
        );
        // The negated pattern refuses the first block, so `Host *` supplies it.
        let db = lookup(config, "db.example.com").unwrap();
        assert_eq!(db.user.as_deref(), Some("fallback"));
        assert_eq!(db.port, Some(2200));
        assert_eq!(
            lookup(config, "unrelated.test").unwrap().user.as_deref(),
            Some("fallback")
        );
    }

    #[test]
    fn the_first_value_obtained_wins_across_blocks() {
        let resolved = lookup(
            "\
Host prod
    User first
Host prod
    User second
    Port 2222
",
            "prod",
        )
        .unwrap();

        assert_eq!(resolved.user.as_deref(), Some("first"));
        assert_eq!(resolved.port, Some(2222), "later blocks fill what is unset");
    }

    #[test]
    fn tokens_expand_in_names_and_paths() {
        let resolved = lookup(
            "\
Host prod
    HostName %n.internal.example
    User deploy
    Port 2200
    IdentityFile ~/.ssh/%r_%h_%p
",
            "prod",
        )
        .unwrap();

        assert_eq!(resolved.hostname, "prod.internal.example");
        assert_eq!(
            resolved.identity_file.as_deref(),
            Some("~/.ssh/deploy_prod.internal.example_2200")
        );
    }

    #[test]
    fn proxy_options_are_reported_not_ignored() {
        assert_eq!(
            lookup(
                "Host only-via-bastion\n    ProxyJump bastion\n",
                "only-via-bastion"
            )
            .unwrap()
            .proxy
            .as_deref(),
            Some("ProxyJump bastion")
        );
        assert_eq!(
            lookup("Host tunnel\n    ProxyCommand nc %h %p\n", "tunnel")
                .unwrap()
                .proxy
                .as_deref(),
            Some("ProxyCommand nc")
        );
    }

    #[test]
    fn match_blocks_are_ignored() {
        let config = "\
Match host prod
    User ignored
Host prod
    Port 2222
";
        let resolved = lookup(config, "prod").unwrap();
        assert_eq!(resolved.user, None, "Match options must not leak");
        assert_eq!(resolved.port, Some(2222));
    }

    #[test]
    fn an_unconfigured_host_has_no_match() {
        assert!(lookup("Host prod\n    User deploy\n", "other").is_none());
        assert!(lookup("", "anything").is_none());
    }

    #[test]
    fn include_files_are_inlined_in_order() {
        let dir = scratch_dir("include");
        std::fs::create_dir_all(dir.join("config.d")).unwrap();
        std::fs::write(
            dir.join("config"),
            "Host prod\n    User deploy\nInclude config.d/*\nHost extra\n    Port 2222\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("config.d").join("10-keys"),
            "Host prod\n    IdentityFile ~/.ssh/prod\n",
        )
        .unwrap();

        let config = SshConfig::load_at(&dir.join("config"));
        let prod = config.lookup("prod").expect("prod from the main file");
        assert_eq!(
            prod.user.as_deref(),
            Some("deploy"),
            "declared before the include"
        );
        assert_eq!(
            prod.identity_file.as_deref(),
            Some("~/.ssh/prod"),
            "supplied by the included file"
        );
        assert_eq!(config.lookup("extra").unwrap().port, Some(2222));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn defines_alias_ignores_wildcards_and_negation() {
        let config = SshConfig::parse(
            "\
Host *
    User fallback
Host prod
    Port 2222
Host blocked !blocked
    User nobody
",
        );

        assert!(config.defines_alias("prod"));
        assert!(config.defines_alias("PROD"), "aliases are case-insensitive");
        assert!(
            !config.defines_alias("other"),
            "Host * must not look like a literal alias"
        );
        assert!(
            !config.defines_alias("blocked"),
            "a negated pattern cancels the block"
        );
    }

    #[test]
    fn add_text_makes_a_rendered_block_resolve_in_memory() {
        let mut config = SshConfig::parse("");
        config.add_text(
            "Host saved\n    HostName 10.1.2.3\n    User deploy\n    IdentityFile ~/.ssh/saved\n",
        );

        let resolved = config.lookup("saved").expect("the added block resolves");
        assert_eq!(resolved.hostname, "10.1.2.3");
        assert_eq!(resolved.user.as_deref(), Some("deploy"));
        assert_eq!(resolved.identity_file.as_deref(), Some("~/.ssh/saved"));
    }

    #[test]
    fn a_missing_config_file_is_empty() {
        let config = SshConfig::load_at(Path::new("definitely-not-a-real-ssh-config"));
        assert!(config.lookup("prod").is_none());
    }
}
