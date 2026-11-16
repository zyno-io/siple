//! Keyboard: the shortcut table (`[ui.shortcuts]` over fixed defaults, read from the live
//! config every frame) and DTMF / dial-field typing.
//!
//! A shortcut runs the same decision function as its button (`stage::*_action`,
//! `rail::click_line`), so the transfer-session rules live in one place. Esc is not bound here:
//! it closes popovers and the transfer composer and never ends a call.

use std::collections::BTreeMap;

use eframe::egui::{self, Event, Key, Modifiers};
use siple_core::config::shortcuts::{Chord, Shortcut};
use siple_sip::CallState;

use super::{Cx, LayoutMode, UiAction, UiState, rail, session_for, stage};

/// Characters typed as DTMF digits (or into the dial field).
const DIAL_CHARS: &str = "0123456789*#";

/// The egui key of a chord (`None` for a key egui has no name for).
pub(crate) fn egui_key(c: &Chord) -> Option<Key> {
    Key::from_name(if c.key == "Plus" { "+" } else { &c.key })
}

/// The modifiers a chord asks for. `Mod` is egui's `command` (Cmd on macOS, Ctrl elsewhere).
pub(crate) fn egui_modifiers(c: &Chord) -> Modifiers {
    Modifiers {
        alt: c.alt,
        ctrl: c.ctrl,
        shift: c.shift,
        mac_cmd: c.cmd,
        command: c.modifier,
    }
}

/// Takes the key press of `c` out of the input. Key repeats do not count.
///
/// Shift and Alt must match exactly, except that Shift is ignored on punctuation keys unless
/// the chord names it (`/` is Shift+7 on DE / FR / ES / IT layouts). Digit and bracket chords
/// also match the physical key: `Mod+]` arrives as `Plus` on DE / ES / IT and AZERTY reports
/// the symbol on the digit row. A consumed press never reaches egui's own zoom shortcut.
fn consume(ctx: &egui::Context, c: &Chord) -> bool {
    let Some(key) = egui_key(c) else {
        return false;
    };
    let want = egui_modifiers(c);
    let by_position = c.key_is_digit() || c.key == "[" || c.key == "]";
    let loose_shift = c.key_is_punctuation() && !want.shift;
    ctx.input_mut(|i| {
        let hit = i.events.iter().position(|e| {
            matches!(e, Event::Key { key: k, physical_key, pressed: true, repeat: false, modifiers: m }
                if (*k == key || (by_position && *physical_key == Some(key)))
                    && (m.shift == want.shift || (loose_shift && m.shift))
                    && m.alt == want.alt
                    && m.cmd_ctrl_matches(want))
        });
        hit.map(|n| i.events.remove(n)).is_some()
    })
}

/// Whether `s` was pressed this frame (and takes the press). A chord that is plain typing
/// (`/`) is ignored while a text field has focus.
pub(crate) fn take(ctx: &egui::Context, overrides: &BTreeMap<String, String>, s: Shortcut) -> bool {
    let chord = s.effective(overrides);
    if chord.is_bare() && ctx.text_edit_focused() {
        return false;
    }
    consume(ctx, &chord)
}

/// The line DTMF goes to: the selected line when the stage shows it and it is Active.
fn dtmf_target<'a>(cx: &Cx<'a>, st: &UiState) -> Option<&'a siple_sip::CallInfo> {
    let stage_shown = !(cx.mode == LayoutMode::Narrow && cx.ext_open(st));
    cx.selected(st)
        .filter(|c| stage_shown && c.state == CallState::Active)
}

/// Opens (if collapsed) and focuses the dial field.
fn show_dial(cx: &Cx, st: &mut UiState) {
    // With no line the idle stage already shows the field; elsewhere "+ New call" opens it.
    if cx.selected(st).is_some() || (cx.mode == LayoutMode::Narrow && cx.ext_open(st)) {
        rail::open_new_call(cx, st);
    }
    st.focus_dial = true;
}

/// Selects the previous / next line in slot order (wraps; selection only).
fn step_line(cx: &Cx, st: &mut UiState, delta: isize) {
    let lines = cx.lines(st);
    if lines.is_empty() {
        return;
    }
    let at = lines
        .iter()
        .position(|(_, c)| st.selected_line.as_deref() == Some(c.id.as_str()));
    let n = lines.len() as isize;
    let next = match at {
        Some(i) => (i as isize + delta).rem_euclid(n),
        None if delta > 0 => 0,
        None => n - 1,
    };
    st.selected_line = Some(lines[next as usize].1.id.clone());
    st.new_call_open = false;
    rail::leave_extensions_tab(cx, st);
}

