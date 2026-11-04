//! Translates RFB pointer and key events into the screencast's CDP input events.

use crate::api::service_api::{
    ScreencastKeyEventType, ScreencastKeyInput, ScreencastMouseButton, ScreencastMouseEventType,
    ScreencastMouseInput,
};
use std::collections::HashSet;
use std::time::{Duration, Instant};

const RFB_LEFT: u8 = 1;
const RFB_MIDDLE: u8 = 2;
const RFB_RIGHT: u8 = 4;
/// Wheel up, down, left, right.
const RFB_WHEEL: [(u8, f64, f64); 4] = [(8, 0.0, -100.0), (16, 0.0, 100.0), (32, -100.0, 0.0), (64, 100.0, 0.0)];
const MULTI_CLICK_TIME: Duration = Duration::from_millis(500);
const MULTI_CLICK_DISTANCE: f64 = 4.0;

const MODIFIER_ALT: u32 = 1;
const MODIFIER_CTRL: u32 = 2;
const MODIFIER_META: u32 = 4;
const MODIFIER_SHIFT: u32 = 8;

#[derive(Default)]
pub struct InputMapper {
    buttons: u8,
    last_press: Option<Press>,
    held_keys: HashSet<String>,
    modifiers: u32,
}

#[derive(Clone, Copy)]
struct Press {
    button: u8,
    at: Instant,
    x: f64,
    y: f64,
    count: u32,
}

impl InputMapper {
    /// Mouse events for an RFB pointer event at page frame coordinates `(x, y)`.
    pub fn pointer(&mut self, mask: u8, x: f64, y: f64, now: Instant) -> Vec<ScreencastMouseInput> {
        let previous = self.buttons;
        let buttons = mask & (RFB_LEFT | RFB_MIDDLE | RFB_RIGHT);
        let mut events = Vec::new();
        if buttons == previous {
            events.push(self.mouse(ScreencastMouseEventType::MouseMoved, x, y, ScreencastMouseButton::None, 0));
        }
        for (bit, button) in [(RFB_LEFT, ScreencastMouseButton::Left), (RFB_MIDDLE, ScreencastMouseButton::Middle), (RFB_RIGHT, ScreencastMouseButton::Right)] {
            if previous & bit != 0 && buttons & bit == 0 {
                self.buttons &= !bit;
                let count = self.last_press.filter(|press| press.button == bit).map_or(1, |press| press.count);
                events.push(self.mouse(ScreencastMouseEventType::MouseReleased, x, y, button, count));
            } else if previous & bit == 0 && buttons & bit != 0 {
                self.buttons |= bit;
                let count = match self.last_press {
                    Some(press) if press.button == bit
                        && now.duration_since(press.at) <= MULTI_CLICK_TIME
                        && (press.x - x).abs() <= MULTI_CLICK_DISTANCE
                        && (press.y - y).abs() <= MULTI_CLICK_DISTANCE => press.count % 3 + 1,
                    _ => 1,
                };
                self.last_press = Some(Press { button: bit, at: now, x, y, count });
                events.push(self.mouse(ScreencastMouseEventType::MousePressed, x, y, button, count));
            }
        }
        // Wheel "buttons" are pressed and released per notch; only the press scrolls.
        for (bit, delta_x, delta_y) in RFB_WHEEL {
            if mask & bit != 0 {
                let mut wheel = self.mouse(ScreencastMouseEventType::MouseWheel, x, y, ScreencastMouseButton::None, 0);
                wheel.delta_x = delta_x;
                wheel.delta_y = delta_y;
                events.push(wheel);
            }
        }
        events
    }

