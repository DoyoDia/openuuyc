//! Resolve protocol VKs without confusing navigation keys with the keypad.

pub(super) fn scan_code(key: u16) -> u16 {
    // These logical keys belong to the dedicated navigation cluster. Some
    // Windows layouts return the shared keypad scan without E0 even for
    // MAPVK_VK_TO_VSC_EX, so their identity must come from the protocol VK.
    match key {
        0x21 => 0xe049, // Page Up
        0x22 => 0xe051, // Page Down
        0x23 => 0xe04f, // End
        0x24 => 0xe047, // Home
        0x25 => 0xe04b, // Left
        0x26 => 0xe048, // Up
        0x27 => 0xe04d, // Right
        0x28 => 0xe050, // Down
        0x2d => 0xe052, // Insert
        0x2e => 0xe053, // Delete
        _ => {
            use windows::Win32::UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*};
            unsafe {
                MapVirtualKeyExW(
                    u32::from(key),
                    MAPVK_VK_TO_VSC_EX,
                    Some(GetKeyboardLayout(GetWindowThreadProcessId(
                        GetForegroundWindow(),
                        None,
                    ))),
                ) as u16
            }
        }
    }
}
