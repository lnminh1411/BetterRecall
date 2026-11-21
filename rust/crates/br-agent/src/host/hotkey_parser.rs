// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Hotkey parser and canonical formatter (`HotkeyParser.cs`).

/// A parsed hotkey: `RegisterHotKey` modifiers (`MOD_*`) and a virtual-key code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub modifiers: u32,
    pub virtual_key: u32,
}

impl Hotkey {
    pub const MOD_ALT: u32 = 0x1;
    pub const MOD_CONTROL: u32 = 0x2;
    pub const MOD_SHIFT: u32 = 0x4;
    pub const MOD_WIN: u32 = 0x8;
}

pub struct HotkeyParser;

impl HotkeyParser {
    /// Returns `None` when empty, has no key, repeats a part, or names an unknown key.
    pub fn parse(text: Option<&str>) -> Option<Hotkey> {
        let raw = text?.trim();
        if raw.is_empty() {
            return None;
        }
        // "Ctrl++" style: a trailing '+' after a separator is the Plus key.
        let mut parts: Vec<&str> = Vec::new();
        let bytes = raw.as_bytes();
        let mut start = 0usize;
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'+' && i > start {
                parts.push(raw[start..i].trim());
                start = i + 1;
            }
        }
        if start < raw.len() {
            parts.push(raw[start..].trim());
        } else if raw.ends_with('+') {
            parts.push("Plus");
        }

        let mut mods = 0u32;
        let mut vk: Option<u32> = None;
        for part in parts {
            if part.is_empty() {
                return None;
            }
            let m = match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => Hotkey::MOD_CONTROL,
                "alt" => Hotkey::MOD_ALT,
                "shift" => Hotkey::MOD_SHIFT,
                "win" | "windows" | "meta" | "super" => Hotkey::MOD_WIN,
                _ => 0,
            };
            if m != 0 {
                if (mods & m) != 0 {
                    return None;
                }
                mods |= m;
                continue;
            }
            if vk.is_some() {
                return None;
            }
            vk = Some(key_code(part)?);
        }
        Some(Hotkey {
            modifiers: mods,
            virtual_key: vk?,
        })
    }

    pub fn format(hk: Hotkey) -> String {
        let mut parts: Vec<String> = Vec::new();
        if (hk.modifiers & Hotkey::MOD_CONTROL) != 0 {
            parts.push("Ctrl".to_string());
        }
        if (hk.modifiers & Hotkey::MOD_ALT) != 0 {
            parts.push("Alt".to_string());
        }
        if (hk.modifiers & Hotkey::MOD_SHIFT) != 0 {
            parts.push("Shift".to_string());
        }
        if (hk.modifiers & Hotkey::MOD_WIN) != 0 {
            parts.push("Win".to_string());
        }
        let key = match hk.virtual_key {
            0x70..=0x87 => format!("F{}", hk.virtual_key - 0x70 + 1),
            0x41..=0x5A | 0x30..=0x39 => (hk.virtual_key as u8 as char).to_string(),
            0x60..=0x69 => format!("NumPad{}", hk.virtual_key - 0x60),
            v => canonical_name(v)
                .map(String::from)
                .unwrap_or_else(|| format!("0x{v:02X}")),
        };
        parts.push(key);
        parts.join("+")
    }
}

fn key_code(key: &str) -> Option<u32> {
    if key.len() == 1 {
        let c = key.as_bytes()[0].to_ascii_uppercase();
        if c.is_ascii_uppercase() || c.is_ascii_digit() {
            return Some(c as u32);
        }
    }
    if let Some(v) = named_vk(key) {
        return Some(v);
    }
    let b = key.as_bytes();
    if b.len() >= 2
        && (b[0] == b'F' || b[0] == b'f')
        && let Ok(f) = key[1..].parse::<u32>()
        && (1..=24).contains(&f)
    {
        return Some(0x70 + f - 1);
    }
    for prefix in ["NumPad", "Num"] {
        if key.len() == prefix.len() + 1
            && key.is_char_boundary(prefix.len())
            && key[..prefix.len()].eq_ignore_ascii_case(prefix)
        {
            let last = key.as_bytes()[prefix.len()];
            if last.is_ascii_digit() {
                return Some(0x60 + (last - b'0') as u32);
            }
        }
    }
    None
}

fn named_vk(key: &str) -> Option<u32> {
    Some(match key.to_ascii_lowercase().as_str() {
        "space" | "spacebar" => 0x20,
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "esc" | "escape" => 0x1B,
        "backspace" | "back" => 0x08,
        "insert" | "ins" => 0x2D,
        "delete" | "del" => 0x2E,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "pgup" => 0x21,
        "pagedown" | "pgdn" => 0x22,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "printscreen" | "prtsc" => 0x2C,
        "pause" => 0x13,
        "scrolllock" => 0x91,
        "plus" | "+" | "=" => 0xBB,
        "minus" | "-" => 0xBD,
        "," | "comma" => 0xBC,
        "." | "period" => 0xBE,
        "/" => 0xBF,
        ";" => 0xBA,
        "'" => 0xDE,
        "[" => 0xDB,
        "]" => 0xDD,
        "\\" => 0xDC,
        "`" | "backtick" => 0xC0,
        _ => return None,
    })
}

fn canonical_name(vk: u32) -> Option<&'static str> {
    Some(match vk {
        0x20 => "Space",
        0x21 => "PageUp",
        0x22 => "PageDown",
        0x23 => "End",
        0x24 => "Home",
        0x25 => "Left",
        0x26 => "Up",
        0x27 => "Right",
        0x28 => "Down",
        0x2D => "Insert",
        0x2E => "Delete",
        0x13 => "Pause",
        0x2C => "PrintScreen",
        0xBA => ";",
        0xBB => "=",
        0xBC => ",",
        0xBD => "-",
        0xBE => ".",
        0xBF => "/",
        0xC0 => "`",
        0xDB => "[",
        0xDC => "\\",
        0xDD => "]",
        0xDE => "'",
        0x0D => "Enter",
        0x09 => "Tab",
        0x1B => "Esc",
        0x08 => "Backspace",
        0x91 => "ScrollLock",
        _ => return None,
    })
}
