//! Visual design system: palette (light + dark), type scale, spacing, radii, fonts and the egui
//! style built from them.
//!
//! Direction: a desk-phone instrument panel. Status is always shown the way a desk phone shows
//! it, as a lamp (a round LED with a soft halo), in one semantic color set shared by
//! registration, call state and BLF. Neutrals are a cool slate; the single brand accent is the
//! teal-blue of the app icon and is reserved for focus, selection and the brand mark. Call
//! actions use their own solid green / red so "answer" and "hang up" never compete with chrome.

use std::sync::Arc;

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Shadow,
    Stroke, TextStyle, Theme, Visuals,
};

// ---- spacing / radii -------------------------------------------------------------------------

/// Spacing scale (points).
pub const SP_XS: f32 = 4.0;
pub const SP_S: f32 = 8.0;
pub const SP_M: f32 = 12.0;
pub const SP_L: f32 = 16.0;

/// Chips, keypad keys, sidecar keys.
pub const R_SM: u8 = 5;
/// Buttons, inputs, combo boxes.
pub const R_MD: u8 = 7;
/// Cards and panels.
pub const R_LG: u8 = 10;

/// Below this width the shell is one column with line tabs (narrow).
pub const NARROW_BREAKPOINT: f32 = 760.0;
/// From this width the shell has three panes: lines, stage, extensions (wide). Between the two
/// breakpoints the extensions are a drawer (medium).
pub const WIDE_BREAKPOINT: f32 = 1180.0;

// ---- type scale ------------------------------------------------------------------------------

pub const FS_DISPLAY: f32 = 22.0;
pub const FS_TITLE: f32 = 15.0;
pub const FS_BODY: f32 = 13.5;
pub const FS_SMALL: f32 = 12.0;
pub const FS_MICRO: f32 = 11.0;
pub const FS_MONO: f32 = 12.5;
pub const FS_TIMER: f32 = 19.0;
pub const FS_ICON: f32 = 15.0;

pub const SANS: &str = "plex-sans";
pub const SANS_SEMIBOLD: &str = "plex-sans-semibold";
pub const MONO: &str = "plex-mono";
pub const MONO_MEDIUM: &str = "plex-mono-medium";
pub const ICONS: &str = "phosphor";
pub const ICONS_FILL: &str = "phosphor-fill";

pub fn semibold() -> FontFamily {
    FontFamily::Name(SANS_SEMIBOLD.into())
}

pub fn mono_medium() -> FontFamily {
    FontFamily::Name(MONO_MEDIUM.into())
}

pub fn icons_fill() -> FontFamily {
    FontFamily::Name(ICONS_FILL.into())
}

// ---- palette ---------------------------------------------------------------------------------

/// One semantic status color: `led` for lamps and borders, `ink` for text on `soft`.
#[derive(Debug, Clone, Copy)]
pub struct Tone {
    pub led: Color32,
    pub ink: Color32,
    pub soft: Color32,
}

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    /// Window background (behind cards).
    pub bg: Color32,
    /// Cards, header, footer.
    pub surface: Color32,
    /// Inset areas: sidecar module, inputs, log pane.
    pub inset: Color32,
    pub border: Color32,
    pub border_strong: Color32,
    pub text: Color32,
    pub muted: Color32,
    /// Hints and disabled text only (not for information).
    pub faint: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    pub accent_soft: Color32,
    pub ok: Tone,
    pub warn: Tone,
    pub err: Tone,
    pub held: Tone,
    pub ring: Tone,
    pub idle: Tone,
    /// Solid call buttons (white text on both, AA).
    pub answer: Color32,
    pub hangup: Color32,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

const fn tone(led: u32, ink: u32, soft: u32) -> Tone {
    Tone {
        led: hex(led),
        ink: hex(ink),
        soft: hex(soft),
    }
}

