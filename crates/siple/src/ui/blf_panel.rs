//! Extensions (BLF) panel: three densities (keys, lamps, list), filter box and
//! chips, groups, click behaviour from `[ui]`, Pick up on ringing keys and subscription health.
//!
//! State is encoded by shape, colour and text, never colour alone: idle = filled circle,
//! ringing = ringed dot (the lamp blinks; a static bold border under reduced motion), dialing =
//! arrow, busy = filled square, held = pause bars, unknown = dashed ring, failed = triangle,
//! stale = clock and dimmed.

use std::collections::{HashMap, HashSet};

use eframe::egui::{self, Align, Color32, CornerRadius, Layout, Margin, RichText, Stroke};
use siple_core::config::{BlfClick, BlfClickInCall, BlfClickRinging, BlfGroupBy, BlfView};
use siple_sip::{BlfState, BlfStatus, CallInfo, SubscriptionPhase};

use super::{
    BLF_CLICK_DEBOUNCE, BlfChip, BlfSort, Cx, LayoutMode, UiAction, UiState, display_name,
    truncate_galley,
};
use crate::snapshot::{
    blf_dialog_lines, blf_health_lines, blf_state_text, blf_time_lines, blf_title, format_duration,
};
use crate::theme::{
    self, FS_BODY, FS_MICRO, FS_SMALL, Palette, R_LG, R_MD, R_SM, SP_S, Tone, icon as ic,
};
use crate::widgets::{icon, segmented, strong};

/// The filter box and chips appear from this many entries on (§3.6).
pub(crate) const FILTER_MIN_ENTRIES: usize = 12;
/// This many failed keys of one account with one code collapse into one banner (§3.5).
const BANNER_MIN_FAILURES: usize = 5;

pub(crate) fn blf_tone(p: &Palette, s: BlfState) -> (Tone, bool) {
    match s {
        BlfState::Idle => (p.ok, true),
        BlfState::Ringing => (p.ring, true),
        BlfState::Dialing => (p.warn, true),
        BlfState::Busy => (p.err, true),
        BlfState::Held => (p.held, true),
        BlfState::Unknown => (p.idle, false),
        BlfState::Failed => (p.err, false),
    }
}

/// The state glyph (shape + colour). `d` is its diameter; `on` is the lit phase of a ringing
/// lamp.
pub(crate) fn paint_state_glyph(
    ui: &egui::Ui,
    c: egui::Pos2,
    state: BlfState,
    tone: Tone,
    d: f32,
    on: bool,
) {
    let pt = ui.painter();
    let r = d / 2.0;
    let led = if state == BlfState::Ringing && !on {
        tone.led.gamma_multiply(0.3)
    } else {
        tone.led
    };
    let stroke = Stroke::new(1.7, led);
    match state {
        BlfState::Idle => {
            pt.circle_filled(c, r, led);
        }
        BlfState::Ringing => {
            pt.circle_filled(c, r * 0.55, led);
            pt.circle_stroke(c, r + 1.5, Stroke::new(1.4, led));
        }
        BlfState::Dialing => {
            let (a, b) = (c + egui::vec2(-r, r), c + egui::vec2(r, -r));
            pt.line_segment([a, b], stroke);
            pt.line_segment([b, b + egui::vec2(-r * 1.1, 0.0)], stroke);
            pt.line_segment([b, b + egui::vec2(0.0, r * 1.1)], stroke);
        }
        BlfState::Busy => {
            pt.rect_filled(
                egui::Rect::from_center_size(c, egui::Vec2::splat(d * 0.85)),
                CornerRadius::same(1),
                led,
            );
        }
        BlfState::Held => {
            for dx in [-r * 0.45, r * 0.45] {
                pt.rect_filled(
                    egui::Rect::from_center_size(c + egui::vec2(dx, 0.0), egui::vec2(d * 0.3, d)),
                    CornerRadius::same(1),
                    led,
                );
            }
        }
        BlfState::Unknown => {
            // Dashed ring.
            let n = 8;
            for i in (0..n).step_by(2) {
                let (a0, a1) = (
                    i as f32 / n as f32 * std::f32::consts::TAU,
                    (i as f32 + 1.0) / n as f32 * std::f32::consts::TAU,
                );
                let pts: Vec<_> = (0..=3)
                    .map(|k| {
                        let a = a0 + (a1 - a0) * k as f32 / 3.0;
                        c + egui::vec2(a.cos(), a.sin()) * r
                    })
                    .collect();
                pt.add(egui::Shape::line(pts, Stroke::new(1.5, led)));
            }
        }
        BlfState::Failed => paint_failed_glyph(ui, c, d * 0.55 + 1.0, led),
    }
}

/// Failed subscriptions: a warning triangle outline instead of a round lamp.
fn paint_failed_glyph(ui: &egui::Ui, c: egui::Pos2, r: f32, color: Color32) {
    let pts = vec![
        c + egui::vec2(0.0, -r),
        c + egui::vec2(r * 0.95, r * 0.75),
        c + egui::vec2(-r * 0.95, r * 0.75),
    ];
    ui.painter()
        .add(egui::Shape::closed_line(pts, Stroke::new(1.6, color)));
    ui.painter().line_segment(
        [c + egui::vec2(0.0, -r * 0.25), c + egui::vec2(0.0, r * 0.2)],
        Stroke::new(1.4, color),
    );
}

