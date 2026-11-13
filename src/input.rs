use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Encodes a `crossterm::event::KeyEvent` into standard xterm/VT terminal input bytes,
/// honoring `application_cursor` (DECCKM) mode from the active pane's `vt100::Screen`.
pub fn encode_key_to_vt(key: KeyEvent, application_cursor: bool) -> Option<String> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    let mut out = String::new();

    match key.code {
        KeyCode::Char(c) => {
            if alt {
                out.push('\x1b');
            }
            if ctrl {
                if let Some(ctrl_ch) = encode_ctrl_char(c) {
                    out.push(ctrl_ch);
                } else {
                    out.push(c);
                }
            } else {
                out.push(c);
            }
        }
        KeyCode::Enter => {
            if alt {
                out.push('\x1b');
            }
            out.push('\r');
        }
        KeyCode::Backspace => {
            if alt {
                out.push('\x1b');
            }
            if ctrl {
                out.push('\x08');
            } else {
                out.push('\x7f');
            }
        }
        KeyCode::Tab => {
            if shift {
                out.push_str("\x1b[Z");
            } else {
                if alt {
                    out.push('\x1b');
                }
                out.push('\t');
            }
        }
        KeyCode::BackTab => {
            out.push_str("\x1b[Z");
        }
        KeyCode::Esc => {
            if alt {
                out.push('\x1b');
            }
            out.push('\x1b');
        }
        KeyCode::Up => out.push_str(&encode_cursor_key('A', shift, alt, ctrl, application_cursor)),
        KeyCode::Down => out.push_str(&encode_cursor_key('B', shift, alt, ctrl, application_cursor)),
        KeyCode::Right => out.push_str(&encode_cursor_key('C', shift, alt, ctrl, application_cursor)),
        KeyCode::Left => out.push_str(&encode_cursor_key('D', shift, alt, ctrl, application_cursor)),
        KeyCode::Home => out.push_str(&encode_cursor_key('H', shift, alt, ctrl, application_cursor)),
        KeyCode::End => out.push_str(&encode_cursor_key('F', shift, alt, ctrl, application_cursor)),
        KeyCode::Insert => out.push_str(&encode_tilde_key(2, shift, alt, ctrl)),
        KeyCode::Delete => out.push_str(&encode_tilde_key(3, shift, alt, ctrl)),
        KeyCode::PageUp => out.push_str(&encode_tilde_key(5, shift, alt, ctrl)),
        KeyCode::PageDown => out.push_str(&encode_tilde_key(6, shift, alt, ctrl)),
        KeyCode::F(n) => {
            if let Some(seq) = encode_function_key(n, shift, alt, ctrl) {
                out.push_str(&seq);
            }
        }
        _ => return None,
    }

    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

pub fn encode_paste_to_vt(text: &str, bracketed_paste: bool) -> String {
    if bracketed_paste {
        // Strip embedded end-of-paste markers so pasted text cannot break out early
        let safe = text.replace("\x1b[201~", "");
        format!("\x1b[200~{}\x1b[201~", safe)
    } else {
        text.to_string()
    }
}

fn encode_ctrl_char(c: char) -> Option<char> {
    let lower = c.to_ascii_lowercase();
    match lower {
        'a'..='z' => Some(((lower as u8) - b'a' + 1) as char),
        ' ' | '@' | '`' | '2' => Some('\x00'),
        '[' | '3' => Some('\x1b'),
        '\\' | '4' => Some('\x1c'),
        ']' | '5' => Some('\x1d'),
        '^' | '6' | '~' => Some('\x1e'),
        '_' | '/' | '7' | '?' => Some('\x1f'),
        '8' => Some('\x7f'),
        _ => None,
    }
}

fn xterm_modifier_param(shift: bool, alt: bool, ctrl: bool) -> u8 {
    let mut m = 1;
    if shift {
        m += 1;
    }
    if alt {
        m += 2;
    }
    if ctrl {
        m += 4;
    }
    m
}

fn encode_cursor_key(
    final_byte: char,
    shift: bool,
    alt: bool,
    ctrl: bool,
    application_cursor: bool,
) -> String {
    let m = xterm_modifier_param(shift, alt, ctrl);
    if m > 1 {
        format!("\x1b[1;{}{}", m, final_byte)
    } else if application_cursor {
        format!("\x1bO{}", final_byte)
    } else {
        format!("\x1b[{}", final_byte)
    }
}

fn encode_tilde_key(code: u8, shift: bool, alt: bool, ctrl: bool) -> String {
    let m = xterm_modifier_param(shift, alt, ctrl);
    if m > 1 {
        format!("\x1b[{};{}~", code, m)
    } else {
        format!("\x1b[{}~", code)
    }
}

fn encode_function_key(n: u8, shift: bool, alt: bool, ctrl: bool) -> Option<String> {
    let m = xterm_modifier_param(shift, alt, ctrl);
    match n {
        1..=4 => {
            let letter = match n {
                1 => 'P',
                2 => 'Q',
                3 => 'R',
                4 => 'S',
                _ => unreachable!(),
            };
            if m > 1 {
                Some(format!("\x1b[1;{}{}", m, letter))
            } else {
                Some(format!("\x1bO{}", letter))
            }
        }
        5 => Some(encode_tilde_key(15, shift, alt, ctrl)),
        6 => Some(encode_tilde_key(17, shift, alt, ctrl)),
        7 => Some(encode_tilde_key(18, shift, alt, ctrl)),
        8 => Some(encode_tilde_key(19, shift, alt, ctrl)),
        9 => Some(encode_tilde_key(20, shift, alt, ctrl)),
        10 => Some(encode_tilde_key(21, shift, alt, ctrl)),
        11 => Some(encode_tilde_key(23, shift, alt, ctrl)),
        12 => Some(encode_tilde_key(24, shift, alt, ctrl)),
        _ => None,
    }
}
