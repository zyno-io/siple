//! Keyboard shortcuts: the one list of action names, their default chords and the chord syntax.
//!
//! `[ui.shortcuts]` maps an action name to a chord string. Validation, the GUI's key handling
//! and the Settings page all read this module, so the names cannot drift apart.
//!
//! Chord syntax: `+`-separated modifiers then one key, case-insensitive, e.g. `Mod+Shift+A`,
//! `Mod+,`, `/`, `Alt+F4`. Modifiers: `Mod` (Cmd on macOS, Ctrl elsewhere), `Ctrl`, `Alt`
//! (Option), `Shift`, `Cmd` (the macOS Command key itself). Keys: a letter, a digit, one of
//! ``, . / ; ' [ ] \ - = ` ``, `Plus`, or a name (`Enter`, `Tab`, `Space`, `Escape`, `Backspace`,
//! `Delete`, `Insert`, `Home`, `End`, `PageUp`, `PageDown`, `Up`, `Down`, `Left`, `Right`,
//! `F1`-`F12`).

use std::collections::BTreeMap;
use std::fmt;

/// A shortcut action. The config names are [`Shortcut::name`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Shortcut {
    /// Answer the selected ringing call, else the topmost ringing one.
    Answer,
    /// Hang up / cancel / decline the selected line.
    Hangup,
    /// Mute toggle.
    Mute,
    /// Hold / resume.
    Hold,
    /// Open the transfer composer.
    Transfer,
    /// Select line slot 1-9 (a held line becomes the active line).
    Line(u8),
    /// Select the previous line (selection only).
    LinePrev,
    /// Select the next line (selection only).
    LineNext,
    /// Focus the dial field.
    FocusDial,
    /// Focus the extensions filter.
    FilterExtensions,
    /// Open / close Settings.
    Settings,
}

const LINE_NAMES: [&str; 9] = [
    "line_1", "line_2", "line_3", "line_4", "line_5", "line_6", "line_7", "line_8", "line_9",
];
const LINE_DEFAULTS: [&str; 9] = [
    "Mod+1", "Mod+2", "Mod+3", "Mod+4", "Mod+5", "Mod+6", "Mod+7", "Mod+8", "Mod+9",
];

impl Shortcut {
    /// Every action, in the order the Settings page lists them.
    pub fn all() -> Vec<Shortcut> {
        let mut v = vec![
            Shortcut::Answer,
            Shortcut::Hangup,
            Shortcut::Mute,
            Shortcut::Hold,
            Shortcut::Transfer,
        ];
        v.extend((1..=9).map(Shortcut::Line));
        v.extend([
            Shortcut::LinePrev,
            Shortcut::LineNext,
            Shortcut::FocusDial,
            Shortcut::FilterExtensions,
            Shortcut::Settings,
        ]);
        v
    }