// ---- click behaviour --------------------------------------------------------------------------

/// What a plain click on a key does.
#[derive(Debug, PartialEq)]
pub(crate) enum BlfClickKind {
    /// Send this action.
    Act(UiAction),
    /// Put the key in the dial field.
    Prefill,
    /// Open the in-call menu ([`BlfInCallAction`]).
    Menu,
}

/// Decides a plain click (`[ui] blf_click*`):
/// - during a call (`line`): in a consult session the menu offers a new call only; a ringing key
///   with pickup (and `blf_click_ringing = "pickup"`) opens the menu leading with Pick up;
///   otherwise `blf_click_in_call` = menu | consult | blind;
/// - otherwise a ringing key with pickup picks up (`blf_click_ringing = "pickup"`), else
///   `blf_click` = dial (`blf.dial`) | prefill.
pub(crate) fn decide_click(
    b: &BlfStatus,
    line: Option<&CallInfo>,
    in_consult: bool,
    cfg: &siple_core::config::UiConfig,
) -> BlfClickKind {
    let pickup = b.pickup_offered() && cfg.blf_click_ringing == BlfClickRinging::Pickup;
    if let Some(line) = line {
        if in_consult || pickup {
            return BlfClickKind::Menu;
        }
        return match cfg.blf_click_in_call {
            BlfClickInCall::Menu => BlfClickKind::Menu,
            BlfClickInCall::Consult
                if BlfInCallAction::Consult.disabled_reason(line, b).is_none() =>
            {
                BlfClickKind::Act(BlfInCallAction::Consult.action(line, b))
            }
            BlfClickInCall::Blind
                if BlfInCallAction::Transfer.disabled_reason(line, b).is_none() =>
            {
                BlfClickKind::Act(BlfInCallAction::Transfer.action(line, b))
            }
            _ => BlfClickKind::Menu,
        };
    }
    if pickup {
        BlfClickKind::Act(UiAction::Pickup(b.account.clone(), b.uri.clone()))
    } else if cfg.blf_click == BlfClick::Prefill {
        BlfClickKind::Prefill
    } else {
        BlfClickKind::Act(UiAction::BlfDial(b.account.clone(), b.uri.clone()))
    }
}

/// Entries of the menu a click on a key opens during a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlfInCallAction {
    /// Pick up the ringing key (holds the current call).
    Pickup,
    /// Start a consult transfer session with the key.
    Consult,
    /// Blind-transfer the active line to the key ("Transfer now").
    Transfer,
    /// Call the key; the current call is held.
    NewCall,
}

impl BlfInCallAction {
    const ALL: [Self; 4] = [Self::Pickup, Self::Consult, Self::Transfer, Self::NewCall];

    /// The entries offered for a key, in menu order. In a consult session only a new call.
    pub(crate) fn offered(b: &BlfStatus, in_consult: bool) -> Vec<Self> {
        Self::ALL
            .into_iter()
            .filter(|e| match e {
                Self::NewCall => true,
                _ if in_consult => false,
                Self::Pickup => b.pickup_offered(),
                _ => true,
            })
            .collect()
    }

    fn label(self, line: &CallInfo, b: &BlfStatus) -> String {
        match self {
            Self::Pickup => "Pick up (holds current)".into(),
            Self::Consult => format!("Consult {}", b.uri),
            Self::Transfer => format!("Transfer {} to {}", display_name(line), b.uri),
            Self::NewCall => "New call (holds current)".into(),
        }
    }

    /// `Some(reason)` when the entry cannot be used for this key.
    fn disabled_reason(self, line: &CallInfo, b: &BlfStatus) -> Option<&'static str> {
        match self {
            // A bare extension would be resolved on the call's account, not the key's.
            Self::Transfer | Self::Consult
                if line.account != b.account
                    && !b.uri.contains('@')
                    && !b.uri.to_ascii_lowercase().starts_with("sip") =>
            {
                Some("The key belongs to another account than the call")
            }
            _ => None,
        }
    }

    pub(crate) fn action(self, line: &CallInfo, b: &BlfStatus) -> UiAction {
        match self {
            Self::Pickup => UiAction::Pickup(b.account.clone(), b.uri.clone()),
            Self::Consult => UiAction::ConsultStart(line.id.clone(), b.uri.clone()),
            Self::Transfer => UiAction::TransferBlind(line.id.clone(), b.uri.clone()),
            Self::NewCall => UiAction::BlfDial(b.account.clone(), b.uri.clone()),
        }
    }
}

/// Tooltip hint matching [`decide_click`].
fn click_hint(kind: &BlfClickKind) -> &'static str {
    match kind {
        BlfClickKind::Act(UiAction::Pickup(..)) => "Click to pick up",
        BlfClickKind::Act(UiAction::ConsultStart(..)) => "Click to consult",
        BlfClickKind::Act(UiAction::TransferBlind(..)) => "Click to transfer now",
        BlfClickKind::Act(_) => "Click to call",
        BlfClickKind::Prefill => "Click to fill the dial field",
        BlfClickKind::Menu => "Click for consult, transfer or new call",
    }
}