impl Palette {
    /// The accent as a status tone (transfers in progress).
    pub fn accent_tone(&self) -> Tone {
        Tone {
            led: self.accent,
            ink: self.accent,
            soft: self.accent_soft,
        }
    }
}

pub const LIGHT: Palette = Palette {
    dark: false,
    bg: hex(0xEEF1F4),
    surface: hex(0xFFFFFF),
    inset: hex(0xF4F6F8),
    border: hex(0xDCE1E7),
    border_strong: hex(0xC3CBD5),
    text: hex(0x17202A),
    muted: hex(0x566271),
    faint: hex(0x7D8896),
    accent: hex(0x17769A),
    on_accent: hex(0xFFFFFF),
    accent_soft: hex(0xDDEFF5),
    ok: tone(0x22A05A, 0x146B3A, 0xE3F4EA),
    warn: tone(0xE0A100, 0x7A5600, 0xFBF1D6),
    err: tone(0xDC3B30, 0xA8231B, 0xFBE5E3),
    held: tone(0x7C6BE0, 0x4E3FB0, 0xECE9FB),
    ring: tone(0xF07C00, 0x9A4A00, 0xFDEBD9),
    idle: tone(0xA3ADB9, 0x566271, 0xEDF0F3),
    answer: hex(0x177A41),
    hangup: hex(0xC9352B),
};

pub const DARK: Palette = Palette {
    dark: true,
    bg: hex(0x10161D),
    surface: hex(0x182029),
    inset: hex(0x131A22),
    border: hex(0x27323E),
    border_strong: hex(0x384656),
    text: hex(0xE4E9EF),
    muted: hex(0x9AA5B1),
    faint: hex(0x6B7683),
    accent: hex(0x3FB2D4),
    on_accent: hex(0x062029),
    accent_soft: hex(0x16313D),
    ok: tone(0x3DD68C, 0x6FE3A8, 0x12301F),
    warn: tone(0xF2C14E, 0xF5CD6B, 0x33290F),
    err: tone(0xFF6B5E, 0xFF8D83, 0x3A1A18),
    held: tone(0xA89BFF, 0xBDB3FF, 0x262145),
    ring: tone(0xFF9A2E, 0xFFB060, 0x3A2510),
    idle: tone(0x5E6A77, 0x9AA5B1, 0x1E2731),
    answer: hex(0x177A41),
    hangup: hex(0xC9352B),
};

/// The palette of the theme `ui` is drawn with.
pub fn palette(ui: &egui::Ui) -> &'static Palette {
    if ui.visuals().dark_mode {
        &DARK
    } else {
        &LIGHT
    }
}

// ---- icons (Phosphor, subset embedded in assets/fonts) ----------------------------------------

pub mod icon {
    pub const ARROW_BEND_UP_RIGHT: &str = "\u{E026}";
    pub const ARROW_LEFT: &str = "\u{E058}";
    pub const ARROW_RIGHT: &str = "\u{E06C}";
    pub const ARROWS_CLOCKWISE: &str = "\u{E094}";
    pub const BELL_SLASH: &str = "\u{E0D4}";
    pub const DOTS_NINE: &str = "\u{E1FC}";
    pub const ENVELOPE_SIMPLE: &str = "\u{E218}";
    pub const FILE_TEXT: &str = "\u{E23A}";
    pub const FOLDER_OPEN: &str = "\u{E256}";
    pub const LOCK_SIMPLE: &str = "\u{E308}";
    pub const LOCK_SIMPLE_OPEN: &str = "\u{E30A}";
    pub const MICROPHONE: &str = "\u{E326}";
    pub const MICROPHONE_SLASH: &str = "\u{E328}";
    pub const PAUSE: &str = "\u{E39E}";
    pub const PHONE: &str = "\u{E3B8}";
    pub const PHONE_INCOMING: &str = "\u{E3BE}";
    pub const PHONE_OUTGOING: &str = "\u{E3C0}";
    pub const PHONE_PLUS: &str = "\u{EC56}";
    pub const PHONE_TRANSFER: &str = "\u{E3C6}";
    pub const PHONE_X: &str = "\u{E3C4}";
    pub const PLAY: &str = "\u{E3D0}";
    pub const PLUS: &str = "\u{E3D4}";
    pub const SPEAKER_HIGH: &str = "\u{E44A}";
    pub const STOP: &str = "\u{E46C}";
    pub const TERMINAL_WINDOW: &str = "\u{EAE8}";
    pub const WARNING: &str = "\u{E4E0}";
    pub const WARNING_CIRCLE: &str = "\u{E4E2}";
    pub const X: &str = "\u{E4F6}";
}

