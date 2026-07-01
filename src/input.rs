use crate::rdp::{ClientEvent, PointerAction};
use ironrdp_pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};
use ironrdp_pdu::input::mouse::PointerFlags;
use ironrdp_pdu::input::scan_code::KeyboardFlags as SlowScanCodeKeyboardFlags;
use ironrdp_pdu::input::unicode::KeyboardFlags as SlowUnicodeKeyboardFlags;
use ironrdp_pdu::input::{InputEvent, MousePdu, ScanCodePdu, UnicodePdu};

pub fn input_event_from_client(event: &ClientEvent) -> Vec<FastPathInputEvent> {
    match event {
        ClientEvent::Pointer {
            action,
            x,
            y,
            button,
            delta_y,
        } => pointer_event(action, *x, *y, *button, *delta_y),
        ClientEvent::Key { down, code, key } => key_event(*down, code, key.as_deref()),
        ClientEvent::Resize { .. } | ClientEvent::Refresh { .. } => Vec::new(),
    }
}

pub fn slow_input_events_from_client(event: &ClientEvent) -> Vec<InputEvent> {
    input_event_from_client(event)
        .into_iter()
        .filter_map(slow_input_event)
        .collect()
}

fn slow_input_event(event: FastPathInputEvent) -> Option<InputEvent> {
    match event {
        FastPathInputEvent::KeyboardEvent(flags, code) => {
            let mut slow_flags = if flags.contains(KeyboardFlags::RELEASE) {
                SlowScanCodeKeyboardFlags::RELEASE
            } else {
                SlowScanCodeKeyboardFlags::DOWN
            };
            if flags.contains(KeyboardFlags::EXTENDED) {
                slow_flags |= SlowScanCodeKeyboardFlags::EXTENDED;
            }
            Some(InputEvent::ScanCode(ScanCodePdu {
                flags: slow_flags,
                key_code: u16::from(code),
            }))
        }
        FastPathInputEvent::UnicodeKeyboardEvent(flags, unicode_code) => {
            let mut slow_flags = SlowUnicodeKeyboardFlags::empty();
            if flags.contains(KeyboardFlags::RELEASE) {
                slow_flags |= SlowUnicodeKeyboardFlags::RELEASE;
            }
            Some(InputEvent::Unicode(UnicodePdu {
                flags: slow_flags,
                unicode_code,
            }))
        }
        FastPathInputEvent::MouseEvent(event) => Some(InputEvent::Mouse(event)),
        FastPathInputEvent::MouseEventEx(event) => Some(InputEvent::MouseX(event)),
        FastPathInputEvent::MouseEventRel(event) => Some(InputEvent::MouseRel(event)),
        FastPathInputEvent::QoeEvent(_) | FastPathInputEvent::SyncEvent(_) => None,
    }
}

fn pointer_event(
    action: &PointerAction,
    x: u16,
    y: u16,
    button: Option<u8>,
    delta_y: Option<i16>,
) -> Vec<FastPathInputEvent> {
    let mut flags = PointerFlags::MOVE;
    let mut number_of_wheel_rotation_units = 0;

    match action {
        PointerAction::Move => {}
        PointerAction::Down => {
            flags |= PointerFlags::DOWN | button_flag(button);
        }
        PointerAction::Up => {
            flags |= button_flag(button);
        }
        PointerAction::Wheel => {
            flags = PointerFlags::VERTICAL_WHEEL;
            number_of_wheel_rotation_units = delta_y.unwrap_or_default().clamp(-255, 255);
        }
    }

    vec![FastPathInputEvent::MouseEvent(MousePdu {
        flags,
        number_of_wheel_rotation_units,
        x_position: x,
        y_position: y,
    })]
}

fn button_flag(button: Option<u8>) -> PointerFlags {
    match button.unwrap_or(0) {
        0 => PointerFlags::LEFT_BUTTON,
        1 => PointerFlags::MIDDLE_BUTTON_OR_WHEEL,
        2 => PointerFlags::RIGHT_BUTTON,
        _ => PointerFlags::LEFT_BUTTON,
    }
}