// ---- panel ------------------------------------------------------------------------------------

fn matches(st: &UiState, b: &BlfStatus) -> bool {
    let chip = match st.blf_chip {
        BlfChip::All => true,
        BlfChip::Ringing => b.state == BlfState::Ringing,
        BlfChip::Busy => matches!(b.state, BlfState::Busy | BlfState::Held | BlfState::Dialing),
        BlfChip::Problems => problem(b),
    };
    let q = st.blf_filter.trim().to_lowercase();
    chip && (q.is_empty()
        || b.uri.to_lowercase().contains(&q)
        || b.label.to_lowercase().contains(&q)
        || b.group.to_lowercase().contains(&q)
        || b.account.to_lowercase().contains(&q))
}

fn problem(b: &BlfStatus) -> bool {
    b.state == BlfState::Failed || b.stale || b.subscription.phase != SubscriptionPhase::Active
}

/// Group key and title of an entry.
fn group_of(by: BlfGroupBy, b: &BlfStatus) -> (String, String) {
    match by {
        BlfGroupBy::Account => (b.account.clone(), b.account.clone()),
        BlfGroupBy::Group if b.group.is_empty() => (b.account.clone(), b.account.clone()),
        BlfGroupBy::Group => (b.group.clone(), b.group.clone()),
        BlfGroupBy::None => (String::new(), String::new()),
    }
}

pub(super) fn show(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let snap = cx.vm.snap;
    let cfg = &snap.ui;
    let view = st.blf_view.unwrap_or(cfg.blf_view);
    let total = snap.blf.len();
    st.blf_acted = false;
    ui.spacing_mut().item_spacing.y = SP_S;
    let shown: Vec<&BlfStatus> = snap.blf.iter().filter(|b| matches(st, b)).collect();
    // Header: title, counts, density switch.
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(strong("EXTENSIONS", FS_SMALL + 0.5).color(p.text));
        let meta = if shown.len() == total {
            total.to_string()
        } else {
            format!("{} of {total}", shown.len())
        };
        ui.label(RichText::new(meta).size(FS_SMALL).color(p.muted));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            const VIEWS: [(BlfView, &str, &str); 3] = [
                (BlfView::Keys, "Keys", "Keys view"),
                (BlfView::Lamps, "Lamps", "Lamps view"),
                (BlfView::List, "List", "List view"),
            ];
            let items: Vec<(&str, &str)> = VIEWS.iter().map(|(_, l, n)| (*l, *n)).collect();
            let sel = VIEWS.iter().position(|(v, _, _)| *v == view).unwrap_or(0);
            if let Some(i) = segmented(ui, p, "blf_view", &items, sel) {
                st.blf_view = Some(VIEWS[i].0);
            }
        });
    });
    if total == 0 {
        ui.label(
            RichText::new(
                "No extensions are monitored. Add [[account.blf]] entries to the config.",
            )
            .size(FS_SMALL)
            .color(p.muted),
        );
        return;
    }
    if total > FILTER_MIN_ENTRIES {
        filter_bar(ui, cx, st);
    }
    health_banners(ui, cx);
    let quiet = quiet_failures(&shown);
    // Groups in first-seen order.
    let mut groups: Vec<(String, String, Vec<&BlfStatus>)> = Vec::new();
    for b in &shown {
        let (key, title) = group_of(cfg.blf_group_by, b);
        match groups.iter_mut().find(|g| g.0 == key) {
            Some(g) => g.2.push(b),
            None => groups.push((key, title, vec![b])),
        }
    }
    if view == BlfView::List {
        list_header(ui, cx, st);
        for g in &mut groups {
            sort_rows(st, &mut g.2);
        }
    }
    if shown.is_empty() {
        ui.label(
            RichText::new("No extension matches the filter.")
                .size(FS_SMALL)
                .color(p.muted),
        );
    }
    for (key, title, keys) in &groups {
        let open = key.is_empty() || !st.blf_collapsed.contains(key);
        if !key.is_empty() {
            group_header(ui, cx, st, key, title, keys, open);
        }
        if !open {
            continue;
        }
        ui.push_id(("blf_group", key), |ui| match view {
            BlfView::Keys => keys_grid(ui, cx, st, keys, &quiet, out),
            BlfView::Lamps => lamps_grid(ui, cx, st, keys, &quiet, out),
            BlfView::List => list_rows(ui, cx, st, keys, &quiet, out),
        });
    }
    // Narrow: the extensions are a tab; an acted click returns to the stage.
    if cx.mode == LayoutMode::Narrow && st.blf_acted && st.ext_tab {
        st.ext_tab = false;
    }
}

