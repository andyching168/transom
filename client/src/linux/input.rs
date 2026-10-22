//! winit input → protocol `InputEvent`s.
//!
//! As on Windows, the client sends **Client-Space physical pixels and Windows VK
//! codes** and nothing else (protocol.md §4): the host owns CS→VDS→AX translation,
//! the VK→macOS-keycode map and modifier state. This module only transcribes what
//! the Wayland seat reports.
//!
//! Keys are mapped by *physical position* (a US-layout key map). The Mac host
//! converts VKs to macOS keycodes, which are positional too, so the Mac's own
//! layout decides which character appears; forwarding the layout-translated
//! symbol here would apply the PC layout twice.
//!
//! Modifiers go out as the generic `VK_SHIFT`/`VK_CONTROL`/`VK_MENU` that
//! `WM_KEYDOWN` reports on Windows, because the host's modifier state machine
//! keys on them.

use egui_winit::winit::keyboard::KeyCode;

pub const DISCONNECT_SHORTCUT: &str = "Ctrl + Alt + Shift + D";

/// Reserved locally before a key is forwarded to the Mac.
pub fn disconnect_chord(code: KeyCode, ctrl: bool, alt: bool, shift: bool, logo: bool) -> bool {
    code == KeyCode::KeyD && ctrl && alt && shift && !logo
}

/// `WHEEL_DELTA`-equivalent: pixel scrolling (touchpads) accumulates until it
/// amounts to a line step, the unit the wire carries.
pub const PIXELS_PER_LINE: f64 = 40.0;

/// Accumulates fractional scroll so slow touchpad swipes still produce steps and
/// no motion is lost to rounding.
#[derive(Debug, Default)]
pub struct ScrollAccumulator {
    x: f64,
    y: f64,
}

impl ScrollAccumulator {
    pub fn add_lines(&mut self, dx: f64, dy: f64) -> (i32, i32) {
        self.x += dx;
        self.y += dy;
        self.take()
    }
    pub fn add_pixels(&mut self, dx: f64, dy: f64) -> (i32, i32) {
        self.add_lines(dx / PIXELS_PER_LINE, dy / PIXELS_PER_LINE)
    }
    fn take(&mut self) -> (i32, i32) {
        let (sx, sy) = (self.x.trunc(), self.y.trunc());
        self.x -= sx;
        self.y -= sy;
        (sx as i32, sy as i32)
    }
}