    /// The key event for an RFB key event; `scancode` comes from QEMU extended key events.
    pub fn key(&mut self, down: bool, keysym: u32, scancode: Option<u32>) -> ScreencastKeyInput {
        let key = key_name(keysym);
        let code = scancode.and_then(code_for_scancode).or_else(|| code_for_keysym(keysym)).unwrap_or("").to_owned();
        let identity = if code.is_empty() { format!("keysym:{keysym}") } else { code.clone() };
        let auto_repeat = down && !self.held_keys.insert(identity.clone());
        if !down {
            self.held_keys.remove(&identity);
        }
        let modifier = modifier_for_key(&key);
        if down {
            self.modifiers |= modifier;
        } else if modifier != 0 && !self.held_keys.iter().any(|held| modifier_for_code(held) == modifier) {
            self.modifiers &= !modifier;
        }
        let text = (down && self.modifiers & (MODIFIER_CTRL | MODIFIER_META) == 0)
            .then(|| text_for_key(&key))
            .flatten();
        ScreencastKeyInput {
            event_type: match (down, text.is_some()) {
                (false, _) => ScreencastKeyEventType::KeyUp,
                (true, true) => ScreencastKeyEventType::KeyDown,
                (true, false) => ScreencastKeyEventType::RawKeyDown,
            },
            windows_virtual_key_code: virtual_key_code(&code, &key),
            key,
            code,
            text,
            auto_repeat,
            modifiers: self.modifiers,
        }
    }

    fn mouse(&self, event_type: ScreencastMouseEventType, x: f64, y: f64, button: ScreencastMouseButton, click_count: u32) -> ScreencastMouseInput {
        ScreencastMouseInput {
            event_type,
            x,
            y,
            button,
            buttons: cdp_buttons(self.buttons),
            click_count,
            delta_x: 0.0,
            delta_y: 0.0,
            modifiers: self.modifiers,
        }
    }
}

/// CDP's pressed-button bit field: left 1, right 2, middle 4.
fn cdp_buttons(rfb: u8) -> u32 {
    u32::from(rfb & RFB_LEFT != 0) | (u32::from(rfb & RFB_RIGHT != 0) << 1) | (u32::from(rfb & RFB_MIDDLE != 0) << 2)
}

fn modifier_for_key(key: &str) -> u32 {
    match key {
        "Alt" | "AltGraph" => MODIFIER_ALT,
        "Control" => MODIFIER_CTRL,
        "Meta" => MODIFIER_META,
        "Shift" => MODIFIER_SHIFT,
        _ => 0,
    }
}

fn modifier_for_code(code: &str) -> u32 {
    match code {
        "AltLeft" | "AltRight" => MODIFIER_ALT,
        "ControlLeft" | "ControlRight" => MODIFIER_CTRL,
        "MetaLeft" | "MetaRight" => MODIFIER_META,
        "ShiftLeft" | "ShiftRight" => MODIFIER_SHIFT,
        _ => 0,
    }
}

fn text_for_key(key: &str) -> Option<String> {
    if key == "Enter" {
        return Some("\r".into());
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if !c.is_control() => Some(key.to_owned()),
        _ => None,
    }
}

/// The DOM `key` for an X11 keysym.
fn key_name(keysym: u32) -> String {
    let named = match keysym {
        0xff08 => "Backspace",
        0xff09 | 0xfe20 => "Tab",
        0xff0d | 0xff8d => "Enter",
        0xff13 => "Pause",
        0xff14 => "ScrollLock",
        0xff1b => "Escape",
        0xffff | 0xff9f => "Delete",
        0xff50 | 0xff95 => "Home",
        0xff51 | 0xff96 => "ArrowLeft",
        0xff52 | 0xff97 => "ArrowUp",
        0xff53 | 0xff98 => "ArrowRight",
        0xff54 | 0xff99 => "ArrowDown",
        0xff55 | 0xff9a => "PageUp",
        0xff56 | 0xff9b => "PageDown",
        0xff57 | 0xff9c => "End",
        0xff61 => "PrintScreen",
        0xff63 | 0xff9e => "Insert",
        0xff67 => "ContextMenu",
        0xff7f => "NumLock",
        0xffe1 | 0xffe2 => "Shift",
        0xffe3 | 0xffe4 => "Control",
        0xffe5 => "CapsLock",
        0xffe7 | 0xffe8 | 0xffeb | 0xffec => "Meta",
        0xffe9 | 0xffea => "Alt",
        0xfe03 => "AltGraph",
        0xffbe..=0xffd5 => return format!("F{}", keysym - 0xffbe + 1),
        0xffaa => "*",
        0xffab => "+",
        0xffad => "-",
        0xffae => ".",
        0xffaf => "/",
        0xffb0..=0xffb9 => return char::from_digit(keysym - 0xffb0, 10).unwrap().to_string(),
        _ => "",
    };
    if !named.is_empty() {
        return named.to_owned();
    }
    let unicode = match keysym {
        0x20..=0x7e | 0xa0..=0xff => Some(keysym),
        0x0100_0100..=0x0110_ffff => Some(keysym - 0x0100_0000),
        _ => None,
    };
    unicode.and_then(char::from_u32).map_or_else(|| "Unidentified".to_owned(), |c| c.to_string())
}