fn filter_bar(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState) {
    let p = cx.p;
    let snap = cx.vm.snap;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let w = ui.available_width() - 30.0;
        let edit = ui.add(
            egui::TextEdit::singleline(&mut st.blf_filter)
                .id_salt("blf_filter")
                .hint_text("Filter by name, extension or group")
                .margin(Margin::symmetric(8, 5))
                .desired_width(w),
        );
        edit.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "Filter extensions")
        });
        // `/` focuses the filter when nothing else is being typed into.
        if ui.memory(|m| m.focused().is_none())
            && super::keys::take(
                ui.ctx(),
                &snap.ui.shortcuts,
                siple_core::config::shortcuts::Shortcut::FilterExtensions,
            )
        {
            edit.request_focus();
        }
        if !st.blf_filter.is_empty()
            && ui
                .add(egui::Button::new(icon(ic::X, 13.0).color(p.muted)).frame(false))
                .on_hover_text("Clear filter")
                .clicked()
        {
            st.blf_filter.clear();
        }
    });
    let count = |f: &dyn Fn(&BlfStatus) -> bool| snap.blf.iter().filter(|b| f(b)).count();
    let chips = [
        (BlfChip::All, "All".to_string()),
        (
            BlfChip::Ringing,
            format!("Ringing {}", count(&|b| b.state == BlfState::Ringing)),
        ),
        (
            BlfChip::Busy,
            format!(
                "Busy {}",
                count(&|b| matches!(b.state, BlfState::Busy | BlfState::Held | BlfState::Dialing))
            ),
        ),
        (BlfChip::Problems, format!("Problems {}", count(&problem))),
    ];
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(6.0, 4.0);
        for (chip, label) in chips {
            let b = egui::Button::new(RichText::new(&label).size(FS_SMALL).color(p.text))
                .selected(st.blf_chip == chip)
                .corner_radius(CornerRadius::same(255));
            if ui.add(b).clicked() {
                st.blf_chip = chip;
            }
        }
    });
}

/// One banner per account whose keys fail with one code (`BANNER_MIN_FAILURES` or more).
fn failure_groups(cx: &Cx) -> Vec<(String, u16, usize)> {
    let mut counts: HashMap<(String, u16), usize> = HashMap::new();
    for b in cx
        .vm
        .snap
        .blf
        .iter()
        .filter(|b| b.state == BlfState::Failed)
    {
        if let Some(code) = b.subscription.last_code {
            *counts.entry((b.account.clone(), code)).or_default() += 1;
        }
    }
    let mut v: Vec<_> = counts
        .into_iter()
        .filter(|(_, n)| *n >= BANNER_MIN_FAILURES)
        .map(|((a, c), n)| (a, c, n))
        .collect();
    v.sort();
    v
}

fn health_banners(ui: &mut egui::Ui, cx: &Cx) {
    let p = cx.p;
    for (account, code, n) in failure_groups(cx) {
        egui::Frame::new()
            .fill(p.err.soft)
            .stroke(Stroke::new(1.0, p.err.led))
            .corner_radius(CornerRadius::same(R_MD))
            .inner_margin(Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(icon(ic::WARNING, 15.0).color(p.err.ink));
                    ui.add(
                        egui::Label::new(
                            strong(
                                format!("{account}: BLF rejected by PBX ({code})"),
                                FS_SMALL,
                            )
                            .color(p.err.ink),
                        )
                        .wrap(),
                    );
                });
                ui.label(
                    RichText::new(format!(
                        "{n} extensions failed with the same code. Check the account's BLF settings in the config."
                    ))
                    .size(FS_SMALL)
                    .color(p.text),
                );
            });
    }
}

/// Failed keys covered by a banner show a quiet (grey) glyph instead of N red ones.
fn quiet_failures(shown: &[&BlfStatus]) -> HashSet<String> {
    let mut counts: HashMap<(&str, u16), usize> = HashMap::new();
    for b in shown.iter().filter(|b| b.state == BlfState::Failed) {
        if let Some(c) = b.subscription.last_code {
            *counts.entry((&b.account, c)).or_default() += 1;
        }
    }
    shown
        .iter()
        .filter(|b| {
            b.state == BlfState::Failed
                && b.subscription
                    .last_code
                    .is_some_and(|c| counts[&(b.account.as_str(), c)] >= BANNER_MIN_FAILURES)
        })
        .map(|b| format!("{}\0{}", b.account, b.uri))
        .collect()
}

fn group_header(
    ui: &mut egui::Ui,
    cx: &Cx,
    st: &mut UiState,
    key: &str,
    title: &str,
    keys: &[&BlfStatus],
    open: bool,
) {
    let p = cx.p;
    let ringing = keys.iter().filter(|b| b.state == BlfState::Ringing).count();
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 24.0), egui::Sense::click());
    let c = egui::pos2(rect.left() + 8.0, rect.center().y);
    let s = 3.5;
    let pts = if open {
        vec![
            c + egui::vec2(-s, -s * 0.5),
            c + egui::vec2(0.0, s * 0.5),
            c + egui::vec2(s, -s * 0.5),
        ]
    } else {
        vec![
            c + egui::vec2(-s * 0.5, -s),
            c + egui::vec2(s * 0.5, 0.0),
            c + egui::vec2(-s * 0.5, s),
        ]
    };
    ui.painter().line(pts, Stroke::new(1.5, p.muted));
    let label = format!("{title} ({})", keys.len());
    ui.painter().text(
        egui::pos2(rect.left() + 20.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        &label,
        egui::FontId::new(FS_SMALL + 0.5, theme::semibold()),
        p.text,
    );
    if ringing > 0 {
        ui.painter().text(
            egui::pos2(rect.right() - 4.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            format!("{ringing} ringing"),
            egui::FontId::new(FS_SMALL, theme::semibold()),
            p.ring.ink,
        );
    }
    resp.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::CollapsingHeader,
            true,
            open,
            format!("Group {title}, {} extensions", keys.len()),
        )
    });
    if resp
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
    {
        if open {
            st.blf_collapsed.insert(key.to_string());
        } else {
            st.blf_collapsed.remove(key);
        }
    }
}