/// Runs one shortcut.
fn run(ctx: &egui::Context, cx: &Cx, st: &mut UiState, s: Shortcut, out: &mut Vec<UiAction>) {
    let selected = cx.selected(st);
    let session = selected.and_then(|c| session_for(cx, st, &c.id));
    match s {
        Shortcut::Answer => {
            if let Some(c) = stage::answer_target(cx, st) {
                let current = stage::current_line(cx, selected);
                out.push(stage::answer_action(c, current));
            }
        }
        Shortcut::Hangup => out.extend(selected.and_then(|c| stage::end_action(session, c))),
        Shortcut::Mute => out.extend(selected.and_then(stage::mute_action)),
        Shortcut::Hold => out.extend(selected.and_then(|c| stage::hold_action(session, c))),
        Shortcut::Transfer => {
            if let Some(c) = selected.filter(|c| stage::can_transfer(session, c)) {
                stage::open_composer(st, c);
                // The composer's field takes the keyboard; Enter must not reach the dial field.
                ctx.memory_mut(|m| {
                    if let Some(id) = m.focused() {
                        m.surrender_focus(id);
                    }
                });
            }
        }
        Shortcut::Line(n) => {
            let line = cx
                .lines(st)
                .into_iter()
                .find(|(slot, _)| *slot == usize::from(n))
                .map(|(_, c)| c);
            if let Some(c) = line {
                rail::click_line(cx, st, c, out);
            }
        }
        Shortcut::LinePrev => step_line(cx, st, -1),
        Shortcut::LineNext => step_line(cx, st, 1),
        Shortcut::FocusDial => show_dial(cx, st),
        // Handled where they live: the extensions filter and the Settings router.
        Shortcut::FilterExtensions | Shortcut::Settings => {}
    }
}

/// Takes this frame's shortcuts and typed digits. Runs before the panels draw.
pub(super) fn handle(ui: &egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    let ctx = ui.ctx();
    let overrides = &cx.vm.snap.ui.shortcuts;
    let mut fired = false;
    for s in Shortcut::all() {
        if matches!(s, Shortcut::FilterExtensions | Shortcut::Settings) {
            continue;
        }
        if take(ctx, overrides, s) {
            run(ctx, cx, st, s, out);
            fired = true;
        }
    }
    // Typed digits: DTMF while the stage shows an Active line, else the dial field. A focused
    // text field takes its own text. After a consumed shortcut the frame's text belongs to
    // that key press. egui-winit sends no text for Ctrl / Cmd combinations (AltGr and Option
    // layouts still type `#`), and auto-repeats of a held key are dropped.
    if fired || ctx.text_edit_focused() {
        return;
    }
    let typed: Vec<char> = ctx.input_mut(|i| {
        let mut chars = Vec::new();
        let mut repeating = false;
        i.events.retain(|e| match e {
            Event::Key {
                pressed: true,
                repeat,
                ..
            } => {
                repeating = *repeat;
                true
            }
            Event::Text(t) if t.chars().all(|c| DIAL_CHARS.contains(c)) => {
                if !std::mem::take(&mut repeating) {
                    chars.extend(t.chars());
                }
                false
            }
            _ => true,
        });
        chars
    });
    for ch in typed {
        match dtmf_target(cx, st) {
            Some(c) => out.push(UiAction::Dtmf(c.id.clone(), ch)),
            None => {
                st.dial_target.push(ch);
                show_dial(cx, st);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_default_chord_maps_to_an_egui_key() {
        for s in Shortcut::all() {
            let c = Chord::parse(s.default_chord()).unwrap();
            assert!(egui_key(&c).is_some(), "{}", s.name());
        }
    }

    #[test]
    fn every_chord_key_maps_to_an_egui_key() {
        for k in [
            "A",
            "Z",
            "0",
            "9",
            ",",
            ".",
            "/",
            ";",
            "'",
            "[",
            "]",
            "\\",
            "-",
            "=",
            "`",
            "Plus",
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
            "F1",
            "F12",
        ] {
            let c = Chord::parse(k).unwrap();
            assert!(egui_key(&c).is_some(), "{k}");
        }
    }

    #[test]
    fn mod_is_egui_command_on_every_platform() {
        let m = egui_modifiers(&Chord::parse("Mod+Shift+A").unwrap());
        assert!(m.command && m.shift && !m.ctrl && !m.mac_cmd);
        // macOS reports Cmd as `mac_cmd` + `command`; Windows / Linux Ctrl as `ctrl` + `command`.
        let mac = Modifiers::MAC_CMD | Modifiers::COMMAND;
        let other = Modifiers::CTRL | Modifiers::COMMAND;
        assert!(mac.cmd_ctrl_matches(m) && other.cmd_ctrl_matches(m));
        // A bare Ctrl press on macOS (no `command`) is not Mod.
        assert!(!Modifiers::CTRL.cmd_ctrl_matches(m));
    }
}