/// The DOM `code` for an RFB/QEMU XT scancode (extended keys as `0x80 | code`).
fn code_for_scancode(scancode: u32) -> Option<&'static str> {
    const LETTERS_Q: [&str; 10] = ["KeyQ", "KeyW", "KeyE", "KeyR", "KeyT", "KeyY", "KeyU", "KeyI", "KeyO", "KeyP"];
    const LETTERS_A: [&str; 9] = ["KeyA", "KeyS", "KeyD", "KeyF", "KeyG", "KeyH", "KeyJ", "KeyK", "KeyL"];
    const LETTERS_Z: [&str; 7] = ["KeyZ", "KeyX", "KeyC", "KeyV", "KeyB", "KeyN", "KeyM"];
    const DIGITS: [&str; 10] = ["Digit1", "Digit2", "Digit3", "Digit4", "Digit5", "Digit6", "Digit7", "Digit8", "Digit9", "Digit0"];
    const FUNCTION: [&str; 10] = ["F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10"];
    Some(match scancode {
        0x01 => "Escape",
        0x02..=0x0b => DIGITS[(scancode - 0x02) as usize],
        0x0c => "Minus",
        0x0d => "Equal",
        0x0e => "Backspace",
        0x0f => "Tab",
        0x10..=0x19 => LETTERS_Q[(scancode - 0x10) as usize],
        0x1a => "BracketLeft",
        0x1b => "BracketRight",
        0x1c => "Enter",
        0x1d => "ControlLeft",
        0x1e..=0x26 => LETTERS_A[(scancode - 0x1e) as usize],
        0x27 => "Semicolon",
        0x28 => "Quote",
        0x29 => "Backquote",
        0x2a => "ShiftLeft",
        0x2b => "Backslash",
        0x2c..=0x32 => LETTERS_Z[(scancode - 0x2c) as usize],
        0x33 => "Comma",
        0x34 => "Period",
        0x35 => "Slash",
        0x36 => "ShiftRight",
        0x37 => "NumpadMultiply",
        0x38 => "AltLeft",
        0x39 => "Space",
        0x3a => "CapsLock",
        0x3b..=0x44 => FUNCTION[(scancode - 0x3b) as usize],
        0x45 => "NumLock",
        0x46 => "ScrollLock",
        0x47 => "Numpad7",
        0x48 => "Numpad8",
        0x49 => "Numpad9",
        0x4a => "NumpadSubtract",
        0x4b => "Numpad4",
        0x4c => "Numpad5",
        0x4d => "Numpad6",
        0x4e => "NumpadAdd",
        0x4f => "Numpad1",
        0x50 => "Numpad2",
        0x51 => "Numpad3",
        0x52 => "Numpad0",
        0x53 => "NumpadDecimal",
        0x56 => "IntlBackslash",
        0x57 => "F11",
        0x58 => "F12",
        0x9c => "NumpadEnter",
        0x9d => "ControlRight",
        0xb5 => "NumpadDivide",
        0xb7 => "PrintScreen",
        0xb8 => "AltRight",
        0xc5 => "Pause",
        0xc7 => "Home",
        0xc8 => "ArrowUp",
        0xc9 => "PageUp",
        0xcb => "ArrowLeft",
        0xcd => "ArrowRight",
        0xcf => "End",
        0xd0 => "ArrowDown",
        0xd1 => "PageDown",
        0xd2 => "Insert",
        0xd3 => "Delete",
        0xdb => "MetaLeft",
        0xdc => "MetaRight",
        0xdd => "ContextMenu",
        _ => return None,
    })
}

