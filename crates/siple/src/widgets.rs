//! Small drawing primitives of the design system: lamps, pills, chips, buttons, cards.

use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Margin, Response, RichText, Sense, Stroke,
    StrokeKind,
};

use crate::theme::{self, FS_BODY, FS_ICON, FS_MICRO, FS_SMALL, Palette, R_LG, R_MD, R_SM, Tone};

/// Text in the semibold face.
pub fn strong(text: impl Into<String>, size: f32) -> RichText {
    RichText::new(text).family(theme::semibold()).size(size)
}

/// Text in the monospace face.
pub fn mono(text: impl Into<String>, size: f32) -> RichText {
    RichText::new(text).family(FontFamily::Monospace).size(size)
}

/// Inner margin of every single-line text field, so fields share the 32 pt height of the combo
/// boxes and buttons beside them.
pub const FIELD_MARGIN: Margin = Margin::symmetric(9, 7);

/// The brand mark: the app icon's tile (same handset as the Call button), `side` points square.
/// Downscaled once per pixel size on the CPU (a box filter), so it stays crisp at any scale.
pub fn brand_mark(ui: &mut egui::Ui, side: f32) -> Response {
    let px = (side * ui.ctx().pixels_per_point()).round().max(1.0) as usize;
    // One cache entry per display size, replaced when the scale changes (zoom, another
    // monitor); dropping the old handle frees its texture.
    let id = egui::Id::new(("siple-brand-mark", side.to_bits()));
    let cached = ui
        .ctx()
        .data(|d| d.get_temp::<(usize, Option<egui::TextureHandle>)>(id))
        .filter(|(at, _)| *at == px);
    let tex = match cached {
        Some((_, t)) => t,
        None => {
            let tex = eframe::icon_data::from_png_bytes(crate::BRAND_MARK_PNG)
                .ok()
                .filter(|i| i.width == i.height && i.width > 0)
                .map(|i| {
                    let rgba = crate::shell::tray_model::render_icon(
                        &i.rgba,
                        i.width as usize,
                        px,
                        crate::shell::tray_model::TrayIconState::Idle,
                    );
                    ui.ctx().load_texture(
                        "brand-mark",
                        egui::ColorImage::from_rgba_unmultiplied([px, px], &rgba),
                        egui::TextureOptions::LINEAR,
                    )
                });
            ui.ctx().data_mut(|d| d.insert_temp(id, (px, tex.clone())));
            tex
        }
    };
    // Painted, not an `Image` widget: decoration beside the "Siple" title, so screen readers
    // get no unnamed image.
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(side, side), Sense::hover());
    if let Some(t) = tex {
        ui.painter().image(
            t.id(),
            rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }
    resp
}

/// A Phosphor glyph (regular weight).
pub fn icon(glyph: &str, size: f32) -> RichText {
    RichText::new(glyph)
        .family(FontFamily::Proportional)
        .size(size)
}

/// Direction of a [`paint_chevron`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Chevron {
    Up,
    Down,
    Left,
    Right,
}

/// A stroked chevron centred on `c` (the embedded Phosphor subset has only the caret-down and
/// caret-right glyphs, so all four directions are drawn the same way).
pub fn paint_chevron(
    painter: &egui::Painter,
    c: egui::Pos2,
    dir: Chevron,
    size: f32,
    color: Color32,
) {
    let h = size / 2.0;
    let k = h * 0.55;
    // Tip first, then the two ends of the arms.
    let (tip, a, b) = match dir {
        Chevron::Right => (egui::vec2(k, 0.0), egui::vec2(-k, -h), egui::vec2(-k, h)),
        Chevron::Left => (egui::vec2(-k, 0.0), egui::vec2(k, -h), egui::vec2(k, h)),
        Chevron::Down => (egui::vec2(0.0, k), egui::vec2(-h, -k), egui::vec2(h, -k)),
        Chevron::Up => (egui::vec2(0.0, -k), egui::vec2(-h, k), egui::vec2(h, k)),
    };
    let stroke = Stroke::new(1.8, color);
    painter.line_segment([c + a, c + tip], stroke);
    painter.line_segment([c + tip, c + b], stroke);
}

/// Keyboard focus ring around `rect` when `r` has focus (same style as [`account_switch`]).
pub fn paint_focus_ring(ui: &egui::Ui, p: &Palette, r: &Response, rect: egui::Rect) {
    if r.has_focus() {
        ui.painter().rect_stroke(
            rect.expand(1.0),
            CornerRadius::same(R_MD),
            Stroke::new(1.5, p.accent),
            StrokeKind::Outside,
        );
    }
}