    /// The `[ui.shortcuts]` key.
    pub fn name(self) -> &'static str {
        match self {
            Shortcut::Answer => "answer",
            Shortcut::Hangup => "hangup",
            Shortcut::Mute => "mute",
            Shortcut::Hold => "hold",
            Shortcut::Transfer => "transfer",
            Shortcut::Line(n) => LINE_NAMES[usize::from(n.clamp(1, 9)) - 1],
            Shortcut::LinePrev => "line_prev",
            Shortcut::LineNext => "line_next",
            Shortcut::FocusDial => "focus_dial",
            Shortcut::FilterExtensions => "filter_extensions",
            Shortcut::Settings => "settings",
        }
    }

    pub fn from_name(name: &str) -> Option<Shortcut> {
        Self::all().into_iter().find(|s| s.name() == name)
    }

    /// The fixed default chord.
    pub fn default_chord(self) -> &'static str {
        match self {
            Shortcut::Answer => "Mod+Shift+A",
            Shortcut::Hangup => "Mod+Shift+E",
            Shortcut::Mute => "Mod+Shift+M",
            Shortcut::Hold => "Mod+Shift+H",
            Shortcut::Transfer => "Mod+Shift+T",
            Shortcut::Line(n) => LINE_DEFAULTS[usize::from(n.clamp(1, 9)) - 1],
            Shortcut::LinePrev => "Mod+[",
            Shortcut::LineNext => "Mod+]",
            Shortcut::FocusDial => "Mod+L",
            Shortcut::FilterExtensions => "/",
            Shortcut::Settings => "Mod+,",
        }
    }

    /// What the action does, for the Settings page.
    pub fn label(self) -> String {
        match self {
            Shortcut::Answer => "Answer selected / topmost ringing".into(),
            Shortcut::Hangup => "Hang up / cancel / decline".into(),
            Shortcut::Mute => "Mute toggle".into(),
            Shortcut::Hold => "Hold / resume".into(),
            Shortcut::Transfer => "Transfer".into(),
            Shortcut::Line(n) => format!("Select line {n}"),
            Shortcut::LinePrev => "Previous line".into(),
            Shortcut::LineNext => "Next line".into(),
            Shortcut::FocusDial => "Focus the dial field".into(),
            Shortcut::FilterExtensions => "Filter extensions".into(),
            Shortcut::Settings => "Settings".into(),
        }
    }

    /// The chord in effect: the `[ui.shortcuts]` override when it parses and can be bound, else
    /// the default.
    pub fn effective(self, overrides: &BTreeMap<String, String>) -> Chord {
        overrides
            .get(self.name())
            .and_then(|s| Chord::parse(s).ok())
            .filter(|c| c.bindable().is_ok())
            .unwrap_or_else(|| Chord::parse(self.default_chord()).expect("default chord parses"))
    }

    /// Whether `[ui.shortcuts]` sets a usable chord for this action.
    pub fn is_overridden(self, overrides: &BTreeMap<String, String>) -> bool {
        overrides
            .get(self.name())
            .and_then(|s| Chord::parse(s).ok())
            .is_some_and(|c| c.bindable().is_ok())
    }
}

/// Pairs of actions whose effective chords are the same key press on this platform
/// (`mac`: `Cmd+X` is `Mod+X`; elsewhere `Ctrl+X` is). Only pairs with at least one override
/// are reported; the fixed defaults do not collide with each other.
pub fn conflicts(overrides: &BTreeMap<String, String>, mac: bool) -> Vec<(Shortcut, Shortcut)> {
    let all = Shortcut::all();
    let mut out = Vec::new();
    for (i, a) in all.iter().enumerate() {
        for b in &all[i + 1..] {
            if (a.is_overridden(overrides) || b.is_overridden(overrides))
                && a.effective(overrides).normalised(mac) == b.effective(overrides).normalised(mac)
            {
                out.push((*a, *b));
            }
        }
    }
    out
}

/// Overrides that parse but can never fire on this platform (`Cmd` off macOS).
pub fn inert(overrides: &BTreeMap<String, String>, mac: bool) -> Vec<Shortcut> {
    Shortcut::all()
        .into_iter()
        .filter(|s| s.is_overridden(overrides) && s.effective(overrides).inert_on(mac))
        .collect()
}

/// A parsed key chord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    /// `Mod`: Cmd on macOS, Ctrl elsewhere.
    pub modifier: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// The macOS Command key itself.
    pub cmd: bool,
    /// Canonical key: `A`, `1`, `[`, `Enter`, `Up`, `F5`, `Plus`, ...
    pub key: String,
}

const NAMED_KEYS: &[&str] = &[
    "Enter",
    "Tab",
    "Space",
    "Escape",
    "Backspace",
    "Delete",
    "Insert",
    "Home",
    "End",
    "PageUp",
    "PageDown",
    "Up",
    "Down",
    "Left",
    "Right",
    "Plus",
];

fn canonical_key(k: &str) -> Option<String> {
    let mut chars = k.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return if c.is_ascii_alphabetic() {
            Some(c.to_ascii_uppercase().to_string())
        } else if c.is_ascii_digit() || ",./;'[]\\-=`".contains(c) {
            Some(c.to_string())
        } else {
            None
        };
    }
    if let Some(digits) = k.strip_prefix(['F', 'f'])
        && let Ok(n) = digits.parse::<u8>()
        && (1..=12).contains(&n)
        && n.to_string() == digits
    {
        return Some(format!("F{n}"));
    }
    NAMED_KEYS
        .iter()
        .find(|n| n.eq_ignore_ascii_case(k))
        .map(|n| (*n).to_string())
}