// ---- densities ---------------------------------------------------------------------------------

/// Keys (comfortable): lamp keys in a module, min 132 px, up to 6 columns, 44 px high.
fn keys_grid(
    ui: &mut egui::Ui,
    cx: &Cx,
    st: &mut UiState,
    keys: &[&BlfStatus],
    quiet: &HashSet<String>,
    out: &mut Vec<UiAction>,
) {
    egui::Frame::new()
        .fill(cx.p.inset)
        .stroke(Stroke::new(1.0, cx.p.border))
        .corner_radius(CornerRadius::same(R_LG))
        .inner_margin(Margin::same(8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let gap = 6.0;
            let avail = ui.available_width();
            let min_key = 132.0;
            let cols = (((avail + gap) / (min_key + gap)).floor() as usize).clamp(1, 6);
            let key_w = ((avail - gap * (cols as f32 - 1.0)) / cols as f32).floor();
            for (i, row) in keys.chunks(cols).enumerate() {
                if i > 0 {
                    ui.add_space(gap - ui.spacing().item_spacing.y);
                }
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for b in row {
                        ui.push_id((&b.account, &b.uri), |ui| {
                            key_item(ui, cx, st, b, key_w, quiet, out)
                        });
                    }
                });
            }
        });
}

/// Lamps (compact): 28 px rows of glyph, label and extension in columns.
fn lamps_grid(
    ui: &mut egui::Ui,
    cx: &Cx,
    st: &mut UiState,
    keys: &[&BlfStatus],
    quiet: &HashSet<String>,
    out: &mut Vec<UiAction>,
) {
    let avail = ui.available_width();
    let min_col = 170.0;
    let cols = ((avail / min_col).floor() as usize).clamp(1, 4);
    let col_w = (avail / cols as f32).floor();
    ui.spacing_mut().item_spacing.y = 2.0;
    for row in keys.chunks(cols) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for b in row {
                ui.push_id((&b.account, &b.uri), |ui| {
                    lamp_item(ui, cx, st, b, col_w, quiet, out)
                });
            }
        });
    }
}

fn list_rows(
    ui: &mut egui::Ui,
    cx: &Cx,
    st: &mut UiState,
    keys: &[&BlfStatus],
    quiet: &HashSet<String>,
    out: &mut Vec<UiAction>,
) {
    ui.spacing_mut().item_spacing.y = 2.0;
    for b in keys {
        ui.push_id((&b.account, &b.uri), |ui| {
            row_item(ui, cx, st, b, quiet, out)
        });
    }
}

/// Column header of the list: click Name / Ext / State to sort; "Ringing first" toggles.
fn list_header(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState) {
    let p = cx.p;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(RichText::new("Sort").size(FS_SMALL).color(p.muted));
        for (s, label) in [
            (BlfSort::Name, "Name"),
            (BlfSort::Ext, "Ext"),
            (BlfSort::State, "State"),
        ] {
            let b = egui::Button::new(RichText::new(label).size(FS_SMALL).color(p.text))
                .selected(st.blf_sort == s)
                .corner_radius(CornerRadius::same(R_MD));
            let r = ui
                .add(b)
                .on_hover_text(format!("Sort by {}", label.to_lowercase()));
            if r.clicked() {
                st.blf_sort = if st.blf_sort == s { BlfSort::Config } else { s };
            }
        }
        let b = egui::Button::new(RichText::new("Ringing first").size(FS_SMALL).color(p.text))
            .selected(st.blf_ringing_first)
            .corner_radius(CornerRadius::same(R_MD));
        if ui.add(b).clicked() {
            st.blf_ringing_first = !st.blf_ringing_first;
        }
    });
}

fn state_rank(s: BlfState) -> u8 {
    match s {
        BlfState::Ringing => 0,
        BlfState::Busy => 1,
        BlfState::Dialing => 2,
        BlfState::Held => 3,
        BlfState::Idle => 4,
        BlfState::Unknown => 5,
        BlfState::Failed => 6,
    }
}

fn sort_rows(st: &UiState, rows: &mut [&BlfStatus]) {
    match st.blf_sort {
        BlfSort::Config => {}
        BlfSort::Name => rows.sort_by_key(|b| blf_title(b).to_lowercase()),
        BlfSort::Ext => rows.sort_by(|a, b| a.uri.cmp(&b.uri)),
        BlfSort::State => rows.sort_by_key(|b| state_rank(b.state)),
    }
    if st.blf_ringing_first {
        rows.sort_by_key(|b| b.state != BlfState::Ringing);
    }
}

// ---- items ------------------------------------------------------------------------------------

