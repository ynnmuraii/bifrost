//! The hotkey grammar, its canonical rendering, and the Linux VT rule.

use config::{Hotkey, HotkeyError, Modifier};

fn parse(text: &str) -> Hotkey {
    Hotkey::parse(text).expect("the hotkey must parse")
}

#[test]
fn order_and_case_do_not_matter_and_output_is_canonical() {
    let reference = parse("Ctrl+Alt+F1");
    assert_eq!(reference.to_string(), "Ctrl+Alt+F1");
    assert_eq!(reference.modifier_count(), 2);
    assert_eq!(reference.key(), "F1");

    for spelling in [
        "alt+ctrl+F1",
        "ALT+CTRL+F1",
        "Control+Alt+F1",
        "  Ctrl + Alt + F1  ",
    ] {
        assert_eq!(
            parse(spelling),
            reference,
            "`{spelling}` must equal the reference"
        );
    }

    // Modifiers are sorted canonically, so the rendering is stable.
    let mixed = parse("Super+Shift+Ctrl+Alt+F1");
    assert_eq!(mixed.to_string(), "Ctrl+Alt+Shift+Super+F1");
    assert_eq!(
        mixed.modifiers(),
        [
            Modifier::Ctrl,
            Modifier::Alt,
            Modifier::Shift,
            Modifier::Super
        ]
    );
    assert!(mixed.has(Modifier::Super));
    assert!(mixed.has(Modifier::Ctrl) && mixed.has(Modifier::Alt) && mixed.has(Modifier::Shift));
    assert_eq!(mixed.key(), "F1");
}

#[test]
fn modifier_aliases_are_accepted() {
    for alias in ["Super", "Meta", "Win", "Cmd", "meta"] {
        let hotkey = parse(&format!("Ctrl+{alias}+F1"));
        assert!(hotkey.has(Modifier::Super), "`{alias}` must mean Super");
        assert_eq!(hotkey.to_string(), "Ctrl+Super+F1");
    }
    assert_eq!(parse("Control+Alt+F5"), parse("Ctrl+Alt+F5"));
}

#[test]
fn the_key_token_is_preserved_verbatim() {
    // The key is what the configuration wrote; only modifiers are normalised.
    assert_eq!(parse("Ctrl+Alt+right").key(), "right");
    assert_eq!(parse("Ctrl+Alt+Right").key(), "Right");
    assert_eq!(parse("Ctrl+Alt+Right").to_string(), "Ctrl+Alt+Right");
}

#[test]
fn malformed_hotkeys_are_reported_by_kind() {
    assert_eq!(Hotkey::parse(""), Err(HotkeyError::Empty));
    assert!(matches!(Hotkey::parse("Ctrl++F1"), Err(HotkeyError::EmptyPart(t)) if t == "Ctrl++F1"));
    assert!(matches!(
        Hotkey::parse("Ctrl+Alt+"),
        Err(HotkeyError::EmptyPart(_))
    ));
    assert!(matches!(
        Hotkey::parse("Ctrl+Ctrl+F1"),
        Err(HotkeyError::DuplicateModifier(t)) if t == "Ctrl"
    ));
    assert!(matches!(
        Hotkey::parse("Ctrl+Alt+ctrl+F1"),
        Err(HotkeyError::DuplicateModifier(_))
    ));
    assert!(matches!(
        Hotkey::parse("Ctrl+F1+Alt"),
        Err(HotkeyError::ModifierAfterKey { after, token }) if after == "F1" && token == "Alt"
    ));
    assert!(matches!(
        Hotkey::parse("Ctrl+Alt"),
        Err(HotkeyError::MissingKey(_))
    ));
}

#[test]
fn the_linux_virtual_terminal_rule_is_a_property_of_the_parsed_hotkey() {
    // Ctrl+Alt+F1..F12 without Shift switches VT on Linux; the rule is checked
    // through this predicate so it can be verified on any host.
    for plain in ["Ctrl+Alt+F1", "Ctrl+Alt+F12", "Alt+Ctrl+F5"] {
        assert!(parse(plain).is_linux_vt_switch(), "`{plain}` switches VT");
    }
    for safe in [
        "Ctrl+Alt+Shift+F1", // Shift makes it safe
        "Ctrl+Alt+F13",      // outside F1..F12
        "Ctrl+Alt+F0",
        "Ctrl+Alt+FB",
        "Ctrl+Shift+F1",     // no Alt
        "Alt+Shift+F1",      // no Ctrl
        "Ctrl+Alt+Super+F1", // super changes the combination
        "Ctrl+Alt+Right",    // not a function key
    ] {
        assert!(
            !parse(safe).is_linux_vt_switch(),
            "`{safe}` must not be the VT switch"
        );
    }
}

#[test]
fn a_hotkey_renders_back_to_a_parseable_string() {
    for text in ["Ctrl+Alt+Shift+F12", "Ctrl+Alt+Right", "Super+Shift+F1"] {
        let hotkey = parse(text);
        assert_eq!(
            parse(&hotkey.to_string()),
            hotkey,
            "`{text}` must round trip"
        );
    }
}