// ---- fonts -----------------------------------------------------------------------------------

static PLEX_SANS: &[u8] = include_bytes!("../assets/fonts/IBMPlexSans-Regular.ttf");
static PLEX_SANS_SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/IBMPlexSans-SemiBold.ttf");
static PLEX_MONO: &[u8] = include_bytes!("../assets/fonts/IBMPlexMono-Regular.ttf");
static PLEX_MONO_MEDIUM: &[u8] = include_bytes!("../assets/fonts/IBMPlexMono-Medium.ttf");
static PHOSPHOR: &[u8] = include_bytes!("../assets/fonts/Phosphor-Regular.ttf");
static PHOSPHOR_FILL: &[u8] = include_bytes!("../assets/fonts/Phosphor-Fill.ttf");

fn font_definitions() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let icon_tweak = egui::FontTweak {
        y_offset_factor: 0.08,
        ..Default::default()
    };
    for (name, bytes) in [
        (SANS, PLEX_SANS),
        (SANS_SEMIBOLD, PLEX_SANS_SEMIBOLD),
        (MONO, PLEX_MONO),
        (MONO_MEDIUM, PLEX_MONO_MEDIUM),
    ] {
        fonts
            .font_data
            .insert(name.into(), Arc::new(FontData::from_static(bytes)));
    }
    for (name, bytes) in [(ICONS, PHOSPHOR), (ICONS_FILL, PHOSPHOR_FILL)] {
        fonts.font_data.insert(
            name.into(),
            Arc::new(FontData::from_static(bytes).tweak(icon_tweak.clone())),
        );
    }
    // Each family: primary face, then the Phosphor icons (so icons can sit inline in any text),
    // then egui's defaults as a fallback for anything else (emoji, rare scripts).
    let defaults_prop = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let defaults_mono = fonts
        .families
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();
    let chain = |primary: &str, icons: &str, rest: &[String]| -> Vec<String> {
        let mut v = vec![primary.to_string(), icons.to_string()];
        v.extend(rest.iter().cloned());
        v
    };
    fonts
        .families
        .insert(FontFamily::Proportional, chain(SANS, ICONS, &defaults_prop));
    fonts
        .families
        .insert(semibold(), chain(SANS_SEMIBOLD, ICONS, &defaults_prop));
    fonts
        .families
        .insert(FontFamily::Monospace, chain(MONO, ICONS, &defaults_mono));
    fonts
        .families
        .insert(mono_medium(), chain(MONO_MEDIUM, ICONS, &defaults_mono));
    fonts
        .families
        .insert(icons_fill(), chain(ICONS_FILL, SANS, &defaults_prop));
    fonts
}

// ---- style -----------------------------------------------------------------------------------

/// Installs fonts and the light + dark styles. The theme follows the system (egui default).
pub fn install(ctx: &egui::Context) {
    ctx.set_fonts(font_definitions());
    ctx.set_style_of(Theme::Light, style(&LIGHT));
    ctx.set_style_of(Theme::Dark, style(&DARK));
}