/// Tone of a key: a failure covered by a banner is quiet.
fn item_tone(cx: &Cx, b: &BlfStatus, quiet: &HashSet<String>) -> (Tone, bool) {
    let (tone, lit) = blf_tone(cx.p, b.state);
    if quiet.contains(&format!("{}\0{}", b.account, b.uri)) {
        (cx.p.idle, lit)
    } else {
        (tone, lit)
    }
}

/// Peer of the live dialog (`Dana`), if any.
fn peer(b: &BlfStatus) -> Option<String> {
    b.dialogs
        .iter()
        .find(|d| d.state != siple_sip::DialogPhase::Terminated)
        .and_then(|d| d.remote_display.clone().or(d.remote_identity.clone()))
}

/// Stale marker: a small painted clock (no font glyph needed).
fn stale_badge(ui: &egui::Ui, p: &Palette, at: egui::Pos2) {
    let pt = ui.painter();
    let s = Stroke::new(1.2, p.muted);
    pt.circle_stroke(at, 4.5, s);
    pt.line_segment([at, at + egui::vec2(0.0, -3.0)], s);
    pt.line_segment([at, at + egui::vec2(2.2, 0.8)], s);
}

/// A key of the keys density.
fn key_item(
    ui: &mut egui::Ui,
    cx: &Cx,
    st: &mut UiState,
    b: &BlfStatus,
    width: f32,
    quiet: &HashSet<String>,
    out: &mut Vec<UiAction>,
) {
    let p = cx.p;
    if b.stale {
        ui.set_opacity(0.5);
    }
    let failed = b.state == BlfState::Failed;
    let (tone, _) = item_tone(cx, b, quiet);
    let ringing = b.state == BlfState::Ringing;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 44.0), egui::Sense::click());
    let key_fill = if p.dark {
        Color32::from_rgb(0x1C, 0x25, 0x2F)
    } else {
        p.surface
    };
    let painter = ui.painter_at(rect);
    // Failed keys get a heavier outline, ringing ones a bold border under reduced motion
    // (shape, not only hue).
    let outline = if failed {
        Stroke::new(1.5, tone.led)
    } else if ringing && cx.reduce_motion {
        Stroke::new(2.0, tone.led)
    } else {
        Stroke::new(1.0, p.border)
    };
    painter.rect(
        rect,
        CornerRadius::same(R_SM + 1),
        key_fill,
        outline,
        egui::StrokeKind::Inside,
    );
    paint_state_glyph(
        ui,
        egui::pos2(rect.left() + 15.0, rect.center().y),
        b.state,
        tone,
        10.0,
        cx.blink,
    );
    let text_x = rect.left() + 28.0;
    let max_w = rect.right() - text_x - 6.0 - if b.stale { 12.0 } else { 0.0 };
    let title_galley = truncate_galley(
        ui,
        blf_title(b),
        FS_BODY - 0.5,
        theme::semibold(),
        p.text,
        max_w,
    );
    painter.galley(egui::pos2(text_x, rect.top() + 6.0), title_galley, p.text);
    if b.stale {
        stale_badge(ui, p, egui::pos2(rect.right() - 11.0, rect.top() + 14.0));
    }
    let state = blf_state_text(b);
    let state_galley = truncate_galley(ui, &state, FS_MICRO, theme::semibold(), tone.ink, max_w);
    let state_w = state_galley.size().x;
    painter.galley(
        egui::pos2(text_x, rect.top() + 25.0),
        state_galley,
        tone.ink,
    );
    // "Pick up" affordance on a ringing key with pickup configured, only when it fits beside
    // the state text (the tooltip says it either way).
    let pick_galley = b
        .pickup_offered()
        .then(|| truncate_galley(ui, "Pick up", FS_MICRO, theme::semibold(), tone.ink, 80.0));
    let pick_rect = pick_galley.as_ref().and_then(|g| {
        let w = g.size().x + 12.0;
        (rect.right() - 4.0 - w >= text_x + state_w + 6.0).then(|| {
            egui::Rect::from_min_size(
                egui::pos2(rect.right() - 4.0 - w, rect.top() + 22.0),
                egui::vec2(w, 18.0),
            )
        })
    });
    let extra_right = pick_rect.map_or(rect.right() - 6.0, |r| r.left() - 4.0);
    if blf_title(b) != b.uri {
        let x = text_x + state_w + 6.0;
        let g = truncate_galley(
            ui,
            &b.uri,
            FS_MICRO,
            egui::FontFamily::Monospace,
            p.muted,
            (extra_right - x).max(0.0),
        );
        painter.galley(egui::pos2(x, rect.top() + 25.5), g, p.muted);
    }
    let mut pick_clicked = false;
    if let (Some(r), Some(g)) = (pick_rect, pick_galley) {
        painter.rect_filled(r, CornerRadius::same(R_SM), tone.soft);
        painter.galley(
            egui::pos2(r.left() + 6.0, r.center().y - g.size().y / 2.0),
            g,
            tone.ink,
        );
        pick_clicked = pickup_button(ui, &resp, r, b);
    }
    finish(ui, cx, st, b, resp, pick_clicked, out);
}

