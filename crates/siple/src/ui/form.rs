//! Labeled form rows shared by Settings and the first-run wizard. Every control gets its label
//! as accessible name (`labelled_by`), so screen readers and kittest find it by its text.

use eframe::egui::{self, Align, CornerRadius, Layout, Margin, RichText, Stroke};

use super::combo_icon;
use crate::shell::edit_cmd::{EditCmd, EditDelivery};
use crate::theme::{FS_BODY, FS_MICRO, FS_SMALL, Palette, R_LG, R_MD, SP_S, SP_XS, icon as ic};
use crate::widgets::{FIELD_MARGIN, account_switch, icon, mono, strong};

/// Width of the label column.
const LABEL_W: f32 = 150.0;
/// Height of every form control (fields, drop-downs, buttons), so a row of mixed controls lines up.
pub(crate) const CONTROL_H: f32 = 32.0;

fn label_cell(ui: &mut egui::Ui, p: &Palette, text: &str) -> egui::Response {
    ui.allocate_ui_with_layout(
        egui::vec2(LABEL_W, CONTROL_H),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_width(LABEL_W);
            ui.add(egui::Label::new(RichText::new(text).size(FS_SMALL).color(p.muted)).truncate())
        },
    )
    .inner
}

/// A message under a field (a validation error from `config.edit`).
pub(crate) fn error_line(ui: &mut egui::Ui, p: &Palette, msg: &str) {
    ui.horizontal(|ui| {
        ui.add_space(LABEL_W + 6.0);
        ui.add(egui::Label::new(RichText::new(msg).size(FS_SMALL).color(p.err.ink)).wrap());
    });
}

/// A muted hint under a field.
pub(crate) fn hint_line(ui: &mut egui::Ui, p: &Palette, msg: &str) {
    ui.horizontal(|ui| {
        ui.add_space(LABEL_W + 6.0);
        ui.add(egui::Label::new(RichText::new(msg).size(FS_SMALL).color(p.faint)).wrap());
    });
}

/// Single-line text row. `Some(new)` when the user edited it this frame.
pub(crate) fn text_row(
    ui: &mut egui::Ui,
    p: &Palette,
    label: &str,
    value: &str,
    hint: &str,
    password: bool,
    err: Option<&str>,
) -> Option<String> {
    text_row_focus(ui, p, label, value, hint, password, err).0
}

/// Right-click menu of a text field: Cut, Copy, Paste and Select All.
///
/// Call right after adding the `TextEdit` (`text` is its buffer). A right-click would collapse
/// the field's selection and may not focus it, so the selection from before the press is put
/// back and the field is focused; the actions then run on it like the keyboard shortcuts do.
/// Cut, Copy and Paste are the same viewport commands as the macOS Edit menu ([`EditCmd`]):
/// egui-winit answers with `Event::Cut` / `Copy` / `Paste(clipboard)`, which the focused field
/// handles. Select All sets the selection directly. Cut and Copy are off without a selection
/// and for password fields.
pub(crate) fn text_context_menu(ui: &mut egui::Ui, r: &egui::Response, text: &str, password: bool) {
    use egui::text::{CCursor, CCursorRange};
    let id = r.id;
    let ctx = ui.ctx().clone();
    let keep = id.with("sel_before_press");
    let load = || egui::TextEdit::load_state(&ctx, id);
    let selection = || {
        load()
            .and_then(|s| s.cursor.char_range())
            .filter(|c| !c.is_empty())
    };
    let pressed =
        r.hovered() && ui.input(|i| i.pointer.button_pressed(egui::PointerButton::Secondary));
    if pressed {
        if let Some(sel) = ctx.data(|d| d.get_temp::<CCursorRange>(keep)) {
            if let Some(mut st) = load() {
                st.cursor.set_char_range(Some(sel));
                st.store(&ctx, id);
            }
        }
        r.request_focus();
    } else if !r.context_menu_opened() {
        match selection() {
            Some(sel) => ctx.data_mut(|d| d.insert_temp(keep, sel)),
            None => ctx.data_mut(|d| d.remove_temp::<CCursorRange>(keep)),
        }
    }
    let has_sel = selection().is_some();
    let n = text.chars().count();
    r.context_menu(|ui| {
        let mut run = |ui: &mut egui::Ui, cmd: EditCmd| {
            r.request_focus();
            match cmd.delivery() {
                EditDelivery::Viewport(c) => ui.ctx().send_viewport_cmd(c),
                EditDelivery::Keys(_) => {}
            }
            ui.close();
        };
        let clip = !password && has_sel;
        if ui
            .add_enabled(clip, egui::Button::new(EditCmd::Cut.title()))
            .clicked()
        {
            run(ui, EditCmd::Cut);
        }
        if ui
            .add_enabled(clip, egui::Button::new(EditCmd::Copy.title()))
            .clicked()
        {
            run(ui, EditCmd::Copy);
        }
        if ui.button(EditCmd::Paste.title()).clicked() {
            run(ui, EditCmd::Paste);
        }
        if ui
            .add_enabled(n > 0, egui::Button::new(EditCmd::SelectAll.title()))
            .clicked()
        {
            if let Some(mut st) = load().or_else(|| Some(Default::default())) {
                st.cursor
                    .set_char_range(Some(CCursorRange::two(CCursor::new(0), CCursor::new(n))));
                st.store(&ctx, id);
            }
            r.request_focus();
            ui.close();
        }
    });
}

