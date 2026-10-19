//! Editable text field and the startup connection form.

use std::path::PathBuf;

use crate::event::{AuthMethod, ConnectParams};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    Password,
    KeyFile,
}

/// The key path the form opens with; `apply_ssh_config` treats this value as
/// "not chosen yet" and may replace it with the block's `IdentityFile`.
pub const DEFAULT_KEY_PATH: &str = "~/.ssh/id_ed25519";

/// Where the connection values came from, shown on the connect screen when a
/// `Host` block filled them in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFill {
    /// The file that supplied the values (`~/.ssh/config`, or the override).
    pub source: String,
    pub alias: String,
    /// The resolved `HostName`; kept so re-resolving an unchanged form does not
    /// clear the note.
    pub hostname: String,
    /// `user@host:port`, for display only.
    pub endpoint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Host,
    Port,
    Username,
    Auth,
    Password,
    KeyPath,
    Passphrase,
}

impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Self::Host => "Host",
            Self::Port => "Port",
            Self::Username => "Username",
            Self::Auth => "Auth",
            Self::Password => "Password",
            Self::KeyPath => "Key file",
            Self::Passphrase => "Passphrase",
        }
    }
}

/// A single-line text editor. `cursor` is a char index, not a byte index.
#[derive(Debug, Clone, Default)]
pub struct TextField {
    pub value: String,
    pub cursor: usize,
}

impl TextField {
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.chars().count();
        Self { value, cursor }
    }

    fn byte_index(&self, char_index: usize) -> usize {
        self.value
            .char_indices()
            .nth(char_index)
            .map(|(i, _)| i)
            .unwrap_or(self.value.len())
    }

    pub fn char_len(&self) -> usize {
        self.value.chars().count()
    }

    pub fn insert(&mut self, c: char) {
        let at = self.byte_index(self.cursor);
        self.value.insert(at, c);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.byte_index(self.cursor - 1);
        let end = self.byte_index(self.cursor);
        self.value.replace_range(start..end, "");
        self.cursor -= 1;
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.char_len() {
            return;
        }
        let start = self.byte_index(self.cursor);
        let end = self.byte_index(self.cursor + 1);
        self.value.replace_range(start..end, "");
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.char_len());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.char_len();
    }

    pub fn trimmed(&self) -> &str {
        self.value.trim()
    }
}

#[derive(Debug)]
pub struct ConnectForm {
    pub host: TextField,
    pub port: TextField,
    pub username: TextField,
    pub auth: AuthKind,
    pub password: TextField,
    pub key_path: TextField,
    pub passphrase: TextField,
    pub focus: Field,
    pub error: Option<String>,
    pub connecting: bool,
    /// Set when `~/.ssh/config` supplied the host/user/port/key.
    pub config: Option<ConfigFill>,
}

impl Default for ConnectForm {
    fn default() -> Self {
        // Nothing about the local account is prefilled: the operator decides
        // which remote user to log in as. Only the transport defaults stay.
        Self {
            host: TextField::default(),
            port: TextField::new("22"),
            username: TextField::default(),
            auth: AuthKind::Password,
            password: TextField::default(),
            key_path: TextField::new(DEFAULT_KEY_PATH),
            passphrase: TextField::default(),
            focus: Field::Host,
            error: None,
            connecting: false,
            config: None,
        }
    }
}

impl ConnectForm {
    /// Prefill from `[user@]host[:port]` supplied on the command line.
    /// IPv6 literals must use brackets: `[::1]:2222`.
    pub fn apply_target(&mut self, target: &str) {
        let (user, rest) = match target.split_once('@') {
            Some((user, rest)) => (Some(user), rest),
            None => (None, target),
        };

        let (host, port) = if let Some(bracketed) = rest.strip_prefix('[') {
            match bracketed.split_once(']') {
                Some((host, after)) => (host, after.strip_prefix(':')),
                None => (rest, None),
            }
        } else {
            match rest.rsplit_once(':') {
                Some((host, port))
                    if !host.contains(':') && port.chars().all(|c| c.is_ascii_digit()) =>
                {
                    (host, Some(port))
                }
                _ => (rest, None),
            }
        };

        if let Some(user) = user {
            self.username = TextField::new(user);
        }
        if !host.is_empty() {
            self.host = TextField::new(host);
        }
        if let Some(port) = port {
            self.port = TextField::new(port);
        }
    }

    /// `user@host:port`, for the "where these values came from" line on the
    /// connect screen.
    pub fn endpoint(&self) -> String {
        let host = self.host.trimmed();
        let user = self.username.trimmed();
        let port = self.port.trimmed();
        if user.is_empty() {
            format!("{host}:{port}")
        } else {
            format!("{user}@{host}:{port}")
        }
    }

    pub fn visible_fields(&self) -> Vec<Field> {
        let mut fields = vec![Field::Host, Field::Port, Field::Username, Field::Auth];
        match self.auth {
            AuthKind::Password => fields.push(Field::Password),
            AuthKind::KeyFile => {
                fields.push(Field::KeyPath);
                fields.push(Field::Passphrase);
            }
        }
        fields
    }