/// A best-effort DOM `code` when the client sent no scancode (US layout).
fn code_for_keysym(keysym: u32) -> Option<&'static str> {
    const LETTERS: [&str; 26] = [
        "KeyA", "KeyB", "KeyC", "KeyD", "KeyE", "KeyF", "KeyG", "KeyH", "KeyI", "KeyJ", "KeyK", "KeyL", "KeyM",
        "KeyN", "KeyO", "KeyP", "KeyQ", "KeyR", "KeyS", "KeyT", "KeyU", "KeyV", "KeyW", "KeyX", "KeyY", "KeyZ",
    ];
    const DIGITS: [&str; 10] = ["Digit0", "Digit1", "Digit2", "Digit3", "Digit4", "Digit5", "Digit6", "Digit7", "Digit8", "Digit9"];
    Some(match keysym {
        0x61..=0x7a => LETTERS[(keysym - 0x61) as usize],
        0x41..=0x5a => LETTERS[(keysym - 0x41) as usize],
        0x30..=0x39 => DIGITS[(keysym - 0x30) as usize],
        0x20 => "Space",
        0xff08 => "Backspace",
        0xff09 | 0xfe20 => "Tab",
        0xff0d => "Enter",
        0xff1b => "Escape",
        0xffff => "Delete",
        0xff50 => "Home",
        0xff51 => "ArrowLeft",
        0xff52 => "ArrowUp",
        0xff53 => "ArrowRight",
        0xff54 => "ArrowDown",
        0xff55 => "PageUp",
        0xff56 => "PageDown",
        0xff57 => "End",
        0xff63 => "Insert",
        0xffe1 => "ShiftLeft",
        0xffe2 => "ShiftRight",
        0xffe3 => "ControlLeft",
        0xffe4 => "ControlRight",
        0xffe9 => "AltLeft",
        0xffea | 0xfe03 => "AltRight",
        0xffe7 | 0xffeb => "MetaLeft",
        0xffe8 | 0xffec => "MetaRight",
        _ => return None,
    })
}