impl Chord {
    /// Whether the key is punctuation, where Shift is only a layout detail (`/` is Shift+7 on
    /// several layouts).
    pub fn key_is_punctuation(&self) -> bool {
        self.key == "Plus"
            || (self.key.chars().count() == 1
                && !self.key.chars().all(|c| c.is_ascii_alphanumeric()))
    }

    /// Whether the key is a digit.
    pub fn key_is_digit(&self) -> bool {
        self.key.chars().all(|c| c.is_ascii_digit())
    }

    /// Whether the chord has no Mod / Ctrl / Cmd / Alt (Shift alone does not count).
    pub fn is_bare(&self) -> bool {
        !(self.modifier || self.ctrl || self.cmd || self.alt)
    }

    /// Chords that cannot be bound: keys with a fixed meaning elsewhere in the GUI.
    pub fn bindable(&self) -> Result<(), String> {
        if self.key == "Escape" {
            return Err(
                "Escape is reserved: it only closes popovers and the transfer composer".into(),
            );
        }
        if self.key_is_digit() && self.is_bare() {
            return Err(
                "a digit without Mod, Ctrl, Cmd or Alt is typed as DTMF or into the dial field"
                    .into(),
            );
        }
        if matches!(self.key.as_str(), "C" | "V" | "X") && (self.modifier || self.ctrl || self.cmd)
        {
            return Err(
                "Mod+C, Mod+V and Mod+X are the clipboard keys and never reach shortcuts".into(),
            );
        }
        Ok(())
    }

    /// The chord can never fire on this platform: `Cmd` is the macOS Command key only.
    pub fn inert_on(&self, mac: bool) -> bool {
        self.cmd && !mac
    }

    /// Platform-normalised form for comparing chords: `(command, ctrl, cmd, alt, shift, key)`
    /// where `command` merges `Mod` with the platform's own command key.
    pub fn normalised(&self, mac: bool) -> (bool, bool, bool, bool, bool, String) {
        let (command, ctrl, cmd) = if mac {
            (self.modifier || self.cmd, self.ctrl, false)
        } else {
            (self.modifier || self.ctrl, false, self.cmd)
        };
        (command, ctrl, cmd, self.alt, self.shift, self.key.clone())
    }

    /// Parses `Mod+Shift+A`. The error says what is wrong.
    pub fn parse(s: &str) -> Result<Chord, String> {
        let parts: Vec<&str> = s.split('+').map(str::trim).collect();
        let (key, mods) = parts.split_last().expect("split yields one part");
        let mut c = Chord {
            modifier: false,
            ctrl: false,
            alt: false,
            shift: false,
            cmd: false,
            key: String::new(),
        };
        for m in mods {
            let slot = match m.to_ascii_lowercase().as_str() {
                "mod" => &mut c.modifier,
                "ctrl" | "control" => &mut c.ctrl,
                "alt" | "option" => &mut c.alt,
                "shift" => &mut c.shift,
                "cmd" | "command" | "meta" | "super" => &mut c.cmd,
                _ => return Err(format!("unknown modifier {}", crate::echo(m))),
            };
            if *slot {
                return Err(format!("modifier {} is repeated", crate::echo(m)));
            }
            *slot = true;
        }
        c.key = canonical_key(key).ok_or_else(|| {
            if key.is_empty() {
                "missing key".to_string()
            } else {
                format!("unknown key {}", crate::echo(key))
            }
        })?;
        Ok(c)
    }
}