    pub fn focus_next(&mut self, backwards: bool) {
        let fields = self.visible_fields();
        let Some(index) = fields.iter().position(|f| *f == self.focus) else {
            self.focus = Field::Host;
            return;
        };
        let len = fields.len();
        let next = if backwards {
            (index + len - 1) % len
        } else {
            (index + 1) % len
        };
        self.focus = fields[next];
    }

    pub fn toggle_auth(&mut self) {
        self.auth = match self.auth {
            AuthKind::Password => AuthKind::KeyFile,
            AuthKind::KeyFile => AuthKind::Password,
        };
        self.error = None;
    }

    pub fn field_mut(&mut self, field: Field) -> Option<&mut TextField> {
        Some(match field {
            Field::Host => &mut self.host,
            Field::Port => &mut self.port,
            Field::Username => &mut self.username,
            Field::Password => &mut self.password,
            Field::KeyPath => &mut self.key_path,
            Field::Passphrase => &mut self.passphrase,
            Field::Auth => return None,
        })
    }

    pub fn field(&self, field: Field) -> Option<&TextField> {
        Some(match field {
            Field::Host => &self.host,
            Field::Port => &self.port,
            Field::Username => &self.username,
            Field::Password => &self.password,
            Field::KeyPath => &self.key_path,
            Field::Passphrase => &self.passphrase,
            Field::Auth => return None,
        })
    }

    pub fn build_params(&self, cols: u16, rows: u16) -> Result<ConnectParams, String> {
        let host = self.host.trimmed().to_string();
        if host.is_empty() {
            return Err("Host is required".to_string());
        }
        let port: u16 = self
            .port
            .trimmed()
            .parse()
            .map_err(|_| "Port must be a number between 1 and 65535".to_string())?;
        if port == 0 {
            return Err("Port must be between 1 and 65535".to_string());
        }
        let username = self.username.trimmed().to_string();
        if username.is_empty() {
            return Err("Username is required".to_string());
        }

        let auth = match self.auth {
            AuthKind::Password => {
                if self.password.value.is_empty() {
                    return Err("Password is required".to_string());
                }
                AuthMethod::Password(self.password.value.clone())
            }
            AuthKind::KeyFile => {
                let raw = self.key_path.trimmed();
                if raw.is_empty() {
                    return Err("Key file path is required".to_string());
                }
                let path = expand_home(raw);
                if !path.exists() {
                    return Err(format!("Key file not found: {}", path.display()));
                }
                let passphrase = if self.passphrase.value.is_empty() {
                    None
                } else {
                    Some(self.passphrase.value.clone())
                };
                AuthMethod::KeyFile { path, passphrase }
            }
        };

        Ok(ConnectParams {
            host,
            port,
            username,
            auth,
            cols,
            rows,
        })
    }
}

pub fn expand_home(raw: &str) -> PathBuf {
    let Some(rest) = raw.strip_prefix("~/") else {
        return PathBuf::from(raw);
    };
    let home = if cfg!(windows) {
        std::env::var_os("USERPROFILE")
    } else {
        std::env::var_os("HOME")
    };
    match home {
        Some(home) => PathBuf::from(home).join(rest),
        None => PathBuf::from(raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_field_editing_is_char_aware() {
        let mut field = TextField::new("héllo");
        assert_eq!(field.cursor, 5);
        field.home();
        field.insert('>');
        assert_eq!(field.value, ">héllo");
        field.end();
        field.backspace();
        assert_eq!(field.value, ">héll");
        field.home();
        field.delete();
        assert_eq!(field.value, "héll");
    }

    #[test]
    fn the_default_form_does_not_guess_the_username() {
        let form = ConnectForm::default();
        assert!(
            form.username.value.is_empty(),
            "the local account must not leak into the login form"
        );
        assert_eq!(form.port.value, "22");
        assert_eq!(form.key_path.value, "~/.ssh/id_ed25519");
    }

    #[test]
    fn target_parsing() {
        let mut form = ConnectForm::default();
        form.apply_target("alice@example.com:2222");
        assert_eq!(form.username.value, "alice");
        assert_eq!(form.host.value, "example.com");
        assert_eq!(form.port.value, "2222");
    }

    #[test]
    fn bracketed_ipv6_targets_are_parsed() {
        let mut form = ConnectForm::default();
        form.apply_target("root@[::1]:2222");
        assert_eq!(form.username.value, "root");
        assert_eq!(form.host.value, "::1");
        assert_eq!(form.port.value, "2222");
    }

    #[test]
    fn invalid_port_is_rejected() {
        let form = ConnectForm {
            host: TextField::new("host"),
            password: TextField::new("secret"),
            port: TextField::new("nope"),
            ..Default::default()
        };
        assert!(form.build_params(80, 24).is_err());
    }
}
