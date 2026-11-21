//! Labelled form rows shared by Settings and the first-run wizard. Every control gets its label
//! as accessible name (`labelled_by`), so screen readers and kittest find it by its text.

use eframe::egui::{self, Align, CornerRadius, Layout, Margin, RichText, Stroke};

use super::combo_icon;
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
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let r = account_switch(ui, p, on, label);
        if r.clicked() {
            toggled = Some(!on);
        }
        ui.label(RichText::new(label).size(FS_BODY).color(p.text));
        if !help.is_empty() {
            ui.label(RichText::new(help).size(FS_SMALL).color(p.faint));
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