/// The Windows virtual-key code for a physical key, or `None` for keys with no
/// Windows equivalent (the host drops unmapped VKs anyway).
pub fn vk_for_code(code: KeyCode) -> Option<u32> {
    use KeyCode::*;
    Some(match code {
        KeyA => 0x41,
        KeyB => 0x42,
        KeyC => 0x43,
        KeyD => 0x44,
        KeyE => 0x45,
        KeyF => 0x46,
        KeyG => 0x47,
        KeyH => 0x48,
        KeyI => 0x49,
        KeyJ => 0x4A,
        KeyK => 0x4B,
        KeyL => 0x4C,
        KeyM => 0x4D,
        KeyN => 0x4E,
        KeyO => 0x4F,
        KeyP => 0x50,
        KeyQ => 0x51,
        KeyR => 0x52,
        KeyS => 0x53,
        KeyT => 0x54,
        KeyU => 0x55,
        KeyV => 0x56,
        KeyW => 0x57,
        KeyX => 0x58,
        KeyY => 0x59,
        KeyZ => 0x5A,
        Digit0 => 0x30,
        Digit1 => 0x31,
        Digit2 => 0x32,
        Digit3 => 0x33,
        Digit4 => 0x34,
        Digit5 => 0x35,
        Digit6 => 0x36,
        Digit7 => 0x37,
        Digit8 => 0x38,
        Digit9 => 0x39,
        Numpad0 => 0x60,
        Numpad1 => 0x61,
        Numpad2 => 0x62,
        Numpad3 => 0x63,
        Numpad4 => 0x64,
        Numpad5 => 0x65,
        Numpad6 => 0x66,
        Numpad7 => 0x67,
        Numpad8 => 0x68,
        Numpad9 => 0x69,
        NumpadMultiply => 0x6A,
        NumpadAdd => 0x6B,
        NumpadSubtract => 0x6D,
        NumpadDecimal => 0x6E,
        NumpadDivide => 0x6F,
        NumpadEnter | Enter => 0x0D,
        F1 => 0x70,
        F2 => 0x71,
        F3 => 0x72,
        F4 => 0x73,
        F5 => 0x74,
        F6 => 0x75,
        F7 => 0x76,
        F8 => 0x77,
        F9 => 0x78,
        F10 => 0x79,
        F11 => 0x7A,
        F12 => 0x7B,
        Backspace => 0x08,
        Tab => 0x09,
        Escape => 0x1B,
        Space => 0x20,
        PageUp => 0x21,
        PageDown => 0x22,
        End => 0x23,
        Home => 0x24,
        ArrowLeft => 0x25,
        ArrowUp => 0x26,
        ArrowRight => 0x27,
        ArrowDown => 0x28,
        Insert => 0x2D,
        Delete => 0x2E,
        CapsLock => 0x14,
        ShiftLeft | ShiftRight => 0x10,
        ControlLeft | ControlRight => 0x11,
        AltLeft | AltRight => 0x12,
        SuperLeft => 0x5B,
        SuperRight => 0x5C,
        Semicolon => 0xBA,
        Equal => 0xBB,
        Comma => 0xBC,
        Minus => 0xBD,
        Period => 0xBE,
        Slash => 0xBF,
        Backquote => 0xC0,
        BracketLeft => 0xDB,
        Backslash => 0xDC,
        BracketRight => 0xDD,
        Quote => 0xDE,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_digits_are_their_ascii_vk() {
        assert_eq!(vk_for_code(KeyCode::KeyA), Some(b'A' as u32));
        assert_eq!(vk_for_code(KeyCode::KeyZ), Some(b'Z' as u32));
        assert_eq!(vk_for_code(KeyCode::Digit0), Some(b'0' as u32));
        assert_eq!(vk_for_code(KeyCode::Digit9), Some(b'9' as u32));
    }

    #[test]
    fn both_sides_of_a_modifier_send_the_generic_vk() {
        assert_eq!(vk_for_code(KeyCode::ShiftLeft), vk_for_code(KeyCode::ShiftRight));
        assert_eq!(vk_for_code(KeyCode::ControlLeft), Some(crate::vk::VK_CONTROL));
        assert_eq!(vk_for_code(KeyCode::AltRight), Some(crate::vk::VK_MENU));
        assert_eq!(vk_for_code(KeyCode::SuperLeft), Some(crate::vk::VK_LWIN));
    }

    #[test]
    fn navigation_keys_match_the_shared_vk_table() {
        assert_eq!(vk_for_code(KeyCode::Enter), Some(crate::vk::VK_RETURN));
        assert_eq!(vk_for_code(KeyCode::Backspace), Some(crate::vk::VK_BACK));
        assert_eq!(vk_for_code(KeyCode::ArrowLeft), Some(crate::vk::VK_LEFT));
        assert_eq!(vk_for_code(KeyCode::Delete), Some(crate::vk::VK_DELETE));
    }

    #[test]
    fn unmapped_keys_are_none() {
        assert_eq!(vk_for_code(KeyCode::MediaPlayPause), None);
    }

    #[test]
    fn disconnect_requires_the_exact_chord() {
        for modifiers in 0..16 {
            assert_eq!(
                disconnect_chord(
                    KeyCode::KeyD,
                    modifiers & 1 != 0,
                    modifiers & 2 != 0,
                    modifiers & 4 != 0,
                    modifiers & 8 != 0
                ),
                modifiers == 7
            );
        }
        assert!(!disconnect_chord(KeyCode::KeyC, true, true, true, false));
    }

    #[test]
    fn scroll_accumulates_fractions_and_keeps_the_remainder() {
        let mut acc = ScrollAccumulator::default();
        assert_eq!(acc.add_pixels(0.0, 30.0), (0, 0));
        assert_eq!(acc.add_pixels(0.0, 30.0), (0, 1));
        assert_eq!(acc.add_pixels(0.0, -40.0), (0, 0));
        assert_eq!(acc.add_lines(2.0, -1.0), (2, -1));
    }
}
