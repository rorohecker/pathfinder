//! Runtime keyboard shortcuts.
//!
//! Defaults match the historic Slint `KeyBinding`s. User overrides live in
//! `shortcuts.json` as `command -> "Ctrl+Shift+N"` chords. The window
//! FocusScope asks [`resolve`] whether a key event should run a command.

use std::collections::HashMap;

/// Built-in chords. First match wins when two commands share a chord.
pub const DEFAULT_SHORTCUTS: &[(&str, &str)] = &[
    ("new-tab", "Ctrl+T"),
    ("close-tab", "Ctrl+W"),
    ("focus-address", "Ctrl+L"),
    ("focus-search", "Ctrl+Shift+F"),
    ("command-palette", "Ctrl+P"),
    ("settings", "Ctrl+,"),
    ("view-grid", "Ctrl+1"),
    ("view-compact", "Ctrl+Shift+1"),
    ("view-list", "Ctrl+2"),
    ("view-gallery", "Ctrl+3"),
    ("toggle-preview", "Ctrl+I"),
    ("copy", "Ctrl+C"),
    ("cut", "Ctrl+X"),
    ("paste", "Ctrl+V"),
    ("select-all", "Ctrl+A"),
    ("undo", "Ctrl+Z"),
    ("redo", "Ctrl+Y"),
    ("new-window", "Ctrl+N"),
    ("new-folder", "Ctrl+Shift+N"),
    ("go-back", "Alt+Left"),
    ("go-forward", "Alt+Right"),
    ("go-up", "Alt+Up"),
    ("toggle-dual", "F3"),
    ("refresh", "F5"),
    ("delete", "Delete"),
    ("purge", "Shift+Delete"),
    ("properties", "Alt+Return"),
    ("rename", "F2"),
    ("duplicates", "Ctrl+D"),
    ("checksum", "Ctrl+Shift+C"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: String,
}

impl Chord {
    pub fn parse(raw: &str) -> Option<Self> {
        let mut ctrl = false;
        let mut shift = false;
        let mut alt = false;
        let mut key: Option<String> = None;
        for part in raw.split('+') {
            let p = part.trim();
            if p.is_empty() {
                continue;
            }
            match p.to_ascii_lowercase().as_str() {
                "ctrl" | "control" | "cmd" | "meta" => ctrl = true,
                "shift" => shift = true,
                "alt" | "option" => alt = true,
                other => {
                    if key.is_some() {
                        return None;
                    }
                    key = Some(normalize_key_name(other));
                }
            }
        }
        let key = key.filter(|k| !k.is_empty())?;
        Some(Self {
            ctrl,
            shift,
            alt,
            key,
        })
    }

    pub fn from_parts(ctrl: bool, shift: bool, alt: bool, key: &str) -> Option<Self> {
        let key = normalize_key_name(key);
        if key.is_empty()
            || matches!(
                key.as_str(),
                "Control" | "Shift" | "Alt" | "Meta" | "Escape"
            )
        {
            return None;
        }
        Some(Self {
            ctrl,
            shift,
            alt,
            key,
        })
    }

    pub fn display(&self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("Ctrl".to_string());
        }
        if self.alt {
            parts.push("Alt".to_string());
        }
        if self.shift && !shift_implied(&self.key) {
            parts.push("Shift".to_string());
        }
        parts.push(display_key(&self.key));
        parts.join("+")
    }

    pub fn matches(&self, ctrl: bool, shift: bool, alt: bool, key: &str) -> bool {
        self.ctrl == ctrl
            && self.alt == alt
            && self.shift == shift
            && self.key.eq_ignore_ascii_case(&normalize_key_name(key))
    }
}

fn shift_implied(key: &str) -> bool {
    matches!(key, "," | "." | ";" | "/" | "\\" | "'" | "[" | "]")
}

/// Map Slint `event.text` (control chars / PUA) and typed names onto one key id.
pub fn canonical_key(raw: &str) -> String {
    normalize_key_name(raw)
}