/// Segmented control: one inset track, the selected segment raised on it. `items` are
/// (visible label, accessible name); `id_salt` keeps two controls in one Ui apart. Returns the
/// index clicked this frame.
pub fn segmented(
    ui: &mut egui::Ui,
    p: &Palette,
    id_salt: &str,
    items: &[(&str, &str)],
    selected: usize,
) -> Option<usize> {
    let base = ui.id().with(id_salt);
    let font = FontId::new(FS_SMALL, FontFamily::Proportional);
    let galleys: Vec<_> = items
        .iter()
        .map(|(l, _)| {
            ui.painter()
                .layout_no_wrap((*l).to_string(), font.clone(), p.text)
        })
        .collect();
    let (pad_x, h, inset) = (10.0, 28.0, 2.0);
    let widths: Vec<f32> = galleys.iter().map(|g| g.size().x + 2.0 * pad_x).collect();
    let total = widths.iter().sum::<f32>() + 2.0 * inset;
    let (track, _) = ui.allocate_exact_size(egui::vec2(total, h), Sense::hover());
    ui.painter().rect(
        track,
        CornerRadius::same(R_MD),
        p.inset,
        Stroke::new(1.0, p.border),
        StrokeKind::Inside,
    );
    let raised = if p.dark {
        Color32::from_rgb(0x2A, 0x36, 0x44)
    } else {
        p.surface
    };
    let seg_r = CornerRadius::same(R_MD - 2);
    let mut x = track.left() + inset;
    let mut clicked = None;
    for (i, g) in galleys.into_iter().enumerate() {
        let rect = egui::Rect::from_min_size(
            egui::pos2(x, track.top() + inset),
            egui::vec2(widths[i], h - 2.0 * inset),
        );
        x += widths[i];
        let r = ui.interact(rect, base.with(i), Sense::click());
        let on = i == selected;
        if on {
            ui.painter().rect_filled(
                rect.translate(egui::vec2(0.0, 1.0)),
                seg_r,
                Color32::from_black_alpha(if p.dark { 70 } else { 18 }),
            );
            ui.painter().rect_filled(rect, seg_r, raised);
        } else if r.hovered() {
            ui.painter()
                .rect_filled(rect, seg_r, p.border.gamma_multiply(0.5));
        }
        let ink = if on || r.hovered() { p.text } else { p.muted };
        let pos = rect.center() - g.size() / 2.0;
        ui.painter().galley_with_override_text_color(pos, g, ink);
        paint_focus_ring(ui, p, &r, rect);
        let name = items[i].1;
        r.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, on, name));
        if r.on_hover_text(name).clicked() {
            clicked = Some(i);
        }
    }
    clicked
}

/// A disclosure toggle: chevron (right when closed, down when open) and a quiet label.
pub fn disclosure(ui: &mut egui::Ui, p: &Palette, open: bool, label: &str) -> Response {
    let galley =
        ui.painter()
            .layout_no_wrap(label.to_string(), FontId::proportional(FS_SMALL), p.muted);
    let size = egui::vec2(16.0 + galley.size().x + 4.0, 24.0);
    let (rect, r) = ui.allocate_exact_size(size, Sense::click());
    let ink = if r.hovered() { p.text } else { p.muted };
    paint_chevron(
        ui.painter(),
        egui::pos2(rect.left() + 6.0, rect.center().y),
        if open { Chevron::Down } else { Chevron::Right },
        8.0,
        ink,
    );
    ui.painter().galley_with_override_text_color(
        egui::pos2(rect.left() + 16.0, rect.center().y - galley.size().y / 2.0),
        galley,
        ink,
    );
    paint_focus_ring(ui, p, &r, rect);
    r.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, open, label));
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A small square icon button with a chevron. The caller sets the accessible label with
/// `widget_info` and the hover text.
pub fn chevron_button(ui: &mut egui::Ui, p: &Palette, dir: Chevron, enabled: bool) -> Response {
    let (rect, r) = ui.allocate_exact_size(
        egui::vec2(24.0, 24.0),
        if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    if enabled && r.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(R_MD), p.accent_soft);
    }
    let ink = if enabled { p.text } else { p.faint };
    paint_chevron(ui.painter(), rect.center(), dir, 10.0, ink);
    paint_focus_ring(ui, p, &r, rect);
    r
}

/// A Phosphor glyph (fill weight).
pub fn icon_fill(glyph: &str, size: f32) -> RichText {
    RichText::new(glyph).family(theme::icons_fill()).size(size)
}

pub fn paint_lamp(ui: &egui::Ui, c: egui::Pos2, tone: Tone, lit: bool, diameter: f32) {
    let p = ui.painter();
    let r = diameter / 2.0;
    if lit {
        p.circle_filled(c, r + 2.5, tone.led.gamma_multiply(0.22));
        p.circle_filled(c, r, tone.led);
        // Specular dot: reads as a lens, not a flat dot.
        p.circle_filled(
            c + egui::vec2(-r * 0.3, -r * 0.3),
            r * 0.32,
            Color32::from_white_alpha(70),
        );
    } else {
        p.circle_stroke(c, r - 0.5, Stroke::new(1.5, tone.led));
    }
}