/// Windows virtual-key code for a DOM `code` (falling back to the key for letters and digits).
fn virtual_key_code(code: &str, key: &str) -> Option<u32> {
    let by_code = match code {
        "Backspace" => 8,
        "Tab" => 9,
        "Enter" | "NumpadEnter" => 13,
        "ShiftLeft" | "ShiftRight" => 16,
        "ControlLeft" | "ControlRight" => 17,
        "AltLeft" | "AltRight" => 18,
        "Pause" => 19,
        "CapsLock" => 20,
        "Escape" => 27,
        "Space" => 32,
        "PageUp" => 33,
        "PageDown" => 34,
        "End" => 35,
        "Home" => 36,
        "ArrowLeft" => 37,
        "ArrowUp" => 38,
        "ArrowRight" => 39,
        "ArrowDown" => 40,
        "PrintScreen" => 44,
        "Insert" => 45,
        "Delete" => 46,
        "MetaLeft" => 91,
        "MetaRight" => 92,
        "ContextMenu" => 93,
        "NumpadMultiply" => 106,
        "NumpadAdd" => 107,
        "NumpadSubtract" => 109,
        "NumpadDecimal" => 110,
        "NumpadDivide" => 111,
        "NumLock" => 144,
        "ScrollLock" => 145,
        "Semicolon" => 186,
        "Equal" => 187,
        "Comma" => 188,
        "Minus" => 189,
        "Period" => 190,
        "Slash" => 191,
        "Backquote" => 192,
        "BracketLeft" => 219,
        "Backslash" => 220,
        "BracketRight" => 221,
        "Quote" => 222,
        "IntlBackslash" => 226,
        _ => 0,
    };
    if by_code != 0 {
        return Some(by_code);
    }
    if let Some(letter) = code.strip_prefix("Key") {
        return letter.chars().next().map(|c| c as u32);
    }
    if let Some(digit) = code.strip_prefix("Digit").or_else(|| code.strip_prefix("Numpad").filter(|rest| rest.len() == 1)) {
        let value = digit.chars().next()?.to_digit(10)?;
        return Some(if code.starts_with("Numpad") { 96 + value } else { 48 + value });
    }
    if let Some(number) = code.strip_prefix('F').and_then(|rest| rest.parse::<u32>().ok()) {
        return Some(111 + number);
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphanumeric() => Some(c.to_ascii_uppercase() as u32),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_buttons_moves_double_clicks_and_wheel_notches() {
        let mut mapper = InputMapper::default();
        let start = Instant::now();
        let types = |events: &[ScreencastMouseInput]| events.iter().map(|event| (event.event_type, event.button, event.buttons, event.click_count)).collect::<Vec<_>>();
        assert_eq!(types(&mapper.pointer(0, 5.0, 5.0, start)), vec![(ScreencastMouseEventType::MouseMoved, ScreencastMouseButton::None, 0, 0)]);
        assert_eq!(types(&mapper.pointer(1, 5.0, 5.0, start)), vec![(ScreencastMouseEventType::MousePressed, ScreencastMouseButton::Left, 1, 1)]);
        assert_eq!(types(&mapper.pointer(1, 9.0, 5.0, start)), vec![(ScreencastMouseEventType::MouseMoved, ScreencastMouseButton::None, 1, 0)]);
        assert_eq!(types(&mapper.pointer(0, 9.0, 5.0, start)), vec![(ScreencastMouseEventType::MouseReleased, ScreencastMouseButton::Left, 0, 1)]);
        let soon = start + Duration::from_millis(200);
        assert_eq!(types(&mapper.pointer(1, 6.0, 5.0, soon)), vec![(ScreencastMouseEventType::MousePressed, ScreencastMouseButton::Left, 1, 2)]);
        assert_eq!(types(&mapper.pointer(0, 6.0, 5.0, soon)), vec![(ScreencastMouseEventType::MouseReleased, ScreencastMouseButton::Left, 0, 2)]);
        assert_eq!(types(&mapper.pointer(4, 6.0, 5.0, soon)), vec![(ScreencastMouseEventType::MousePressed, ScreencastMouseButton::Right, 2, 1)]);
        mapper.pointer(0, 6.0, 5.0, soon);

        let wheel = mapper.pointer(16, 6.0, 5.0, soon);
        assert_eq!(wheel.len(), 2, "a move, then the notch");
        assert_eq!((wheel[1].event_type, wheel[1].delta_y), (ScreencastMouseEventType::MouseWheel, 100.0));
        assert_eq!(mapper.pointer(0, 6.0, 5.0, soon).len(), 1, "the notch release only moves");
    }

    #[test]
    fn maps_keys_with_scancodes_text_modifiers_and_repeats() {
        let mut mapper = InputMapper::default();
        let a = mapper.key(true, 0x61, Some(0x1e));
        assert_eq!((a.event_type, a.key.as_str(), a.code.as_str(), a.text.as_deref(), a.windows_virtual_key_code), (ScreencastKeyEventType::KeyDown, "a", "KeyA", Some("a"), Some(65)));
        assert!(mapper.key(true, 0x61, Some(0x1e)).auto_repeat);
        assert_eq!(mapper.key(false, 0x61, Some(0x1e)).event_type, ScreencastKeyEventType::KeyUp);

        let control = mapper.key(true, 0xffe3, Some(0x1d));
        assert_eq!((control.key.as_str(), control.code.as_str(), control.modifiers), ("Control", "ControlLeft", MODIFIER_CTRL));
        let ctrl_c = mapper.key(true, 0x63, Some(0x2e));
        assert_eq!((ctrl_c.event_type, ctrl_c.text, ctrl_c.modifiers), (ScreencastKeyEventType::RawKeyDown, None, MODIFIER_CTRL));
        mapper.key(false, 0x63, Some(0x2e));
        assert_eq!(mapper.key(false, 0xffe3, Some(0x1d)).modifiers, 0);

        let enter = mapper.key(true, 0xff0d, None);
        assert_eq!((enter.key.as_str(), enter.code.as_str(), enter.text.as_deref(), enter.windows_virtual_key_code), ("Enter", "Enter", Some("\r"), Some(13)));
        let up = mapper.key(true, 0xff52, Some(0xc8));
        assert_eq!((up.event_type, up.key.as_str(), up.code.as_str(), up.windows_virtual_key_code), (ScreencastKeyEventType::RawKeyDown, "ArrowUp", "ArrowUp", Some(38)));
        let euro = mapper.key(true, 0x0100_20ac, None);
        assert_eq!((euro.key.as_str(), euro.text.as_deref()), ("€", Some("€")));
        assert_eq!(mapper.key(true, 0xffc9, Some(0x58)).key, "F12");
    }
}
