//! Bindable key names: one table between the names a lesson uses and evdev
//! key codes.
//!
//! Names are physical positions, named after US QWERTY, so they do not
//! depend on the keyboard layout. Only keys the window never acts on are
//! here: the letters except its control keys `h j k q r`, the digit `0`,
//! and the punctuation keys. Digits `1`–`9`, `space`, arrows and every
//! non-printing key (`tab`, `enter`, modifiers, …) are not bindable.

use evdev::KeyCode;

const TABLE: &[(&str, KeyCode)] = &[
    ("a", KeyCode::KEY_A),
    ("b", KeyCode::KEY_B),
    ("c", KeyCode::KEY_C),
    ("d", KeyCode::KEY_D),
    ("e", KeyCode::KEY_E),
    ("f", KeyCode::KEY_F),
    ("g", KeyCode::KEY_G),
    ("i", KeyCode::KEY_I),
    ("l", KeyCode::KEY_L),
    ("m", KeyCode::KEY_M),
    ("n", KeyCode::KEY_N),
    ("o", KeyCode::KEY_O),
    ("p", KeyCode::KEY_P),
    ("s", KeyCode::KEY_S),
    ("t", KeyCode::KEY_T),
    ("u", KeyCode::KEY_U),
    ("v", KeyCode::KEY_V),
    ("w", KeyCode::KEY_W),
    ("x", KeyCode::KEY_X),
    ("y", KeyCode::KEY_Y),
    ("z", KeyCode::KEY_Z),
    ("0", KeyCode::KEY_0),
    ("comma", KeyCode::KEY_COMMA),
    ("period", KeyCode::KEY_DOT),
    ("slash", KeyCode::KEY_SLASH),
    ("semicolon", KeyCode::KEY_SEMICOLON),
    ("apostrophe", KeyCode::KEY_APOSTROPHE),
    ("lbracket", KeyCode::KEY_LEFTBRACE),
    ("rbracket", KeyCode::KEY_RIGHTBRACE),
    ("backslash", KeyCode::KEY_BACKSLASH),
    ("minus", KeyCode::KEY_MINUS),
    ("equal", KeyCode::KEY_EQUAL),
    ("grave", KeyCode::KEY_GRAVE),
];

/// The evdev key code of a bindable key name.
pub fn code(name: &str) -> Option<u16> {
    TABLE
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, k)| k.code())
}

/// Every bindable key name, in table order.
pub fn names() -> impl Iterator<Item = &'static str> {
    TABLE.iter().map(|(n, _)| *n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn names_and_codes_are_one_to_one() {
        let names: HashSet<&str> = names().collect();
        let codes: HashSet<u16> = TABLE.iter().map(|(_, k)| k.code()).collect();
        assert_eq!(names.len(), TABLE.len());
        assert_eq!(codes.len(), TABLE.len());
        for (n, k) in TABLE {
            assert_eq!(code(n), Some(k.code()));
            let back: Vec<&str> = TABLE
                .iter()
                .filter(|(_, c)| c.code() == k.code())
                .map(|(m, _)| *m)
                .collect();
            assert_eq!(back, [*n]);
        }
        assert_eq!(code("f"), Some(33));
        assert_eq!(code("F"), None);
    }

    #[test]
    fn control_and_non_printing_keys_have_no_name() {
        let bindable: HashSet<u16> = TABLE.iter().map(|(_, k)| k.code()).collect();
        let forbidden = [
            KeyCode::KEY_SPACE,
            KeyCode::KEY_H,
            KeyCode::KEY_J,
            KeyCode::KEY_K,
            KeyCode::KEY_Q,
            KeyCode::KEY_R,
            KeyCode::KEY_1,
            KeyCode::KEY_2,
            KeyCode::KEY_3,
            KeyCode::KEY_4,
            KeyCode::KEY_5,
            KeyCode::KEY_6,
            KeyCode::KEY_7,
            KeyCode::KEY_8,
            KeyCode::KEY_9,
            KeyCode::KEY_UP,
            KeyCode::KEY_DOWN,
            KeyCode::KEY_LEFT,
            KeyCode::KEY_RIGHT,
            KeyCode::KEY_TAB,
            KeyCode::KEY_ENTER,
            KeyCode::KEY_KPENTER,
            KeyCode::KEY_BACKSPACE,
            KeyCode::KEY_ESC,
            KeyCode::KEY_CAPSLOCK,
            KeyCode::KEY_LEFTSHIFT,
            KeyCode::KEY_RIGHTSHIFT,
            KeyCode::KEY_LEFTCTRL,
            KeyCode::KEY_RIGHTCTRL,
            KeyCode::KEY_LEFTALT,
            KeyCode::KEY_RIGHTALT,
            KeyCode::KEY_LEFTMETA,
            KeyCode::KEY_RIGHTMETA,
        ];
        for k in forbidden {
            assert!(!bindable.contains(&k.code()), "{k:?} is bindable");
        }
        for n in [
            "h", "j", "k", "q", "r", "1", "9", "space", "tab", "enter", "escape",
        ] {
            assert_eq!(code(n), None, "{n} is bindable");
        }
    }
}