fn style(p: &Palette) -> egui::Style {
    let mut s = egui::Style {
        text_styles: [
            (
                TextStyle::Small,
                FontId::new(FS_SMALL, FontFamily::Proportional),
            ),
            (
                TextStyle::Body,
                FontId::new(FS_BODY, FontFamily::Proportional),
            ),
            (
                TextStyle::Button,
                FontId::new(FS_BODY, FontFamily::Proportional),
            ),
            (TextStyle::Heading, FontId::new(FS_TITLE, semibold())),
            (
                TextStyle::Monospace,
                FontId::new(FS_MONO, FontFamily::Monospace),
            ),
        ]
        .into(),
        ..Default::default()
    };

    let sp = &mut s.spacing;
    sp.item_spacing = egui::vec2(SP_S, SP_S);
    sp.button_padding = egui::vec2(10.0, 5.0);
    sp.interact_size = egui::vec2(32.0, 32.0);
    sp.combo_height = 300.0;
    sp.menu_margin = Margin::same(6);
    sp.window_margin = Margin::same(SP_M as i8);
    sp.icon_width = 14.0;
    sp.icon_width_inner = 8.0;
    sp.icon_spacing = 6.0;
    sp.tooltip_width = 360.0;
    sp.scroll.bar_width = 8.0;
    sp.scroll.floating = true;

    s.visuals = visuals(p);
    s
}

fn visuals(p: &Palette) -> Visuals {
    let mut v = if p.dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    v.override_text_color = None;
    v.weak_text_color = Some(p.muted);
    v.hyperlink_color = p.accent;
    v.faint_bg_color = p.inset;
    v.extreme_bg_color = p.inset;
    v.text_edit_bg_color = Some(p.inset);
    v.code_bg_color = p.inset;
    v.warn_fg_color = p.warn.ink;
    v.error_fg_color = p.err.ink;
    v.panel_fill = p.bg;
    v.window_fill = p.surface;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(R_LG);
    v.menu_corner_radius = CornerRadius::same(R_MD + 1);
    let shadow_color = if p.dark {
        Color32::from_black_alpha(110)
    } else {
        Color32::from_rgba_unmultiplied(20, 32, 48, 34)
    };
    v.window_shadow = Shadow {
        offset: [0, 6],
        blur: 18,
        spread: 0,
        color: shadow_color,
    };
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 14,
        spread: 0,
        color: shadow_color,
    };
    v.selection.bg_fill = p.accent_soft;
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.text_cursor.stroke = Stroke::new(2.0, p.accent);
    v.collapsing_header_frame = false;
    v.indent_has_left_vline = false;
    v.striped = false;

    let r = CornerRadius::same(R_MD);
    let w = &mut v.widgets;
    // Non-interactive: frames, separators, labels.
    w.noninteractive.bg_fill = p.surface;
    w.noninteractive.weak_bg_fill = p.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = r;
    // Resting buttons / combos: quiet surface with a hairline.
    let raised = if p.dark { hex(0x202A35) } else { hex(0xFFFFFF) };
    w.inactive.bg_fill = p.inset;
    w.inactive.weak_bg_fill = raised;
    w.inactive.bg_stroke = Stroke::new(1.0, p.border_strong);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = r;
    w.inactive.expansion = 0.0;
    let hover = if p.dark { hex(0x293542) } else { hex(0xF1F4F7) };
    w.hovered.bg_fill = hover;
    w.hovered.weak_bg_fill = hover;
    w.hovered.bg_stroke = Stroke::new(1.0, p.accent);
    w.hovered.fg_stroke = Stroke::new(1.5, p.text);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;
    let active = if p.dark { hex(0x2F3D4C) } else { hex(0xE6EBF0) };
    w.active.bg_fill = active;
    w.active.weak_bg_fill = active;
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(1.5, p.text);
    w.active.corner_radius = r;
    w.active.expansion = 0.0;
    w.open.bg_fill = p.inset;
    w.open.weak_bg_fill = raised;
    w.open.bg_stroke = Stroke::new(1.0, p.accent);
    w.open.fg_stroke = Stroke::new(1.0, p.text);
    w.open.corner_radius = r;
    v
}