fn key_event(down: bool, code: &str, key: Option<&str>) -> Vec<FastPathInputEvent> {
    if let Some(scancode) = scancode_for_code(code) {
        let mut flags = KeyboardFlags::empty();
        if !down {
            flags |= KeyboardFlags::RELEASE;
        }
        if scancode.extended {
            flags |= KeyboardFlags::EXTENDED;
        }
        return vec![FastPathInputEvent::KeyboardEvent(flags, scancode.code)];
    }

    let Some(key) = key else {
        return Vec::new();
    };
    let mut chars = key.chars();
    let Some(ch) = chars.next() else {
        return Vec::new();
    };
    if chars.next().is_some() {
        return Vec::new();
    }

    let mut flags = KeyboardFlags::empty();
    if !down {
        flags |= KeyboardFlags::RELEASE;
    }

    vec![FastPathInputEvent::UnicodeKeyboardEvent(flags, ch as u16)]
}

#[derive(Debug, Clone, Copy)]
struct ScanCode {
    code: u8,
    extended: bool,
}

fn scancode_for_code(code: &str) -> Option<ScanCode> {
    let basic = match code {
        "Escape" => 0x01,
        "Digit1" => 0x02,
        "Digit2" => 0x03,
        "Digit3" => 0x04,
        "Digit4" => 0x05,
        "Digit5" => 0x06,
        "Digit6" => 0x07,
        "Digit7" => 0x08,
        "Digit8" => 0x09,
        "Digit9" => 0x0a,
        "Digit0" => 0x0b,
        "Minus" => 0x0c,
        "Equal" => 0x0d,
        "Backspace" => 0x0e,
        "Tab" => 0x0f,
        "KeyQ" => 0x10,
        "KeyW" => 0x11,
        "KeyE" => 0x12,
        "KeyR" => 0x13,
        "KeyT" => 0x14,
        "KeyY" => 0x15,
        "KeyU" => 0x16,
        "KeyI" => 0x17,
        "KeyO" => 0x18,
        "KeyP" => 0x19,
        "BracketLeft" => 0x1a,
        "BracketRight" => 0x1b,
        "Enter" => 0x1c,
        "ControlLeft" => 0x1d,
        "KeyA" => 0x1e,
        "KeyS" => 0x1f,
        "KeyD" => 0x20,
        "KeyF" => 0x21,
        "KeyG" => 0x22,
        "KeyH" => 0x23,
        "KeyJ" => 0x24,
        "KeyK" => 0x25,
        "KeyL" => 0x26,
        "Semicolon" => 0x27,
        "Quote" => 0x28,
        "Backquote" => 0x29,
        "ShiftLeft" => 0x2a,
        "Backslash" => 0x2b,
        "KeyZ" => 0x2c,
        "KeyX" => 0x2d,
        "KeyC" => 0x2e,
        "KeyV" => 0x2f,
        "KeyB" => 0x30,
        "KeyN" => 0x31,
        "KeyM" => 0x32,
        "Comma" => 0x33,
        "Period" => 0x34,
        "Slash" => 0x35,
        "ShiftRight" => 0x36,
        "AltLeft" => 0x38,
        "Space" => 0x39,
        "CapsLock" => 0x3a,
        "F1" => 0x3b,
        "F2" => 0x3c,
        "F3" => 0x3d,
        "F4" => 0x3e,
        "F5" => 0x3f,
        "F6" => 0x40,
        "F7" => 0x41,
        "F8" => 0x42,
        "F9" => 0x43,
        "F10" => 0x44,
        "F11" => 0x57,
        "F12" => 0x58,
        _ => {
            return match code {
                "ControlRight" => Some(ScanCode {
                    code: 0x1d,
                    extended: true,
                }),
                "AltRight" => Some(ScanCode {
                    code: 0x38,
                    extended: true,
                }),
                "ArrowUp" => Some(ScanCode {
                    code: 0x48,
                    extended: true,
                }),
                "ArrowLeft" => Some(ScanCode {
                    code: 0x4b,
                    extended: true,
                }),
                "ArrowRight" => Some(ScanCode {
                    code: 0x4d,
                    extended: true,
                }),
                "ArrowDown" => Some(ScanCode {
                    code: 0x50,
                    extended: true,
                }),
                "Insert" => Some(ScanCode {
                    code: 0x52,
                    extended: true,
                }),
                "Delete" => Some(ScanCode {
                    code: 0x53,
                    extended: true,
                }),
                "Home" => Some(ScanCode {
                    code: 0x47,
                    extended: true,
                }),
                "End" => Some(ScanCode {
                    code: 0x4f,
                    extended: true,
                }),
                "PageUp" => Some(ScanCode {
                    code: 0x49,
                    extended: true,
                }),
                "PageDown" => Some(ScanCode {
                    code: 0x51,
                    extended: true,
                }),
                "MetaLeft" => Some(ScanCode {
                    code: 0x5b,
                    extended: true,
                }),
                "MetaRight" => Some(ScanCode {
                    code: 0x5c,
                    extended: true,
                }),
                _ => None,
            };
        }
    };

    Some(ScanCode {
        code: basic,
        extended: false,
    })
}