impl fmt::Display for Chord {
    /// Canonical text: `Mod+Shift+A`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (on, name) in [
            (self.modifier, "Mod"),
            (self.ctrl, "Ctrl"),
            (self.cmd, "Cmd"),
            (self.alt, "Alt"),
            (self.shift, "Shift"),
        ] {
            if on {
                write!(f, "{name}+")?;
            }
        }
        f.write_str(&self.key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_round_trip() {
        let all = Shortcut::all();
        assert_eq!(all.len(), 19);
        let mut names: Vec<_> = all.iter().map(|s| s.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), all.len());
        for s in all {
            assert_eq!(Shortcut::from_name(s.name()), Some(s));
            Chord::parse(s.default_chord()).unwrap();
        }
        assert_eq!(Shortcut::from_name("nope"), None);
    }

    #[test]
    fn chords_parse() {
        let c = Chord::parse("mod+shift+a").unwrap();
        assert!(c.modifier && c.shift && !c.alt);
        assert_eq!(c.to_string(), "Mod+Shift+A");
        assert_eq!(Chord::parse("/").unwrap().to_string(), "/");
        assert_eq!(Chord::parse("Mod+,").unwrap().key, ",");
        assert_eq!(Chord::parse("Mod+-").unwrap().key, "-");
        assert_eq!(Chord::parse("Alt+f4").unwrap().key, "F4");
        assert_eq!(Chord::parse("Ctrl+pageup").unwrap().key, "PageUp");
        for bad in [
            "",
            " ",
            "Mod+",
            "Mod+Shift",
            "Hyper+A",
            "Mod+Mod+A",
            "A+B",
            "Mod+F13",
            "Mod+F0",
            "Mod+F01",
            "é",
        ] {
            assert!(Chord::parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn unbindable_chords_are_rejected_and_fall_back() {
        for bad in [
            "Escape",
            "Mod+Escape",
            "1",
            "Shift+5",
            "Mod+C",
            "Ctrl+v",
            "Cmd+X",
            "Mod+Shift+C",
        ] {
            let c = Chord::parse(bad).unwrap();
            assert!(c.bindable().is_err(), "{bad}");
        }
        for ok in ["Mod+1", "Alt+3", "Mod+A", "F9", "/", "Mod+Shift+T"] {
            assert!(Chord::parse(ok).unwrap().bindable().is_ok(), "{ok}");
        }
        let mut o = BTreeMap::new();
        o.insert("hangup".into(), "Escape".into());
        assert_eq!(Shortcut::Hangup.effective(&o).to_string(), "Mod+Shift+E");
        assert!(!Shortcut::Hangup.is_overridden(&o));
    }

    #[test]
    fn conflicts_are_found_per_platform() {
        let mut o = BTreeMap::new();
        assert!(conflicts(&o, true).is_empty() && conflicts(&o, false).is_empty());
        // Collides with hangup's default.
        o.insert("mute".into(), "Mod+Shift+E".into());
        assert_eq!(conflicts(&o, true), [(Shortcut::Hangup, Shortcut::Mute)]);
        // Ctrl+X is Mod+X off macOS only; Cmd+X is Mod+X on macOS only.
        o.clear();
        o.insert("mute".into(), "Ctrl+Shift+E".into());
        assert_eq!(conflicts(&o, false).len(), 1);
        assert!(conflicts(&o, true).is_empty());
        o.insert("mute".into(), "Cmd+Shift+E".into());
        assert_eq!(conflicts(&o, true).len(), 1);
        assert!(conflicts(&o, false).is_empty());
        assert_eq!(inert(&o, false), [Shortcut::Mute]);
        assert!(inert(&o, true).is_empty());
        // Two overrides on one chord.
        o.clear();
        o.insert("mute".into(), "F9".into());
        o.insert("hold".into(), "f9".into());
        assert_eq!(conflicts(&o, true), [(Shortcut::Mute, Shortcut::Hold)]);
    }

    #[test]
    fn effective_prefers_a_valid_override() {
        let mut o = BTreeMap::new();
        assert_eq!(Shortcut::Mute.effective(&o).to_string(), "Mod+Shift+M");
        o.insert("mute".into(), "Alt+M".into());
        assert_eq!(Shortcut::Mute.effective(&o).to_string(), "Alt+M");
        o.insert("mute".into(), "garbage+".into());
        assert_eq!(Shortcut::Mute.effective(&o).to_string(), "Mod+Shift+M");
    }
}