/// [`text_row`] that also says whether the field has keyboard focus.
pub(crate) fn text_row_focus(
    ui: &mut egui::Ui,
    p: &Palette,
    label: &str,
    value: &str,
    hint: &str,
    password: bool,
    err: Option<&str>,
) -> (Option<String>, bool) {
    let mut buf = value.to_string();
    let (changed, focus) = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let l = label_cell(ui, p, label);
            let r = ui.add(
                egui::TextEdit::singleline(&mut buf)
                    .id_salt(("field", label))
                    .hint_text(hint)
                    .password(password)
                    .margin(FIELD_MARGIN)
                    .desired_width(ui.available_width()),
            );
            let r = r.labelled_by(l.id);
            text_context_menu(ui, &r, &buf, password);
            (r.changed(), r.has_focus())
        })
        .inner;
    if let Some(e) = err {
        error_line(ui, p, e);
    }
    (changed.then_some(buf), focus)
}

/// Drop-down row over `(value, label)` options. `Some(value)` when the user picked another one.
pub(crate) fn choice_row(
    ui: &mut egui::Ui,
    p: &Palette,
    label: &str,
    current: &str,
    options: &[(&str, &str)],
    err: Option<&str>,
) -> Option<String> {
    let mut picked = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let l = label_cell(ui, p, label);
        let shown = options
            .iter()
            .find(|(v, _)| *v == current)
            .map_or(current, |(_, t)| *t);
        let r = egui::ComboBox::from_id_salt(("choice", label))
            .width(ui.available_width().min(260.0))
            .icon(combo_icon)
            .selected_text(RichText::new(shown).color(p.text))
            .show_ui(ui, |ui| {
                for (v, t) in options {
                    if ui.selectable_label(*v == current, *t).clicked() {
                        picked = Some((*v).to_string());
                    }
                }
            });
        let _ = r.response.labelled_by(l.id);
    });
    if let Some(e) = err {
        error_line(ui, p, e);
    }
    picked.filter(|v| v != current)
}

/// Switch row with an optional explanation. `Some(new)` when toggled.
pub(crate) fn toggle_row(
    ui: &mut egui::Ui,
    p: &Palette,
    label: &str,
    on: bool,
    help: &str,
    err: Option<&str>,
) -> Option<bool> {
    let mut toggled = None;
    let mut indent = 0.0;
    // One block, so the hint sits right under its label instead of a full row gap away (it read
    // as belonging to the next toggle).
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let r = account_switch(ui, p, on, label);
            if r.clicked() {
                toggled = Some(!on);
            }
            indent = r.rect.width() + 8.0;
            ui.label(RichText::new(label).size(FS_BODY).color(p.text));
        });
        // The explanation goes under the label and wraps: on the same row it ran off the window.
        if !help.is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(indent);
                ui.add(egui::Label::new(RichText::new(help).size(FS_SMALL).color(p.faint)).wrap());
            });
        }
    });
    if let Some(e) = err {
        error_line(ui, p, e);
    }
    toggled
}

/// Integer row. `Some(new)` once the edit is done (drag released, field left), not on every
/// frame of a drag or keystroke.
pub(crate) fn int_row(
    ui: &mut egui::Ui,
    p: &Palette,
    label: &str,
    value: i64,
    range: std::ops::RangeInclusive<i64>,
    err: Option<&str>,
) -> Option<i64> {
    let id = ui.id().with(("int_row", label));
    let mut v = ui.data(|d| d.get_temp::<i64>(id)).unwrap_or(value);
    let mut done = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let l = label_cell(ui, p, label);
        let r = ui.add(egui::DragValue::new(&mut v).range(range));
        let editing = r.dragged() || r.has_focus();
        done = r.drag_stopped() || r.lost_focus() || (r.changed() && !editing);
        let _ = r.labelled_by(l.id);
        ui.data_mut(|d| {
            if editing {
                d.insert_temp(id, v);
            } else {
                d.remove_temp::<i64>(id);
            }
        });
    });
    if let Some(e) = err {
        error_line(ui, p, e);
    }
    (done && v != value).then_some(v)
}