/// The off phase of a blinking lamp: same hue, mostly dark.
pub fn dimmed(t: Tone) -> Tone {
    Tone {
        led: t.led.gamma_multiply(0.3),
        ..t
    }
}

/// Compact account switch; the confirmed setting drives its position and accessible state.
pub fn account_switch(ui: &mut egui::Ui, p: &Palette, on: bool, label: &str) -> Response {
    let (hit_rect, response) = ui.allocate_exact_size(egui::vec2(34.0, 26.0), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), on, label)
    });
    if ui.is_rect_visible(hit_rect) {
        let track = egui::Rect::from_center_size(hit_rect.center(), egui::vec2(34.0, 20.0));
        let position = ui.ctx().animate_bool_responsive(response.id, on);
        let fill = egui::lerp(
            egui::Rgba::from(p.border_strong)..=egui::Rgba::from(p.accent),
            position,
        );
        let painter = ui.painter();
        painter.rect_filled(track, CornerRadius::same(10), Color32::from(fill));
        let thumb = egui::pos2(
            egui::lerp(track.left() + 10.0..=track.right() - 10.0, position),
            track.center().y,
        );
        painter.circle_filled(
            thumb + egui::vec2(0.0, 1.0),
            7.5,
            Color32::from_black_alpha(30),
        );
        painter.circle_filled(thumb, 7.0, Color32::WHITE);
        if response.has_focus() {
            painter.rect_stroke(
                track.expand(2.0),
                CornerRadius::same(12),
                Stroke::new(1.5, p.accent),
                StrokeKind::Outside,
            );
        }
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Status pill: lamp + label on the tone's soft fill. Painted directly, so it lays out the
/// same in left-to-right and right-to-left rows.
pub fn pill(ui: &mut egui::Ui, tone: Tone, label: &str, lit: bool) -> Response {
    let galley = ui.painter().layout_no_wrap(
        label.to_string(),
        FontId::new(FS_MICRO, theme::semibold()),
        tone.ink,
    );
    let lamp_d = 7.0;
    let (pad_l, gap, pad_r, h) = (8.0, 6.0, 10.0, 22.0);
    let size = egui::vec2(pad_l + lamp_d + gap + galley.size().x + pad_r, h);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(255), tone.soft);
    paint_lamp(
        ui,
        egui::pos2(rect.left() + pad_l + lamp_d / 2.0, rect.center().y),
        tone,
        lit,
        lamp_d,
    );
    let text_pos = egui::pos2(
        rect.left() + pad_l + lamp_d + gap,
        rect.center().y - galley.size().y / 2.0,
    );
    ui.painter().galley(text_pos, galley, tone.ink);
    // Painted, not a label widget: give screen readers the text.
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, label));
    resp
}

/// Small rectangular tag (transport, codec, flags). `tone` = None is the neutral variant.
/// Painted directly (see [`pill`]).
pub fn chip(
    ui: &mut egui::Ui,
    p: &Palette,
    glyph: Option<&str>,
    text: RichText,
    tone: Option<Tone>,
) -> Response {
    let (fill, stroke, ink) = match tone {
        Some(t) => (t.soft, Stroke::NONE, t.ink),
        None => (p.inset, Stroke::new(1.0, p.border), p.muted),
    };
    let text = text.color(ink);
    let galley = egui::WidgetText::from(text).into_galley(
        ui,
        Some(egui::TextWrapMode::Extend),
        f32::INFINITY,
        egui::FontSelection::Default,
    );
    let icon_galley = glyph.map(|g| {
        ui.painter().layout_no_wrap(
            g.to_string(),
            FontId::new(FS_SMALL + 0.5, FontFamily::Proportional),
            ink,
        )
    });
    let (pad, gap, h) = (7.0, 4.0, 22.0);
    let icon_w = icon_galley.as_ref().map_or(0.0, |g| g.size().x + gap);
    let size = egui::vec2(pad * 2.0 + icon_w + galley.size().x, h);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect(
        rect,
        CornerRadius::same(R_SM),
        fill,
        stroke,
        StrokeKind::Inside,
    );
    let mut x = rect.left() + pad;
    if let Some(g) = icon_galley {
        let w = g.size().x;
        ui.painter()
            .galley(egui::pos2(x, rect.center().y - g.size().y / 2.0), g, ink);
        x += w + gap;
    }
    let text_for_readers = galley.text().to_string();
    ui.painter().galley(
        egui::pos2(x, rect.center().y - galley.size().y / 2.0),
        galley,
        ink,
    );
    resp.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text_for_readers.clone())
    });
    resp
}