fn normalize_key_name(raw: &str) -> String {
    if let Some(c) = raw.chars().next() {
        if raw.chars().count() == 1 {
            match c {
                '\u{0008}' => return "Backspace".into(),
                '\u{0009}' => return "Tab".into(),
                '\u{000a}' | '\u{000d}' => return "Return".into(),
                '\u{001b}' => return "Escape".into(),
                '\u{007f}' => return "Delete".into(),
                '\u{0010}' | '\u{0015}' => return "Shift".into(),
                '\u{0011}' | '\u{0016}' => return "Control".into(),
                '\u{0012}' | '\u{0013}' => return "Alt".into(),
                '\u{0017}' | '\u{0018}' => return "Meta".into(),
                '\u{0020}' => return "Space".into(),
                '\u{F700}' => return "Up".into(),
                '\u{F701}' => return "Down".into(),
                '\u{F702}' => return "Left".into(),
                '\u{F703}' => return "Right".into(),
                c if ('\u{F704}'..='\u{F717}').contains(&c) => {
                    let n = (c as u32) - 0xF703;
                    return format!("F{n}");
                }
                _ => {}
            }
        }
    }
    let t = raw.trim();
    match t.to_ascii_lowercase().as_str() {
        "esc" | "escape" => "Escape".into(),
        "enter" | "return" => "Return".into(),
        "del" | "delete" => "Delete".into(),
        "bksp" | "backspace" => "Backspace".into(),
        "space" | " " => "Space".into(),
        "left" | "leftarrow" | "arrowleft" => "Left".into(),
        "right" | "rightarrow" | "arrowright" => "Right".into(),
        "up" | "uparrow" | "arrowup" => "Up".into(),
        "down" | "downarrow" | "arrowdown" => "Down".into(),
        "comma" => ",".into(),
        "period" | "dot" => ".".into(),
        other if other.len() == 1 => other.to_ascii_uppercase(),
        other => {
            let mut s = other.to_string();
            if let Some(c) = s.get_mut(..1) {
                c.make_ascii_uppercase();
            }
            s
        }
    }
}

fn display_key(key: &str) -> String {
    match key {
        "Left" => "Left".into(),
        "Right" => "Right".into(),
        "Up" => "Up".into(),
        "Down" => "Down".into(),
        "," => ",".into(),
        other => other.to_string(),
    }
}

pub fn hint_for(command: &str, overrides: &HashMap<String, String>, default_hint: &str) -> String {
    if let Some(raw) = overrides.get(command) {
        if let Some(chord) = Chord::parse(raw) {
            return chord.display();
        }
        if !raw.trim().is_empty() {
            return raw.trim().to_string();
        }
    }
    if let Some((_, chord)) = DEFAULT_SHORTCUTS.iter().find(|(c, _)| *c == command) {
        return (*chord).to_string();
    }
    default_hint.to_string()
}