/// Interactive "Pick up" rectangle on a key / row.
fn pickup_button(ui: &mut egui::Ui, key: &egui::Response, r: egui::Rect, b: &BlfStatus) -> bool {
    let pick = ui.interact(r, key.id.with("pickup"), egui::Sense::click());
    pick.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            true,
            format!("Pick up {} ({})", blf_title(b), b.account),
        )
    });
    pick.on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

/// A row of the lamps density.
fn lamp_item(
    ui: &mut egui::Ui,
    cx: &Cx,
    st: &mut UiState,
    b: &BlfStatus,
    width: f32,
    quiet: &HashSet<String>,
    out: &mut Vec<UiAction>,
) {
    let p = cx.p;
    if b.stale {
        ui.set_opacity(0.5);
    }
    let (tone, _) = item_tone(cx, b, quiet);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 28.0), egui::Sense::click());
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(R_SM), p.inset);
    }
    if b.state == BlfState::Ringing && cx.reduce_motion {
        ui.painter().rect_stroke(
            rect.shrink(1.0),
            CornerRadius::same(R_SM),
            Stroke::new(2.0, tone.led),
            egui::StrokeKind::Inside,
        );
    }
    paint_state_glyph(
        ui,
        egui::pos2(rect.left() + 12.0, rect.center().y),
        b.state,
        tone,
        9.0,
        cx.blink,
    );
    let ext = truncate_galley(
        ui,
        &b.uri,
        FS_MICRO,
        egui::FontFamily::Monospace,
        p.muted,
        56.0,
    );
    let ext_w = ext.size().x;
    let show_ext = blf_title(b) != b.uri;
    let title_w = rect.width()
        - 30.0
        - if show_ext { ext_w + 8.0 } else { 0.0 }
        - if b.stale { 14.0 } else { 0.0 };
    let title = truncate_galley(
        ui,
        blf_title(b),
        FS_SMALL + 0.5,
        theme::semibold(),
        p.text,
        title_w.max(20.0),
    );
    ui.painter().galley(
        egui::pos2(rect.left() + 26.0, rect.center().y - title.size().y / 2.0),
        title,
        p.text,
    );
    if show_ext {
        ui.painter().galley(
            egui::pos2(
                rect.right() - ext_w - 6.0,
                rect.center().y - ext.size().y / 2.0,
            ),
            ext,
            p.muted,
        );
    }
    if b.stale {
        stale_badge(
            ui,
            p,
            egui::pos2(
                rect.right() - if show_ext { ext_w + 18.0 } else { 10.0 },
                rect.center().y,
            ),
        );
    }
    finish(ui, cx, st, b, resp, false, out);
}

/// A row of the list density: state glyph, name, ext, state text, with, for, Pick up.
fn row_item(
    ui: &mut egui::Ui,
    cx: &Cx,
    st: &mut UiState,
    b: &BlfStatus,
    quiet: &HashSet<String>,
    out: &mut Vec<UiAction>,
) {
    let p = cx.p;
    if b.stale {
        ui.set_opacity(0.5);
    }
    let (tone, _) = item_tone(cx, b, quiet);
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 30.0), egui::Sense::click());
    let painter = ui.painter_at(rect);
    painter.rect(
        rect,
        CornerRadius::same(R_SM),
        if resp.hovered() { p.inset } else { p.surface },
        if b.state == BlfState::Ringing && cx.reduce_motion {
            Stroke::new(2.0, tone.led)
        } else {
            Stroke::new(1.0, p.border)
        },
        egui::StrokeKind::Inside,
    );
    paint_state_glyph(
        ui,
        egui::pos2(rect.left() + 14.0, rect.center().y),
        b.state,
        tone,
        9.0,
        cx.blink,
    );
    let wide = w > 520.0;
    let action_w = if b.pickup_offered() { 66.0 } else { 0.0 };
    let (state_w, with_w, for_w) = (
        96.0,
        if wide { 110.0 } else { 0.0 },
        if wide { 44.0 } else { 0.0 },
    );
    let mut right = rect.right() - 6.0 - action_w;
    let cell = |ui: &egui::Ui, text: &str, x: f32, w: f32, color: Color32, mono_font: bool| {
        if w <= 0.0 {
            return;
        }
        let g = truncate_galley(
            ui,
            text,
            FS_SMALL,
            if mono_font {
                egui::FontFamily::Monospace
            } else {
                theme::semibold()
            },
            color,
            w - 4.0,
        );
        ui.painter()
            .galley(egui::pos2(x, rect.center().y - g.size().y / 2.0), g, color);
    };
    let for_text = if matches!(
        b.state,
        BlfState::Idle | BlfState::Unknown | BlfState::Failed
    ) {
        String::new()
    } else {
        format_duration((cx.vm.now - b.updated_at).num_seconds())
    };
    right -= for_w;
    cell(ui, &for_text, right, for_w, p.muted, true);
    right -= with_w;
    cell(
        ui,
        &peer(b).unwrap_or_default(),
        right,
        with_w,
        p.muted,
        false,
    );
    right -= state_w;
    cell(ui, &blf_state_text(b), right, state_w, tone.ink, false);
    right -= 52.0;
    cell(ui, &b.uri, right, 52.0, p.muted, true);
    let name_w = (right - (rect.left() + 28.0) - 4.0).max(20.0);
    cell(ui, blf_title(b), rect.left() + 28.0, name_w, p.text, false);
    if b.stale {
        stale_badge(
            ui,
            p,
            egui::pos2(rect.left() + 14.0 + 12.0, rect.center().y - 9.0),
        );
    }
    let mut pick_clicked = false;
    if b.pickup_offered() {
        let r = egui::Rect::from_min_size(
            egui::pos2(rect.right() - 6.0 - action_w + 4.0, rect.center().y - 10.0),
            egui::vec2(action_w - 4.0, 20.0),
        );
        painter.rect_filled(r, CornerRadius::same(R_SM), tone.soft);
        painter.text(
            r.center(),
            egui::Align2::CENTER_CENTER,
            "Pick up",
            egui::FontId::new(FS_MICRO, theme::semibold()),
            tone.ink,
        );
        pick_clicked = pickup_button(ui, &resp, r, b);
    }
    finish(ui, cx, st, b, resp, pick_clicked, out);
}

