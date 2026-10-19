//! Mapping from crossterm key events to VT100/xterm byte sequences.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Encode a key press for the remote PTY. Returns `None` for keys that have no
/// meaning to a shell (media keys, lock keys, ...).
pub fn encode(key: KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    let mut bytes = match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                vec![control_byte(c)?]
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        }
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => cursor_key('A', ctrl, alt, shift),
        KeyCode::Down => cursor_key('B', ctrl, alt, shift),
        KeyCode::Right => cursor_key('C', ctrl, alt, shift),
        KeyCode::Left => cursor_key('D', ctrl, alt, shift),
        KeyCode::Home => cursor_key('H', ctrl, alt, shift),
        KeyCode::End => cursor_key('F', ctrl, alt, shift),
        KeyCode::Insert => tilde_key(2, ctrl, alt, shift),
        KeyCode::Delete => tilde_key(3, ctrl, alt, shift),
        KeyCode::PageUp => tilde_key(5, ctrl, alt, shift),
        KeyCode::PageDown => tilde_key(6, ctrl, alt, shift),
        KeyCode::F(n) => function_key(n, ctrl, alt, shift)?,
        _ => return None,
    };

    if alt {
        let mut prefixed = Vec::with_capacity(bytes.len() + 1);
        prefixed.push(0x1b);
        prefixed.append(&mut bytes);
        bytes = prefixed;
    }
    Some(bytes)
}

fn modifier_param(ctrl: bool, alt: bool, shift: bool) -> u8 {
    1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl)
}

fn cursor_key(letter: char, ctrl: bool, alt: bool, shift: bool) -> Vec<u8> {
    let modifier = modifier_param(ctrl, alt, shift);
    if modifier == 1 {
        format!("\x1b[{letter}").into_bytes()
    } else {
        format!("\x1b[1;{modifier}{letter}").into_bytes()
    }
}

fn tilde_key(code: u8, ctrl: bool, alt: bool, shift: bool) -> Vec<u8> {
    let modifier = modifier_param(ctrl, alt, shift);
    if modifier == 1 {
        format!("\x1b[{code}~").into_bytes()
    } else {
        format!("\x1b[{code};{modifier}~").into_bytes()
    }
}

fn ss3_key(letter: char, ctrl: bool, alt: bool, shift: bool) -> Vec<u8> {
    let modifier = modifier_param(ctrl, alt, shift);
    if modifier == 1 {
        format!("\x1bO{letter}").into_bytes()
    } else {
        format!("\x1b[1;{modifier}{letter}").into_bytes()
    }
}

fn function_key(n: u8, ctrl: bool, alt: bool, shift: bool) -> Option<Vec<u8>> {
    // xterm uses SS3 for F1-F4 and CSI <code>~ for F5 onwards.
    let modifier = modifier_param(ctrl, alt, shift);
    let code = match n {
        1 => return Some(ss3_key('P', ctrl, alt, shift)),
        2 => return Some(ss3_key('Q', ctrl, alt, shift)),
        3 => return Some(ss3_key('R', ctrl, alt, shift)),
        4 => return Some(ss3_key('S', ctrl, alt, shift)),
        5 => 15,
        6 => 17,
        7 => 18,
        8 => 19,
        9 => 20,
        10 => 21,
        11 => 23,
        12 => 24,
        _ => return None,
    };
    Some(if modifier == 1 {
        format!("\x1b[{code}~").into_bytes()
    } else {
        format!("\x1b[{code};{modifier}~").into_bytes()
    })
}

fn control_byte(c: char) -> Option<u8> {
    let c = c.to_ascii_lowercase();
    let byte = match c {
        ' ' | '@' => 0x00,
        'a'..='z' => (c as u8) - b'a' + 1,
        '[' => 0x1b,
        '\\' => 0x1c,
        ']' => 0x1d,
        '^' => 0x1e,
        '_' => 0x1f,
        '?' => 0x7f,
        _ => return None,
    };
    Some(byte)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn plain_text_is_utf8() {
        assert_eq!(
            encode(key(KeyCode::Char('a'), KeyModifiers::NONE)).unwrap(),
            b"a"
        );
        assert_eq!(
            encode(key(KeyCode::Char('é'), KeyModifiers::NONE)).unwrap(),
            "é".as_bytes()
        );
    }

    #[test]
    fn control_keys_emit_c0_bytes() {
        assert_eq!(
            encode(key(KeyCode::Char('c'), KeyModifiers::CONTROL)).unwrap(),
            vec![0x03]
        );
        assert_eq!(
            encode(key(KeyCode::Char('a'), KeyModifiers::CONTROL)).unwrap(),
            vec![0x01]
        );
        assert_eq!(
            encode(key(KeyCode::Char(' '), KeyModifiers::CONTROL)).unwrap(),
            vec![0x00]
        );
    }

    #[test]
    fn editing_keys_are_forwarded() {
        assert_eq!(
            encode(key(KeyCode::Enter, KeyModifiers::NONE)).unwrap(),
            b"\r"
        );
        assert_eq!(
            encode(key(KeyCode::Backspace, KeyModifiers::NONE)).unwrap(),
            vec![0x7f]
        );
        assert_eq!(
            encode(key(KeyCode::Tab, KeyModifiers::NONE)).unwrap(),
            b"\t"
        );
        assert_eq!(
            encode(key(KeyCode::Esc, KeyModifiers::NONE)).unwrap(),
            vec![0x1b]
        );
    }

    #[test]
    fn arrows_and_navigation() {
        assert_eq!(
            encode(key(KeyCode::Up, KeyModifiers::NONE)).unwrap(),
            b"\x1b[A"
        );
        assert_eq!(
            encode(key(KeyCode::Left, KeyModifiers::CONTROL)).unwrap(),
            b"\x1b[1;5D"
        );
        assert_eq!(
            encode(key(KeyCode::Delete, KeyModifiers::NONE)).unwrap(),
            b"\x1b[3~"
        );
        assert_eq!(
            encode(key(KeyCode::PageUp, KeyModifiers::SHIFT)).unwrap(),
            b"\x1b[5;2~"
        );
    }

    #[test]
    fn function_keys() {
        assert_eq!(
            encode(key(KeyCode::F(1), KeyModifiers::NONE)).unwrap(),
            b"\x1bOP"
        );
        assert_eq!(
            encode(key(KeyCode::F(5), KeyModifiers::NONE)).unwrap(),
            b"\x1b[15~"
        );
        assert_eq!(
            encode(key(KeyCode::F(10), KeyModifiers::NONE)).unwrap(),
            b"\x1b[21~"
        );
        assert_eq!(
            encode(key(KeyCode::F(12), KeyModifiers::NONE)).unwrap(),
            b"\x1b[24~"
        );
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(
            encode(key(KeyCode::Char('x'), KeyModifiers::ALT)).unwrap(),
            b"\x1bx"
        );
    }
}