/// Solid call button (Answer, Reject, Hang up): white semibold text on `fill`, at least
/// `min_width` wide.
pub fn solid_button(
    ui: &mut egui::Ui,
    glyph: &str,
    label: &str,
    fill: Color32,
    min_width: f32,
) -> Response {
    let fg = Color32::WHITE;
    let b = egui::Button::new((
        icon_fill(glyph, FS_ICON).color(fg),
        strong(label, FS_BODY).color(fg),
    ))
    .fill(fill)
    .stroke(Stroke::NONE)
    .corner_radius(CornerRadius::same(R_MD))
    .min_size(egui::vec2(min_width, 34.0));
    ui.add(b)
}

/// Secondary button with an icon; `latched` shows it locked on in that tone (Mute, Hold).
pub fn soft_button(
    ui: &mut egui::Ui,
    p: &Palette,
    glyph: &str,
    label: &str,
    latched: Option<Tone>,
    enabled: bool,
    min_width: f32,
) -> Response {
    let (fill, stroke, ink) = if let Some(tone) = latched {
        (tone.soft, Stroke::new(1.0, tone.led), tone.ink)
    } else {
        (
            if p.dark {
                Color32::from_rgb(0x20, 0x2A, 0x35)
            } else {
                p.surface
            },
            Stroke::new(1.0, p.border_strong),
            p.text,
        )
    };
    let b = egui::Button::new((
        icon(glyph, FS_ICON).color(ink),
        RichText::new(label).color(ink),
    ))
    .fill(fill)
    .stroke(stroke)
    .corner_radius(CornerRadius::same(R_MD))
    .min_size(egui::vec2(min_width, 34.0));
    ui.add_enabled(enabled, b)
}

/// Quiet footer button: icon + label, frame only on hover.
pub fn ghost_button(ui: &mut egui::Ui, p: &Palette, glyph: &str, label: &str) -> Response {
    let b = egui::Button::new((
        icon(glyph, FS_ICON).color(p.muted),
        RichText::new(label).size(FS_SMALL).color(p.text),
    ))
    .frame_when_inactive(false)
    .corner_radius(CornerRadius::same(R_MD));
    ui.add(b)
}

/// Raised card (calls): surface, hairline, soft shadow.
pub fn raised_card(p: &Palette, border: Stroke) -> egui::Frame {
    egui::Frame::new()
        .fill(p.surface)
        .stroke(border)
        .corner_radius(CornerRadius::same(R_LG))
        .inner_margin(Margin::symmetric(16, 14))
        .shadow(egui::Shadow {
            offset: [0, 2],
            blur: 10,
            spread: 0,
            color: if p.dark {
                Color32::from_black_alpha(80)
            } else {
                Color32::from_rgba_unmultiplied(20, 32, 48, 18)
            },
        })
}

/// Flat panel (accounts, dial row): surface and hairline, no shadow.
pub fn panel(p: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(R_LG))
        .inner_margin(Margin::symmetric(14, 12))
}

/// Full-width hairline.
pub fn hairline(ui: &mut egui::Ui, p: &Palette) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 1.0), Sense::hover());
    ui.painter()
        .hline(rect.x_range(), rect.center().y, Stroke::new(1.0, p.border));
}

/// One telephone keypad key: digit with the letters below, like a desk phone.
pub fn keypad_key(ui: &mut egui::Ui, p: &Palette, digit: char) -> Response {
    let letters = match digit {
        '2' => "ABC",
        '3' => "DEF",
        '4' => "GHI",
        '5' => "JKL",
        '6' => "MNO",
        '7' => "PQRS",
        '8' => "TUV",
        '9' => "WXYZ",
        '0' => "+",
        _ => "",
    };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(52.0, 40.0), Sense::click());
    let painter = ui.painter();
    let hovered = resp.hovered();
    let pressed = resp.is_pointer_button_down_on();
    let fill = if pressed {
        p.accent_soft
    } else if hovered {
        p.inset
    } else {
        p.surface
    };
    let stroke = if hovered {
        Stroke::new(1.0, p.accent)
    } else {
        Stroke::new(1.0, p.border_strong)
    };
    painter.rect(
        rect,
        CornerRadius::same(R_SM + 1),
        fill,
        stroke,
        StrokeKind::Inside,
    );
    let digit_pos = if letters.is_empty() {
        rect.center()
    } else {
        rect.center() - egui::vec2(0.0, 5.0)
    };
    painter.text(
        digit_pos,
        egui::Align2::CENTER_CENTER,
        digit,
        FontId::new(17.0, theme::mono_medium()),
        p.text,
    );
    if !letters.is_empty() {
        painter.text(
            rect.center() + egui::vec2(0.0, 11.0),
            egui::Align2::CENTER_CENTER,
            letters,
            FontId::new(8.5, theme::semibold()),
            p.muted,
        );
    }
    resp.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, digit.to_string())
    });
    resp
}