// ---- interaction shared by all densities -------------------------------------------------------

/// Tooltip, accessible name / value, debounced click and the in-call menu of one key.
fn finish(
    ui: &mut egui::Ui,
    cx: &Cx,
    st: &mut UiState,
    b: &BlfStatus,
    resp: egui::Response,
    pick_clicked: bool,
    out: &mut Vec<UiAction>,
) {
    let snap = cx.vm.snap;
    let line = cx.active_call();
    let in_consult = line.is_some_and(|l| {
        snap.transfers.iter().any(|s| {
            !s.phase.is_terminal()
                && (s.original == l.id || s.consult.as_deref() == Some(l.id.as_str()))
        })
    });
    let kind = decide_click(b, line, in_consult, &snap.ui);
    let state = blf_state_text(b);
    let mut tip = vec![format!("{} ({})", b.uri, b.account), state];
    tip.extend(blf_dialog_lines(b));
    tip.extend(blf_health_lines(b));
    tip.extend(blf_time_lines(b));
    tip.push(
        if b.pickup_offered()
            && snap.ui.blf_click_ringing == BlfClickRinging::Pickup
            && line.is_none()
        {
            "Click to pick up"
        } else {
            click_hint(&kind)
        }
        .into(),
    );
    // Name stays "<label> (<account>)"; the state travels as the accessible value.
    resp.widget_info(|| {
        let mut info = egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            true,
            format!("{} ({})", blf_title(b), b.account),
        );
        info.current_text_value = Some(access_value(b));
        info
    });
    let clicked = resp
        .clone()
        .on_hover_text(tip.join("\n"))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked();
    // One click, one action: a second click on the same key within the debounce window (a
    // double click) is dropped.
    let mut accepted = false;
    if pick_clicked || clicked {
        let now = std::time::Instant::now();
        let key = format!("{}\0{}", b.account, b.uri);
        accepted = st
            .blf_last_click
            .get(&key)
            .is_none_or(|t| now.duration_since(*t) >= BLF_CLICK_DEBOUNCE);
        if accepted {
            st.blf_last_click.insert(key, now);
        }
    }
    let mut result = None;
    let mut open_menu = false;
    if accepted {
        if pick_clicked {
            result = Some(UiAction::Pickup(b.account.clone(), b.uri.clone()));
        } else {
            match kind {
                BlfClickKind::Act(a) => result = Some(a),
                BlfClickKind::Prefill => {
                    st.dial_target = b.uri.clone();
                    st.dial_account = Some(b.account.clone());
                    st.new_call_open = true;
                    st.blf_acted = true;
                }
                BlfClickKind::Menu => open_menu = true,
            }
        }
    }
    if let Some(line) = line {
        // A click the debounce swallowed (a double click) must not close the open menu: for
        // that frame clicks do not close it (Escape and the entries still do).
        let popup_id = egui::Popup::default_response_id(&resp);
        let swallowed = clicked && !accepted && egui::Popup::is_id_open(ui.ctx(), popup_id);
        egui::Popup::menu(&resp)
            .close_behavior(if swallowed {
                egui::PopupCloseBehavior::IgnoreClicks
            } else {
                egui::PopupCloseBehavior::CloseOnClick
            })
            .open_memory(open_menu.then_some(egui::SetOpenCommand::Bool(true)))
            .show(|ui| {
                for entry in BlfInCallAction::offered(b, in_consult) {
                    let reason = entry.disabled_reason(line, b);
                    let r =
                        ui.add_enabled(reason.is_none(), egui::Button::new(entry.label(line, b)));
                    let r = match reason {
                        Some(why) => r.on_disabled_hover_text(why),
                        None => r,
                    };
                    if r.clicked() {
                        result = Some(entry.action(line, b));
                        ui.close();
                    }
                }
            });
    }
    if let Some(a) = result {
        st.blf_acted = true;
        out.push(a);
    }
}

/// Accessible value of a key: `<ext>, <state>[, with <peer>][, stale]`.
fn access_value(b: &BlfStatus) -> String {
    let mut v = format!("{}, {}", b.uri, blf_state_text(b));
    if let Some(who) = peer(b) {
        v.push_str(&format!(", with {who}"));
    }
    if b.stale {
        v.push_str(", stale");
    }
    v
}
