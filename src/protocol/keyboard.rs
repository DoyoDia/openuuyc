//! UU Windows peers consume VKs; Mac peers pass native key codes directly to CGEvent.
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum KeyboardKey {
    Windows(u16),
    Mac(u16),
}

impl KeyboardKey {
    pub(crate) fn from_windows_vk(platform: i32, key: u16) -> Option<Self> {
        match platform {
            1 if (1..=254).contains(&key) => Some(Self::Windows(key)),
            4 => mac_key_code(key).map(Self::Mac),
            _ => None,
        }
    }

    pub(crate) fn is_modifier(self) -> bool {
        match self {
            Self::Windows(key) => matches!(key, 16..=18 | 91..=92 | 160..=165),
            Self::Mac(key) => matches!(key, 0x36..=0x38 | 0x3a..=0x3f),
        }
    }

    pub(crate) fn encode(self, down: bool, lock: Option<u8>, interrupt: bool) -> Value {
        let key = match self {
            Self::Windows(key) | Self::Mac(key) => key,
        };
        let mut value = json!({
            "action": if down { "kbd_press" } else { "kbd_release" }, "key": key
        });
        if matches!(self, Self::Windows(_)) {
            if interrupt {
                value["interrept"] = "i".into();
            }
            if let Some(status) = lock {
                value[if key == 144 {
                    "lockkeysstatus"
                } else {
                    "status_key_value"
                }] = status.into();
            }
        }
        value
    }
}

// Apple HIToolbox/Events.h: kVK_ANSI_* and layout-independent virtual key codes.
// Keys without a Mac counterpart must not fall through as Windows VKs.
fn mac_key_code(key: u16) -> Option<u16> {
    Some(match key {
        0x08 => 0x33,        // Backspace / Delete
        0x09 => 0x30,        // Tab
        0x0c => 0x47,        // Clear
        0x0d => 0x24,        // Return
        0x10 | 0xa0 => 0x38, // Left Shift
        0x11 | 0xa2 => 0x3b, // Left Control
        0x12 | 0xa4 => 0x3a, // Left Alt / Option
        0x14 => 0x39,        // Caps Lock
        0x1b => 0x35,        // Escape
        0x20 => 0x31,        // Space
        0x21 => 0x74,        // Page Up
        0x22 => 0x79,        // Page Down
        0x23 => 0x77,        // End
        0x24 => 0x73,        // Home
        0x25 => 0x7b,        // Left
        0x26 => 0x7e,        // Up
        0x27 => 0x7c,        // Right
        0x28 => 0x7d,        // Down
        0x2e => 0x75,        // Forward Delete
        0x2f => 0x72,        // Help
        0x30 => 0x1d,        // 0
        0x31 => 0x12,        // 1
        0x32 => 0x13,        // 2
        0x33 => 0x14,        // 3
        0x34 => 0x15,        // 4
        0x35 => 0x17,        // 5
        0x36 => 0x16,        // 6
        0x37 => 0x1a,        // 7
        0x38 => 0x1c,        // 8
        0x39 => 0x19,        // 9
        0x41 => 0x00,        // A: zero is a valid Mac key code.
        0x42 => 0x0b,        // B
        0x43 => 0x08,        // C
        0x44 => 0x02,        // D
        0x45 => 0x0e,        // E
        0x46 => 0x03,        // F
        0x47 => 0x05,        // G
        0x48 => 0x04,        // H
        0x49 => 0x22,        // I
        0x4a => 0x26,        // J
        0x4b => 0x28,        // K
        0x4c => 0x25,        // L
        0x4d => 0x2e,        // M
        0x4e => 0x2d,        // N
        0x4f => 0x1f,        // O
        0x50 => 0x23,        // P
        0x51 => 0x0c,        // Q
        0x52 => 0x0f,        // R
        0x53 => 0x01,        // S
        0x54 => 0x11,        // T
        0x55 => 0x20,        // U
        0x56 => 0x09,        // V
        0x57 => 0x0d,        // W
        0x58 => 0x07,        // X
        0x59 => 0x10,        // Y
        0x5a => 0x06,        // Z
        0x5b => 0x37,        // Left Windows / Command
        0x5c => 0x36,        // Right Windows / Command
        0x60 => 0x52,        // Keypad 0
        0x61 => 0x53,        // Keypad 1
        0x62 => 0x54,        // Keypad 2
        0x63 => 0x55,        // Keypad 3
        0x64 => 0x56,        // Keypad 4
        0x65 => 0x57,        // Keypad 5
        0x66 => 0x58,        // Keypad 6
        0x67 => 0x59,        // Keypad 7
        0x68 => 0x5b,        // Keypad 8
        0x69 => 0x5c,        // Keypad 9
        0x6a => 0x43,        // Keypad Multiply
        0x6b => 0x45,        // Keypad Plus
        0x6d => 0x4e,        // Keypad Minus
        0x6e => 0x41,        // Keypad Decimal
        0x6f => 0x4b,        // Keypad Divide
        0x70 => 0x7a,        // F1
        0x71 => 0x78,        // F2
        0x72 => 0x63,        // F3
        0x73 => 0x76,        // F4
        0x74 => 0x60,        // F5
        0x75 => 0x61,        // F6
        0x76 => 0x62,        // F7
        0x77 => 0x64,        // F8
        0x78 => 0x65,        // F9
        0x79 => 0x6d,        // F10
        0x7a => 0x67,        // F11
        0x7b => 0x6f,        // F12
        0x7c => 0x69,        // F13
        0x7d => 0x6b,        // F14
        0x7e => 0x71,        // F15
        0x7f => 0x6a,        // F16
        0x80 => 0x40,        // F17
        0x81 => 0x4f,        // F18
        0x82 => 0x50,        // F19
        0x83 => 0x5a,        // F20
        0xa1 => 0x3c,        // Right Shift
        0xa3 => 0x3e,        // Right Control
        0xa5 => 0x3d,        // Right Alt / Option
        0xba => 0x29,        // Semicolon
        0xbb => 0x18,        // Equal
        0xbc => 0x2b,        // Comma
        0xbd => 0x1b,        // Minus
        0xbe => 0x2f,        // Period
        0xbf => 0x2c,        // Slash
        0xc0 => 0x32,        // Grave
        0xdb => 0x21,        // Left Bracket
        0xdc => 0x2a,        // Backslash
        0xdd => 0x1e,        // Right Bracket
        0xde => 0x27,        // Quote
        0xe2 => 0x0a,        // ISO Section
        _ => return None,
    })
}