/// A read-only row: label and a (monospace-free) value.
pub(crate) fn info_row(ui: &mut egui::Ui, p: &Palette, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        label_cell(ui, p, label);
        ui.add(egui::Label::new(RichText::new(value).size(FS_SMALL).color(p.text)).wrap());
    });
}

/// A titled card grouping rows.
pub(crate) fn group(ui: &mut egui::Ui, p: &Palette, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(R_LG))
        .inner_margin(Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = SP_S;
            if !title.is_empty() {
                ui.label(strong(title, FS_BODY).color(p.text));
            }
            body(ui);
        });
}

/// A small outlined button (secondary actions in a form).
pub(crate) fn small_button(
    ui: &mut egui::Ui,
    p: &Palette,
    text: &str,
    enabled: bool,
) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(text).size(FS_SMALL).color(p.text))
            .fill(egui::Color32::TRANSPARENT)
            .stroke(Stroke::new(1.0, p.border_strong))
            .corner_radius(CornerRadius::same(R_MD))
            .min_size(egui::vec2(0.0, CONTROL_H)),
    )
}

/// The accent-filled primary button.
pub(crate) fn primary_button(
    ui: &mut egui::Ui,
    p: &Palette,
    text: &str,
    enabled: bool,
) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(strong(text, FS_BODY).color(p.on_accent))
            .fill(p.accent)
            .stroke(Stroke::NONE)
            .corner_radius(CornerRadius::same(R_MD))
            .min_size(egui::vec2(88.0, CONTROL_H)),
    )
}

/// [`primary_button`] with a leading icon; the accessible name is `text` alone.
pub(crate) fn primary_icon_button(
    ui: &mut egui::Ui,
    p: &Palette,
    glyph: &str,
    text: &str,
    enabled: bool,
) -> egui::Response {
    let r = ui.add_enabled(
        enabled,
        egui::Button::new((
            icon(glyph, FS_BODY + 1.0).color(p.on_accent),
            strong(text, FS_BODY).color(p.on_accent),
        ))
        .fill(p.accent)
        .stroke(Stroke::NONE)
        .corner_radius(CornerRadius::same(R_MD))
        .min_size(egui::vec2(88.0, CONTROL_H)),
    );
    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, text));
    r
}

/// A one-line status message on a tone's soft fill.
pub(crate) fn message_box(ui: &mut egui::Ui, tone: crate::theme::Tone, text: &str) {
    egui::Frame::new()
        .fill(tone.soft)
        .corner_radius(CornerRadius::same(R_MD))
        .inner_margin(Margin::symmetric(10, SP_XS as i8 + 2))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(egui::Label::new(RichText::new(text).size(FS_SMALL).color(tone.ink)).wrap());
        });
}

/// A problem as three lines: icon and strong title in the tone's ink, the hint under it, and the
/// server's raw answer as muted monospace details. Everything wraps.
pub(crate) fn problem_lines(
    ui: &mut egui::Ui,
    p: &Palette,
    tone: crate::theme::Tone,
    title: &str,
    hint: &str,
    detail: &str,
) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.label(icon(ic::WARNING_CIRCLE, 15.0).color(tone.ink));
            ui.add(egui::Label::new(strong(title, FS_SMALL).color(tone.ink)).wrap());
        });
        ui.add(egui::Label::new(RichText::new(hint).size(FS_SMALL).color(p.text)).wrap());
        if !detail.is_empty() {
            ui.add(egui::Label::new(mono(detail, FS_MICRO).color(p.muted)).wrap());
        }
    });
}

/// [`problem_lines`] inside a frame on the tone's soft fill.
pub(crate) fn problem_box(
    ui: &mut egui::Ui,
    p: &Palette,
    tone: crate::theme::Tone,
    title: &str,
    hint: &str,
    detail: &str,
) {
    egui::Frame::new()
        .fill(tone.soft)
        .corner_radius(CornerRadius::same(R_MD))
        .inner_margin(Margin::symmetric(10, SP_XS as i8 + 2))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            problem_lines(ui, p, tone, title, hint, detail);
        });
}

/// The outcome of an account test: a failure with a named problem shows its title, hint and
/// evidence; anything else (success, `other`, an old server) shows the one-line message.
pub(crate) fn test_result_box(ui: &mut egui::Ui, p: &Palette, r: &super::rpc::TestResult) {
    match r.problem {
        Some(problem) if !r.ok && problem != siple_sip::problem::Problem::Other => {
            problem_box(ui, p, p.err, problem.title(), problem.hint(), &r.detail);
        }
        _ => message_box(ui, if r.ok { p.ok } else { p.err }, &r.message),
    }
}
