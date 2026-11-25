//! The Edit menu commands (Cut, Copy, Paste, Select All, Undo, Redo) and how they reach egui.
//!
//! On macOS the Edit menu items are plain menu items with accelerators, not the predefined
//! responder-chain ones: those are disabled (nothing in winit's view implements `copy:` and the
//! like) yet AppKit's `performKeyEquivalent:` still swallows the shortcut, so egui never saw
//! Cmd+C. A menu choice comes back as an [`EditCmd`] through the global menu handler. The
//! right-click menu of text fields uses the same commands.

use eframe::egui;

/// One command of the Edit menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // menu ids: the macOS menu bar only
pub enum EditCmd {
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
}

/// How a command is delivered to egui.
#[derive(Debug, Clone, PartialEq)]
pub enum EditDelivery {
    /// A viewport command: egui-winit answers with `Event::Cut` / `Copy` / `Paste(clipboard)`.
    Viewport(egui::ViewportCommand),
    /// Key presses fed to the next frame's input.
    Keys(Vec<egui::Event>),
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // menu ids: the macOS menu bar only
impl EditCmd {
    pub const ALL: [EditCmd; 6] = [
        Self::Undo,
        Self::Redo,
        Self::Cut,
        Self::Copy,
        Self::Paste,
        Self::SelectAll,
    ];

    /// Stable menu item id.
    pub fn to_id(self) -> &'static str {
        match self {
            Self::Undo => "edit:undo",
            Self::Redo => "edit:redo",
            Self::Cut => "edit:cut",
            Self::Copy => "edit:copy",
            Self::Paste => "edit:paste",
            Self::SelectAll => "edit:select_all",
        }
    }

    /// Inverse of [`Self::to_id`]; `None` for foreign ids.
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.to_id() == id)
    }

    /// Menu title.
    pub fn title(self) -> &'static str {
        match self {
            Self::Undo => "Undo",
            Self::Redo => "Redo",
            Self::Cut => "Cut",
            Self::Copy => "Copy",
            Self::Paste => "Paste",
            Self::SelectAll => "Select All",
        }
    }

    /// What to hand egui for this command.
    pub fn delivery(self) -> EditDelivery {
        let key = |key, modifiers| {
            EditDelivery::Keys(vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            }])
        };
        match self {
            Self::Cut => EditDelivery::Viewport(egui::ViewportCommand::RequestCut),
            Self::Copy => EditDelivery::Viewport(egui::ViewportCommand::RequestCopy),
            Self::Paste => EditDelivery::Viewport(egui::ViewportCommand::RequestPaste),
            Self::SelectAll => key(egui::Key::A, egui::Modifiers::COMMAND),
            Self::Undo => key(egui::Key::Z, egui::Modifiers::COMMAND),
            Self::Redo => key(
                egui::Key::Z,
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
            ),
        }
    }

    /// Delivers the command: viewport commands go out now, key events are queued in `pending`
    /// for [`eframe::App::raw_input_hook`], and a repaint is requested either way.
    pub fn deliver(self, ctx: &egui::Context, pending: &mut Vec<egui::Event>) {
        match self.delivery() {
            EditDelivery::Viewport(c) => ctx.send_viewport_cmd(c),
            EditDelivery::Keys(mut ev) => pending.append(&mut ev),
        }
        ctx.request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_foreign_ids_are_refused() {
        for c in EditCmd::ALL {
            assert_eq!(EditCmd::from_id(c.to_id()), Some(c));
        }
        assert_eq!(EditCmd::from_id("settings"), None);
        assert_eq!(EditCmd::from_id("edit:nope"), None);
    }

    #[test]
    fn the_tray_command_parser_routes_edit_ids() {
        use super::super::tray_model::TrayCmd;
        for c in EditCmd::ALL {
            assert_eq!(TrayCmd::from_id(c.to_id()), Some(TrayCmd::Edit(c)));
        }
    }

    #[test]
    fn clipboard_commands_become_viewport_requests() {
        use egui::ViewportCommand as V;
        assert_eq!(
            EditCmd::Cut.delivery(),
            EditDelivery::Viewport(V::RequestCut)
        );
        assert_eq!(
            EditCmd::Copy.delivery(),
            EditDelivery::Viewport(V::RequestCopy)
        );
        assert_eq!(
            EditCmd::Paste.delivery(),
            EditDelivery::Viewport(V::RequestPaste)
        );
    }

    #[test]
    fn select_all_undo_redo_become_command_key_presses() {
        let one = |c: EditCmd| match c.delivery() {
            EditDelivery::Keys(mut v) if v.len() == 1 => v.remove(0),
            d => panic!("{d:?}"),
        };
        for (cmd, k, m) in [
            (EditCmd::SelectAll, egui::Key::A, egui::Modifiers::COMMAND),
            (EditCmd::Undo, egui::Key::Z, egui::Modifiers::COMMAND),
            (
                EditCmd::Redo,
                egui::Key::Z,
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
            ),
        ] {
            match one(cmd) {
                egui::Event::Key {
                    key,
                    pressed,
                    repeat,
                    modifiers,
                    ..
                } => {
                    assert_eq!((key, pressed, repeat), (k, true, false), "{cmd:?}");
                    assert_eq!(modifiers, m, "{cmd:?}");
                }
                e => panic!("{e:?}"),
            }
        }
    }

    #[test]
    fn deliver_queues_keys_only() {
        let ctx = egui::Context::default();
        let mut q = Vec::new();
        EditCmd::Copy.deliver(&ctx, &mut q);
        assert!(q.is_empty());
        EditCmd::SelectAll.deliver(&ctx, &mut q);
        EditCmd::Redo.deliver(&ctx, &mut q);
        assert_eq!(q.len(), 2);
    }
}