/// Resolve a physical key event to a command id.
///
/// User overrides win when they claim the chord. Defaults fill the rest.
/// Commands whose default chord was overridden no longer fire on the old key.
pub fn resolve(
    ctrl: bool,
    shift: bool,
    alt: bool,
    key: &str,
    overrides: &HashMap<String, String>,
) -> Option<String> {
    let key = normalize_key_name(key);
    if key.is_empty() || key == "Escape" {
        return None;
    }

    let mut entries: Vec<(&String, &String)> = overrides.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let mut overridden_commands: Vec<&str> = Vec::new();
    for (command, raw) in entries {
        let Some(chord) = Chord::parse(raw) else {
            continue;
        };
        overridden_commands.push(command.as_str());
        if chord.matches(ctrl, shift, alt, &key) {
            return Some((*command).clone());
        }
    }

    for (command, raw) in DEFAULT_SHORTCUTS {
        if overridden_commands.contains(command) {
            continue;
        }
        let Some(chord) = Chord::parse(raw) else {
            continue;
        };
        if chord.matches(ctrl, shift, alt, &key) {
            return Some((*command).to_string());
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
pub fn overlay_is_blocking(
    confirm: bool,
    prompt: bool,
    settings: bool,
    welcome: bool,
    mode_prompt: bool,
    command_palette: bool,
    tool_overlay: bool,
    compare: bool,
    image_tools: bool,
    dupe: bool,
) -> bool {
    confirm
        || prompt
        || settings
        || welcome
        || mode_prompt
        || command_palette
        || tool_overlay
        || compare
        || image_tools
        || dupe
}

/// Destructive or navigation-altering commands that must not run under a modal.
pub fn blocked_while_modal(command: &str) -> bool {
    matches!(
        command,
        "delete"
            | "purge"
            | "empty-trash"
            | "paste"
            | "cut"
            | "rename"
            | "new-folder"
            | "new-file"
            | "new-window"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_standard_chords() {
        let c = Chord::parse("Ctrl+Shift+N").unwrap();
        assert!(c.ctrl && c.shift && !c.alt);
        assert_eq!(c.key, "N");
        assert_eq!(c.display(), "Ctrl+Shift+N");
    }

    #[test]
    fn resolve_default_copy() {
        let map = HashMap::new();
        assert_eq!(
            resolve(true, false, false, "c", &map).as_deref(),
            Some("copy")
        );
        assert_eq!(
            resolve(true, true, false, "n", &map).as_deref(),
            Some("new-folder")
        );
        assert_eq!(
            resolve(false, true, false, "Delete", &map).as_deref(),
            Some("purge")
        );
    }

    #[test]
    fn override_replaces_default_chord() {
        let mut map = HashMap::new();
        map.insert("copy".into(), "Ctrl+E".into());
        assert_eq!(
            resolve(true, false, false, "e", &map).as_deref(),
            Some("copy")
        );
        assert_eq!(resolve(true, false, false, "c", &map), None);
    }

    #[test]
    fn from_parts_skips_modifiers() {
        assert!(Chord::from_parts(true, false, false, "c").is_some());
        assert!(Chord::from_parts(true, false, false, "Control").is_none());
        assert!(Chord::from_parts(false, false, false, "").is_none());
    }

    #[test]
    fn canonical_key_maps_slint_control_chars() {
        assert_eq!(canonical_key("\u{001b}"), "Escape");
        assert_eq!(canonical_key("\u{0008}"), "Backspace");
        assert_eq!(canonical_key("\u{007f}"), "Delete");
        assert_eq!(canonical_key("\u{0011}"), "Control");
        assert_eq!(canonical_key("\u{F705}"), "F2");
        assert_eq!(canonical_key("\u{F702}"), "Left");
        assert_eq!(canonical_key("\u{F700}"), "Up");
        assert_eq!(canonical_key("\n"), "Return");
    }

    #[test]
    fn resolve_alt_chords_from_slint_pua() {
        let map = HashMap::new();
        assert_eq!(
            resolve(false, false, true, "\u{F700}", &map).as_deref(),
            Some("go-up")
        );
        assert_eq!(
            resolve(false, false, true, "\n", &map).as_deref(),
            Some("properties")
        );
    }

    #[test]
    fn hint_prefers_override() {
        let mut map = HashMap::new();
        map.insert("copy".into(), "Ctrl+E".into());
        assert_eq!(hint_for("copy", &map, "Ctrl+C"), "Ctrl+E");
        assert_eq!(hint_for("cut", &map, "Ctrl+X"), "Ctrl+X");
    }

    #[test]
    fn modal_blocks_delete() {
        assert!(blocked_while_modal("delete"));
        assert!(!blocked_while_modal("copy"));
        assert!(overlay_is_blocking(
            false, false, false, false, false, false, true, false, false, false
        ));
    }
}