#[cfg(test)]
mod keyboard_tests {
    use super::*;

    fn single_mouse_event(events: Vec<FastPathInputEvent>) -> MousePdu {
        assert_eq!(events.len(), 1);
        match events.into_iter().next().unwrap() {
            FastPathInputEvent::MouseEvent(event) => event,
            other => panic!("expected mouse event, got {other:?}"),
        }
    }

    #[test]
    fn left_button_down_maps_to_rdp_button_press() {
        let event = single_mouse_event(pointer_event(&PointerAction::Down, 10, 20, Some(0), None));

        assert!(event.flags.contains(PointerFlags::MOVE));
        assert!(event.flags.contains(PointerFlags::DOWN));
        assert!(event.flags.contains(PointerFlags::LEFT_BUTTON));
        assert_eq!(event.x_position, 10);
        assert_eq!(event.y_position, 20);
    }

    #[test]
    fn left_button_up_maps_to_rdp_button_release() {
        let event = single_mouse_event(pointer_event(&PointerAction::Up, 10, 20, Some(0), None));

        assert!(event.flags.contains(PointerFlags::MOVE));
        assert!(!event.flags.contains(PointerFlags::DOWN));
        assert!(event.flags.contains(PointerFlags::LEFT_BUTTON));
        assert_eq!(event.x_position, 10);
        assert_eq!(event.y_position, 20);
    }

    #[test]
    fn right_button_maps_to_rdp_right_button() {
        let event = single_mouse_event(pointer_event(&PointerAction::Down, 5, 6, Some(2), None));

        assert!(event.flags.contains(PointerFlags::DOWN));
        assert!(event.flags.contains(PointerFlags::RIGHT_BUTTON));
        assert!(!event.flags.contains(PointerFlags::LEFT_BUTTON));
    }

    #[test]
    fn wheel_delta_is_clamped_to_rdp_range() {
        let event =
            single_mouse_event(pointer_event(&PointerAction::Wheel, 1, 2, None, Some(-511)));

        assert!(event.flags.contains(PointerFlags::VERTICAL_WHEEL));
        assert_eq!(event.number_of_wheel_rotation_units, -255);
        assert_eq!(event.x_position, 1);
        assert_eq!(event.y_position, 2);
    }

    #[test]
    fn left_click_maps_to_slow_path_mouse_input() {
        let event = ClientEvent::Pointer {
            action: PointerAction::Down,
            x: 10,
            y: 20,
            button: Some(0),
            delta_y: None,
        };

        let events = slow_input_events_from_client(&event);
        assert_eq!(events.len(), 1);
        match &events[0] {
            InputEvent::Mouse(event) => {
                assert!(event.flags.contains(PointerFlags::DOWN));
                assert!(event.flags.contains(PointerFlags::LEFT_BUTTON));
                assert_eq!(event.x_position, 10);
                assert_eq!(event.y_position, 20);
            }
            other => panic!("expected slow-path mouse input, got {other:?}"),
        }
    }

    #[test]
    fn key_press_maps_to_slow_path_scancode_down() {
        let event = ClientEvent::Key {
            down: true,
            code: "ArrowLeft".to_owned(),
            key: None,
        };

        let events = slow_input_events_from_client(&event);
        assert_eq!(events.len(), 1);
        match &events[0] {
            InputEvent::ScanCode(event) => {
                assert!(event.flags.contains(SlowScanCodeKeyboardFlags::DOWN));
                assert!(event.flags.contains(SlowScanCodeKeyboardFlags::EXTENDED));
                assert_eq!(event.key_code, 0x4b);
            }
            other => panic!("expected slow-path scancode input, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_arrow_key_to_extended_scancode() {
        let events = key_event(true, "ArrowLeft", None);
        assert_eq!(events.len(), 1);
        match &events[0] {
            FastPathInputEvent::KeyboardEvent(flags, code) => {
                assert!(flags.contains(KeyboardFlags::EXTENDED));
                assert_eq!(*code, 0x4b);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn maps_printable_key_to_unicode_fallback() {
        let events = key_event(true, "Unknown", Some("a"));
        assert_eq!(events.len(), 1);
        match &events[0] {
            FastPathInputEvent::UnicodeKeyboardEvent(flags, ch) => {
                assert!(!flags.contains(KeyboardFlags::RELEASE));
                assert_eq!(*ch, 'a' as u16);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
