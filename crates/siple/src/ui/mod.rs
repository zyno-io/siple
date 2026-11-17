//! Views: the desktop shell (header, line rail, stage, extensions sidecar, footer).
//!
//! [`draw`] renders from plain data ([`ViewModel`] + [`UiState`]) and returns what the user asked
//! for as [`UiAction`]s, so the views can be rendered without a running engine (snapshot tests).
//! [`PhoneUi`] is the live wrapper that feeds it the current snapshot and executes the actions.
//!
//! Layout, chosen from the window width ([`layout_for`]):
//! - wide (>= 1180 px): lines rail, stage, extensions pane (open by default; the header chevron
//!   collapses it);
//! - medium (760-1180 px): rail and stage, extensions as a right drawer (open by default when the stage keeps >= 440 px, else collapsed);
//! - narrow (< 760 px): one column with line tabs, extensions as a tab.
//!
//! Modules: [`header`] (brand, account chips and menus, voicemail, extensions chevron), [`rail`] (lines),
//! [`stage`] (selected line, call-waiting banners, dial), [`transfer`] (composer and session
//! panel), [`blf_panel`] (extensions), [`footer`] (status, logs) and [`audio`] (devices).

mod audio;
mod blf_panel;
pub(crate) mod draft;
mod footer;
mod form;
mod header;
mod keys;
mod rail;
pub(crate) mod rpc;
pub(crate) mod settings;
mod stage;
mod transfer;
pub(crate) mod wizard;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use chrono::{DateTime, Utc};
use eframe::egui::{self, Align, CornerRadius, Layout, Margin, RichText, Stroke};
use siple_core::config::BlfView;
use siple_core::state::PartialSelection;
use siple_sip::{AccountStatus, AnswerMode, CallDirection, CallInfo, CallState, CompleteMode};
use siple_sip::{RegState, TransferSession};

#[cfg(not(feature = "store"))]
use crate::actions::open_url;
use crate::actions::{Actions, open_path};
use crate::snapshot::{RegClass, Shared, Snapshot, reg_class, visible_calls};
use crate::theme::{
    self, FS_BODY, FS_TITLE, NARROW_BREAKPOINT, Palette, R_MD, SP_L, SP_S, Tone, WIDE_BREAKPOINT,
    icon as ic,
};
use crate::widgets::{icon_fill, strong};

const NOTICE_TTL: Duration = Duration::from_secs(8);
/// A finished transfer session stays in the stage this long (unless dismissed).
pub(crate) const SESSION_LINGER_SECS: i64 = 30;

// ---- view model / state / actions -------------------------------------------------------------

/// Everything a frame draws, as plain data.
pub struct ViewModel<'a> {
    pub snap: &'a Snapshot,
    /// Clock for call timers and the ended-call linger.
    pub now: DateTime<Utc>,
    pub config_path: &'a Path,
    /// A log directory exists (shows "Open logs").
    pub has_log_dir: bool,
    /// Transient failure message (last action that failed).
    pub notice: Option<&'a str>,
    /// Microphone level and echo-test progress (meters).
    pub live: LiveAudio,
}

/// Live audio readings, taken every frame while a meter is on screen.
#[derive(Debug, Clone, Default)]
pub struct LiveAudio {
    /// Microphone level, dBFS; `None` without a microphone.
    pub mic_dbfs: Option<f32>,
    pub echo: siple_core::media::EchoStatus,
}

/// The transfer composer: open for one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composer {
    /// The call being transferred.
    pub call_id: String,
    pub target: String,
}

/// Filter chips of the extensions panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlfChip {
    #[default]
    All,
    Ringing,
    Busy,
    Problems,
}

/// Sort column of the list density.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlfSort {
    /// Config order.
    #[default]
    Config,
    Name,
    Ext,
    State,
}

/// UI-only state that survives between frames.
#[derive(Debug, Default)]
pub struct UiState {
    pub dial_account: Option<String>,
    pub dial_target: String,
    /// Focus the dial field on the next frame (set when a link prefills it).
    pub focus_dial: bool,
    pub show_logs: bool,
    /// The audio device panel above the footer.
    pub audio_open: bool,
    /// Why the audio layer is off (`status.audio.disabled_reason`); fixed at startup.
    pub audio_disabled_reason: Option<String>,
    /// When the last click on each BLF key was accepted (`account\0uri`): a second click within
    /// [`BLF_CLICK_DEBOUNCE`] is ignored so a double click cannot dial twice.
    pub blf_last_click: HashMap<String, std::time::Instant>,

    /// The line the stage shows (UI-only; the engine's `active_line` is separate).
    pub selected_line: Option<String>,
    /// Display slot (L1, L2, ...) of every visible call: the lowest free number when first seen.
    pub slots: BTreeMap<String, usize>,
    /// Last seen state per call (detects a ringing call becoming active).
    call_states: HashMap<String, CallState>,
    /// Ended calls the user dismissed.
    pub dismissed_calls: HashSet<String>,
    /// Ringing calls whose banner was silenced.
    pub silenced: HashSet<String>,
    /// The dial bar is open although lines exist ("+ New call").
    pub new_call_open: bool,
    /// Wide / medium: this session's extensions panel choice. `None` = the saved choice, else
    /// the default: open on wide windows and on medium ones wide enough to leave the stage
    /// 440 px next to the drawer. Latched to the current value when the rail is toggled, so the
    /// panel does not jump when the rail width changes the default.
    pub ext_open: Option<bool>,
    /// The panel choice remembered across launches (loaded from `state.toml`, set by an explicit
    /// toggle or `ui.set_panels`). Wide / medium only.
    pub ext_pref: Option<bool>,
    /// Narrow: the extensions tab replaces the stage. Session only, never saved, and separate
    /// from the wide / medium panel state.
    pub ext_tab: bool,
    /// The Lines rail shows the full list; collapsed (the default) is a slim strip of badges.
    pub lines_expanded: bool,
    /// The rail choice remembered across launches: `Some(collapsed)`.
    pub lines_pref: Option<bool>,
    /// What was last handed to the state writer (or loaded), to skip unchanged writes.
    pub panels_saved: siple_core::state::GuiPanels,
    /// Whether the extensions panel was showing in the last frame (for `ui.state`).
    pub ext_shown: bool,

    pub composer: Option<Composer>,
    /// Finished transfer sessions the user dismissed.
    pub dismissed_sessions: HashSet<String>,

    /// Density chosen in the panel header; `None` = `[ui] blf_view`. Session state only: the
    /// GUI never writes the config.
    pub blf_view: Option<BlfView>,
    pub blf_filter: String,
    pub blf_chip: BlfChip,
    pub blf_collapsed: HashSet<String>,
    pub blf_sort: BlfSort,
    pub blf_ringing_first: bool,
    /// A BLF click acted this frame (narrow: returns from the extensions tab to the stage).
    pub(crate) blf_acted: bool,

    /// Smoothed microphone level of the meter, dBFS.
    pub meter_db: f32,
    /// The Settings page.
    pub settings: settings::SettingsState,
    /// The first-run wizard.
    pub wizard: wizard::WizardState,
}

/// Clicks on the same BLF key closer together than this are one click.
pub const BLF_CLICK_DEBOUNCE: Duration = Duration::from_secs(1);

/// What the user asked for in a frame.
#[derive(Debug, Clone, PartialEq)]
pub enum UiAction {
    Answer(String),
    /// Call waiting: answer and hold the other lines.
    AnswerHold(String),
    /// Call waiting: answer `.0`, end `.1` once `.0` is active.
    AnswerEnd(String, String),
    /// Declines (with another call live: `[calls] decline_code`).
    Reject(String),
    /// Silences the ringing call's banner; the call keeps ringing (the engine has no ringer
    /// silencing yet, so the tone is unaffected).
    Silence(String),
    /// Click on a held line: `call.activate`.
    Activate(String),
    Hangup(String),
    Mute(String, bool),
    Hold(String, bool),
    Dtmf(String, char),
    TransferBlind(String, String),
    /// `call.consult_start`: hold the call and dial the target on its account.
    ConsultStart(String, String),
    /// `call.consult_link`: link a second live call as the consult call.
    ConsultLink(String, String),
    TransferComplete(String, CompleteMode),
    TransferSwap(String),
    TransferCancel(String),
    Dial(String, String),
    /// Call a monitored BLF entry on its account (`blf.dial`): `(account, blf uri)`.
    BlfDial(String, String),
    /// Directed pickup of a ringing BLF key: `(account, blf uri)`.
    Pickup(String, String),
    SetDnd(String, bool),
    TestSpeaker,
    SetDevices(PartialSelection),
    SetAccountEnabled(String, bool),
    OpenConfig,
    OpenLogs,
    /// The log pane was opened / closed (the poller only tails the log while it is open).
    LogsVisible(bool),
    /// A control-API call (`config.edit`, `account.test`, ...); the reply comes back through
    /// [`UiState::deliver`].
    Rpc(rpc::RpcRequest),
    /// The first-run wizard was finished or skipped (recorded in `state.toml`).
    FirstRunDone,
    /// Ask the OS for microphone access (macOS prompt; elsewhere a no-op).
    RequestMicAccess,
    /// Open a release page in the browser (the update pill).
    #[cfg(not(feature = "store"))]
    OpenUrl(String),
    /// "Skip this version" of the update pill: remembered in `state.toml`.
    #[cfg(not(feature = "store"))]
    SkipUpdate(String),
}

// ---- layout -----------------------------------------------------------------------------------

/// The shell variant for a window width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    Wide,
    Medium,
    Narrow,
}

/// Medium layout: rail width, default extensions drawer width, and the stage width the drawer
/// may leave for it to be open by default.
const MEDIUM_RAIL_W: f32 = 208.0;
const MEDIUM_DRAWER_W: f32 = 300.0;
const MEDIUM_STAGE_MIN: f32 = 440.0;

pub fn layout_for(width: f32) -> LayoutMode {
    if width >= WIDE_BREAKPOINT {
        LayoutMode::Wide
    } else if width >= NARROW_BREAKPOINT {
        LayoutMode::Medium
    } else {
        LayoutMode::Narrow
    }
}

/// Per-frame read-only context shared by the panels.
pub(crate) struct Cx<'a> {
    pub p: &'static Palette,
    pub vm: &'a ViewModel<'a>,
    /// Visible calls (live plus recently ended, minus dismissed).
    pub calls: Vec<&'a CallInfo>,
    pub mode: LayoutMode,
    /// Window width the layout was chosen from.
    pub width: f32,
    /// Lamps blink (false in the off phase and when motion is reduced: static then).
    pub blink: bool,
    /// `[ui] reduced_motion = "on"`: ringing shows a static bold border instead of blinking.
    pub reduce_motion: bool,
}

impl<'a> Cx<'a> {
    pub fn call(&self, id: &str) -> Option<&'a CallInfo> {
        self.calls.iter().find(|c| c.id == id).copied()
    }

    /// Lines in slot order (stable: never reordered by state changes).
    pub fn lines(&self, st: &UiState) -> Vec<(usize, &'a CallInfo)> {
        let mut v: Vec<_> = self
            .calls
            .iter()
            .map(|c| (st.slots.get(&c.id).copied().unwrap_or(usize::MAX), *c))
            .collect();
        v.sort_by_key(|(s, _)| *s);
        v
    }

    pub fn selected(&self, st: &UiState) -> Option<&'a CallInfo> {
        st.selected_line.as_deref().and_then(|id| self.call(id))
    }

    /// The un-held, established device call (what a BLF click would hold).
    pub fn active_call(&self) -> Option<&'a CallInfo> {
        let id = self.vm.snap.active_line.as_ref()?;
        self.call(id)
            .filter(|c| matches!(c.state, CallState::Active | CallState::RemoteHeld))
    }

    /// Slot label: `L1`.
    pub fn slot(&self, st: &UiState, id: &str) -> String {
        st.slots
            .get(id)
            .map_or_else(String::new, |s| format!("L{s}"))
    }

    pub fn ext_open(&self, st: &UiState) -> bool {
        match self.mode {
            // The narrow tab is its own session state.
            LayoutMode::Narrow => st.ext_tab,
            LayoutMode::Wide => st.ext_open.or(st.ext_pref).unwrap_or(true),
            // Only when the stage keeps room for a call card next to the drawer; a saved
            // "open" means "open when it fits", a choice made this session is honoured.
            LayoutMode::Medium => st
                .ext_open
                .unwrap_or(st.ext_pref != Some(false) && self.fits(st)),
        }
    }

    /// Medium: the drawer leaves the stage its minimum width next to the current rail.
    fn fits(&self, st: &UiState) -> bool {
        let rail = if st.lines_expanded {
            MEDIUM_RAIL_W
        } else {
            rail::COLLAPSED_W
        };
        self.width - rail - MEDIUM_DRAWER_W >= MEDIUM_STAGE_MIN
    }

    /// The panel choice as `ui.state` reports it. In narrow windows that is the wide / medium
    /// panel (the saved or session choice, closed when none), not the tab.
    fn panel_state(&self, st: &UiState) -> bool {
        match self.mode {
            LayoutMode::Narrow => st.ext_open.or(st.ext_pref).unwrap_or(false),
            _ => self.ext_open(st),
        }
    }

    pub fn ringing_extensions(&self) -> usize {
        self.vm
            .snap
            .blf
            .iter()
            .filter(|b| b.state == siple_sip::BlfState::Ringing)
            .count()
    }
}

impl UiState {
    /// Restores the remembered panel choices.
    pub fn restore_panels(&mut self, saved: siple_core::state::GuiPanels) {
        self.lines_pref = saved.lines_collapsed;
        self.lines_expanded = saved.lines_collapsed == Some(false);
        self.ext_pref = saved.extensions_open;
        self.panels_saved = saved;
    }

    /// Explicit rail choice (chevron or `ui.set_panels`).
    pub fn set_lines_expanded(&mut self, expanded: bool) {
        self.lines_expanded = expanded;
        self.lines_pref = Some(!expanded);
    }

    /// Explicit wide / medium extensions panel choice (chevron or `ui.set_panels`).
    pub fn set_ext_panel(&mut self, open: bool) {
        self.ext_open = Some(open);
        self.ext_pref = Some(open);
    }

    /// Applies a `ui.set_panels` request: the wide / medium layouts only; the narrow view is
    /// not switched.
    pub fn apply_panel_request(&mut self, lines_collapsed: Option<bool>, ext_open: Option<bool>) {
        if let Some(c) = lines_collapsed {
            self.set_lines_expanded(!c);
        }
        if let Some(o) = ext_open {
            self.set_ext_panel(o);
        }
    }

    /// The explicit choices that changed since the last call; `None` when nothing is to write.
    pub fn take_panel_save(&mut self) -> Option<siple_core::state::GuiPanels> {
        let differs = |now: Option<bool>, was: Option<bool>| now.filter(|v| Some(*v) != was);
        let out = siple_core::state::GuiPanels {
            lines_collapsed: differs(self.lines_pref, self.panels_saved.lines_collapsed),
            extensions_open: differs(self.ext_pref, self.panels_saved.extensions_open),
        };
        if out == siple_core::state::GuiPanels::default() {
            return None;
        }
        self.panels_saved = siple_core::state::GuiPanels {
            lines_collapsed: self.lines_pref,
            extensions_open: self.ext_pref,
        };
        Some(out)
    }

    /// Routes the answer of an [`UiAction::Rpc`] to the view that asked.
    pub fn deliver(&mut self, reply: rpc::RpcReply, snap: &Snapshot) {
        use rpc::RpcTag as T;
        match reply.tag {
            T::TestDraft | T::WizardAdd => self.wizard.on_reply(reply),
            T::Echo if self.wizard.on_audio_step() => self.wizard.on_reply(reply),
            T::SetPassword(_) if self.wizard.adding() => self.wizard.on_reply(reply),
            _ => self.settings.on_reply(reply, snap),
        }
    }

    /// A control-API call is in flight (the live wrapper keeps repainting to see its reply).
    pub fn busy(&self) -> bool {
        self.settings.busy() || self.wizard.busy()
    }

    /// Whether a live level meter is on screen (the live wrapper then samples it every frame).
    pub fn wants_live_audio(&self) -> bool {
        self.audio_open
            || (self.settings.open && self.settings.section == settings::Section::Audio)
            || self.wizard.on_audio_step()
    }

    /// Assigns slots, forgets gone calls and keeps `selected_line` valid.
    fn sync_lines(&mut self, calls: &[&CallInfo], snap: &Snapshot) {
        let live = |id: &String| calls.iter().any(|c| &c.id == id);
        self.slots.retain(|id, _| live(id));
        self.call_states.retain(|id, _| live(id));
        self.silenced.retain(live);
        self.dismissed_calls
            .retain(|id| snap.calls.iter().any(|c| &c.id == id));
        for c in calls {
            if !self.slots.contains_key(&c.id) {
                let slot = (1..)
                    .find(|n| !self.slots.values().any(|s| s == n))
                    .unwrap_or(1);
                self.slots.insert(c.id.clone(), slot);
                // A call we place takes the stage.
                if c.direction == CallDirection::Outbound && c.state != CallState::Ended {
                    self.selected_line = Some(c.id.clone());
                    self.new_call_open = false;
                }
            }
            // An answered waiting call takes the stage too.
            if self.call_states.get(&c.id) == Some(&CallState::Ringing)
                && c.state == CallState::Active
            {
                self.selected_line = Some(c.id.clone());
            }
            self.call_states.insert(c.id.clone(), c.state);
        }
        // The composer closes with its call: gone, no longer established, or now in a session.
        if let Some(k) = &self.composer {
            let open = calls.iter().any(|c| {
                c.id == k.call_id
                    && matches!(
                        c.state,
                        CallState::Active | CallState::Held | CallState::RemoteHeld
                    )
            }) && !snap.transfers.iter().any(|s| {
                !s.phase.is_terminal()
                    && (s.original == k.call_id || s.consult.as_deref() == Some(k.call_id.as_str()))
            });
            if !open {
                self.composer = None;
            }
        }
        if !self
            .selected_line
            .as_ref()
            .is_some_and(|id| calls.iter().any(|c| &c.id == id))
        {
            self.selected_line = default_selection(calls, snap);
        }
    }
}

/// Selection when the selected line is gone: the active line, else the most
/// recent held one (not resumed), else any other call; a ringing call only when nothing else is
/// left.
fn default_selection(calls: &[&CallInfo], snap: &Snapshot) -> Option<String> {
    let ringing =
        |c: &CallInfo| c.direction == CallDirection::Inbound && c.state == CallState::Ringing;
    let started = |c: &&&CallInfo| c.answered_at.unwrap_or(c.created_at);
    snap.active_line
        .as_ref()
        .filter(|id| calls.iter().any(|c| &c.id == *id))
        .cloned()
        .or_else(|| {
            calls
                .iter()
                .filter(|c| matches!(c.state, CallState::Held | CallState::RemoteHeld))
                .max_by_key(started)
                .map(|c| c.id.clone())
        })
        .or_else(|| {
            calls
                .iter()
                .find(|c| c.state != CallState::Ended && !ringing(c))
                .map(|c| c.id.clone())
        })
        .or_else(|| calls.iter().find(|c| ringing(c)).map(|c| c.id.clone()))
        .or_else(|| calls.first().map(|c| c.id.clone()))
}

/// The transfer session a line belongs to: the live one, else the newest finished one that is
/// still shown (30 s, not dismissed).
pub(crate) fn session_for<'a>(
    cx: &Cx<'a>,
    st: &UiState,
    call_id: &str,
) -> Option<&'a TransferSession> {
    let involved =
        |s: &&TransferSession| s.original == call_id || s.consult.as_deref() == Some(call_id);
    visible_session(cx, st, cx.vm.snap.transfers.iter().filter(involved))
}

/// Newest session worth showing among `sessions`: live ones first.
pub(crate) fn visible_session<'a>(
    cx: &Cx<'a>,
    st: &UiState,
    sessions: impl Iterator<Item = &'a TransferSession>,
) -> Option<&'a TransferSession> {
    sessions
        .filter(|s| {
            !s.phase.is_terminal()
                || (!st.dismissed_sessions.contains(&s.id)
                    && s.ended_at
                        .is_none_or(|e| (cx.vm.now - e).num_seconds() < SESSION_LINGER_SECS))
        })
        .max_by_key(|s| (!s.phase.is_terminal(), s.started_at))
}

// ---- draw -------------------------------------------------------------------------------------

/// Draws the whole window into the root `ui`.
pub fn draw(ui: &mut egui::Ui, vm: &ViewModel, st: &mut UiState) -> Vec<UiAction> {
    let mut out = Vec::new();
    let p = theme::palette(ui);
    let calls: Vec<&CallInfo> = visible_calls(&vm.snap.calls, vm.now)
        .into_iter()
        .filter(|c| !st.dismissed_calls.contains(&c.id))
        .collect();
    st.sync_lines(&calls, vm.snap);
    if vm.snap.ui.density == siple_core::config::Density::Compact {
        let sp = ui.spacing_mut();
        sp.item_spacing.y *= 0.7;
        sp.button_padding.y *= 0.7;
    }
    // Call timers and registration countdowns tick once a second.
    if !calls.is_empty()
        || vm
            .snap
            .accounts
            .iter()
            .any(|a| reg_class(a) == RegClass::Registered)
    {
        ui.ctx().request_repaint_after(Duration::from_secs(1));
    }
    if vm.notice.is_some() {
        ui.ctx().request_repaint_after(NOTICE_TTL);
    }
    let reduce_motion = vm.snap.ui.reduced_motion == siple_core::config::ReducedMotion::On;
    let ringing = calls
        .iter()
        .any(|c| matches!(c.state, CallState::Ringing | CallState::Early))
        || vm
            .snap
            .blf
            .iter()
            .any(|b| b.state == siple_sip::BlfState::Ringing);
    if ringing && !reduce_motion {
        ui.ctx().request_repaint_after(Duration::from_millis(500));
    }
    let cx = Cx {
        p,
        vm,
        calls,
        mode: layout_for(ui.available_width()),
        width: ui.available_width(),
        blink: reduce_motion || blink_on(ui),
        reduce_motion,
    };

    st.ext_shown = cx.panel_state(st);
    if settings::route(ui, &cx, st, &mut out) {
        return out;
    }
    keys::handle(ui, &cx, st, &mut out);
    egui::Panel::top("header")
        .frame(
            egui::Frame::new()
                .fill(p.surface)
                .inner_margin(Margin::symmetric(16, 10))
                .stroke(Stroke::new(1.0, p.border)),
        )
        .show_separator_line(false)
        .show(ui, |ui| header::show(ui, &cx, st, &mut out));
    egui::Panel::bottom("footer")
        .frame(
            egui::Frame::new()
                .fill(p.surface)
                .inner_margin(Margin::symmetric(12, 6))
                .stroke(Stroke::new(1.0, p.border)),
        )
        .show_separator_line(false)
        .show(ui, |ui| footer::show(ui, &cx, st, &mut out));

    let side_frame = |fill| {
        egui::Frame::new()
            .fill(fill)
            .stroke(Stroke::new(1.0, p.border))
            .inner_margin(Margin::same(SP_S as i8 + 2))
    };
    match cx.mode {
        LayoutMode::Wide | LayoutMode::Medium => {
            let rail_w = if !st.lines_expanded {
                rail::COLLAPSED_W
            } else if cx.mode == LayoutMode::Wide {
                240.0
            } else {
                MEDIUM_RAIL_W
            };
            let rail_frame = if st.lines_expanded {
                side_frame(p.surface)
            } else {
                side_frame(p.surface).inner_margin(Margin::symmetric(8, SP_S as i8 + 2))
            };
            egui::Panel::left("rail")
                .exact_size(rail_w)
                .resizable(false)
                .show_separator_line(false)
                .frame(rail_frame)
                .show(ui, |ui| rail::show(ui, &cx, st, &mut out));
            if cx.ext_open(st) {
                egui::Panel::right("extensions")
                    .resizable(true)
                    .default_size(if cx.mode == LayoutMode::Wide {
                        340.0
                    } else {
                        MEDIUM_DRAWER_W
                    })
                    .size_range(280.0..=420.0)
                    .show_separator_line(false)
                    .frame(side_frame(p.bg))
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| blf_panel::show(ui, &cx, st, &mut out));
                    });
            }
            central(ui, &cx, st, &mut out, false);
        }
        LayoutMode::Narrow => {
            egui::Panel::top("line_tabs")
                .frame(
                    egui::Frame::new()
                        .fill(p.surface)
                        .inner_margin(Margin::symmetric(10, 6))
                        .stroke(Stroke::new(1.0, p.border)),
                )
                .show_separator_line(false)
                .show(ui, |ui| rail::tabs(ui, &cx, st, &mut out));
            central(ui, &cx, st, &mut out, cx.ext_open(st));
        }
    }
    out
}

fn central(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>, ext_tab: bool) {
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(cx.p.bg).inner_margin(Margin::ZERO))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui::Frame::new()
                        .inner_margin(Margin::same(SP_L as i8))
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = theme::SP_M;
                            if ext_tab {
                                stage::pinned(ui, cx, st, out);
                                blf_panel::show(ui, cx, st, out);
                            } else {
                                stage::show(ui, cx, st, out);
                            }
                        });
                });
        });
}

// ---- shared helpers ---------------------------------------------------------------------------

pub(crate) fn state_tone(p: &Palette, s: CallState) -> Tone {
    match s {
        CallState::Active => p.ok,
        CallState::Ringing | CallState::Early => p.ring,
        CallState::Dialing => p.warn,
        CallState::Held | CallState::RemoteHeld => p.held,
        CallState::Ended => p.idle,
    }
}

pub(crate) fn display_name(c: &CallInfo) -> &str {
    c.remote_display
        .as_deref()
        .filter(|d| !d.is_empty())
        .unwrap_or(&c.remote_uri)
}

pub(crate) fn blink_on(ui: &egui::Ui) -> bool {
    ui.input(|i| ((i.time * 2.0) as u64).is_multiple_of(2))
}

/// Combo box chevron in the muted color.
pub(crate) fn combo_icon(
    ui: &egui::Ui,
    rect: egui::Rect,
    visuals: &egui::style::WidgetVisuals,
    _open: bool,
) {
    let c = rect.center();
    let s = 3.5;
    let color = visuals.fg_stroke.color.gamma_multiply(0.7);
    ui.painter().line(
        vec![
            c + egui::vec2(-s, -s * 0.5),
            c + egui::vec2(0.0, s * 0.5),
            c + egui::vec2(s, -s * 0.5),
        ],
        Stroke::new(1.5, color),
    );
}

pub fn reg_tone(p: &Palette, c: RegClass) -> Tone {
    match c {
        RegClass::Registered => p.ok,
        RegClass::Registering => p.warn,
        RegClass::Failed => p.err,
        RegClass::Inactive => p.idle,
    }
}

pub(crate) fn reg_pill_label(c: RegClass, a: &AccountStatus) -> &'static str {
    match c {
        RegClass::Registered => "Registered",
        RegClass::Registering if matches!(a.registration, RegState::WaitingForNetwork) => {
            "Waiting for network"
        }
        RegClass::Registering => "Registering",
        RegClass::Failed => "Failed",
        RegClass::Inactive if !a.enabled => "Disabled",
        RegClass::Inactive => "Not registered",
    }
}

/// `2026-10-08T09:12:44.123+02:00` -> `09:12:44`; anything else unchanged.
pub(crate) fn short_time(rfc3339: &str) -> String {
    DateTime::parse_from_rfc3339(rfc3339)
        .map(|t| t.format("%H:%M:%S").to_string())
        .unwrap_or_else(|_| rfc3339.to_string())
}

pub(crate) fn truncate_galley(
    ui: &egui::Ui,
    text: &str,
    size: f32,
    family: egui::FontFamily,
    color: egui::Color32,
    max_w: f32,
) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(
        text.to_string(),
        egui::TextFormat::simple(egui::FontId::new(size, family), color),
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_w);
    ui.painter().layout_job(job)
}

// ---- live wrapper ----------------------------------------------------------------------------

pub struct PhoneUi {
    shared: Arc<Shared>,
    actions: Actions,
    state: UiState,
}

impl PhoneUi {
    pub fn new(shared: Arc<Shared>, actions: Actions) -> Self {
        let mut state = UiState::default();
        state.wizard.dismissed = actions.first_run_completed();
        state.restore_panels(actions.gui_panels());
        Self {
            shared,
            actions,
            state,
        }
    }

    /// Records why the audio layer is off so the audio panel can say so.
    pub fn with_audio_disabled_reason(mut self, reason: Option<&str>) -> Self {
        self.state.audio_disabled_reason = reason.map(str::to_owned);
        self
    }

    /// Puts a validated link target into the dial bar and focuses it. Never dials: the user
    /// confirms with Call.
    pub fn prefill_dial(&mut self, target: &str) {
        self.state.dial_target = target.to_string();
        self.state.new_call_open = true;
        self.state.focus_dial = true;
    }

    /// Applies `ui.set_panels` requests and saves changed panel choices. Driven from the shell's
    /// `logic` too, because `ui` does not run while the window is hidden or minimized.
    pub fn sync_panels(&mut self) {
        let (lines, ext) = self.shared.take_panel_requests();
        self.state.apply_panel_request(lines, ext);
        if let Some(save) = self.state.take_panel_save() {
            self.actions.set_gui_panels(save);
        }
    }

    /// Opens the Settings page (tray / app menu "Settings…").
    pub fn open_settings(&mut self) {
        self.state.settings.open = true;
    }

    fn dispatch(&mut self, a: UiAction) {
        let act = &self.actions;
        match a {
            UiAction::Answer(id) => act.answer(&id),
            UiAction::AnswerHold(id) => act.answer_with(&id, AnswerMode::HoldOthers),
            UiAction::AnswerEnd(id, end) => act.answer_with(&id, AnswerMode::EndOthers(end)),
            UiAction::Reject(id) => act.reject(&id),
            // UI-only for now: the banner is silenced in the view state.
            UiAction::Silence(_) => {}
            UiAction::Activate(id) => act.activate(&id),
            UiAction::Hangup(id) => act.hangup(&id),
            UiAction::Mute(id, on) => act.mute(&id, on),
            UiAction::Hold(id, on) => act.hold(&id, on),
            UiAction::Dtmf(id, d) => act.dtmf(&id, d),
            UiAction::TransferBlind(id, target) => act.transfer_blind(&id, &target),
            UiAction::ConsultStart(id, target) => act.consult_start(&id, &target),
            UiAction::ConsultLink(id, other) => act.consult_link(&id, &other),
            UiAction::TransferComplete(id, mode) => act.transfer_complete(&id, mode),
            UiAction::TransferSwap(id) => act.transfer_swap(&id),
            UiAction::TransferCancel(id) => act.transfer_cancel(&id),
            UiAction::Dial(acc, target) => act.dial(&acc, &target),
            UiAction::BlfDial(acc, uri) => act.blf_dial(&acc, &uri),
            UiAction::Pickup(acc, uri) => act.pickup(&acc, &uri),
            UiAction::SetDnd(acc, on) => act.set_dnd(&acc, on),
            UiAction::TestSpeaker => act.test_speaker(),
            UiAction::SetDevices(sel) => act.set_devices(&sel),
            UiAction::SetAccountEnabled(id, on) => act.set_account_enabled(&id, on),
            UiAction::OpenConfig => {
                if let Err(e) = open_path(&self.shared.config_path, true) {
                    self.shared.set_notice(format!("Cannot open config: {e}"));
                }
            }
            UiAction::OpenLogs => {
                if let Some(dir) = &self.shared.log_dir
                    && let Err(e) = open_path(dir, false)
                {
                    self.shared.set_notice(format!("Cannot open logs: {e}"));
                }
            }
            UiAction::Rpc(req) => act.rpc(req),
            UiAction::FirstRunDone => act.set_first_run_completed(),
            UiAction::RequestMicAccess => act.request_mic_access(),
            #[cfg(not(feature = "store"))]
            UiAction::OpenUrl(url) => {
                if let Err(e) = open_url(&url) {
                    self.shared.set_notice(format!("Cannot open the link: {e}"));
                }
            }
            #[cfg(not(feature = "store"))]
            UiAction::SkipUpdate(v) => act.skip_update(&v),
            UiAction::LogsVisible(open) => {
                self.shared.want_logs.store(open, Ordering::Relaxed);
                self.shared.wake.notify_one();
            }
        }
    }
}

impl eframe::App for PhoneUi {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let snap = self.shared.snapshot();
        let notice = self.shared.notice(NOTICE_TTL);
        self.shared.ctx.get_or_init(|| ui.ctx().clone());
        ui.ctx().set_theme(match snap.ui.theme {
            siple_core::config::Theme::System => egui::ThemePreference::System,
            siple_core::config::Theme::Light => egui::ThemePreference::Light,
            siple_core::config::Theme::Dark => egui::ThemePreference::Dark,
        });
        for reply in self.shared.take_replies() {
            self.state.deliver(reply, &snap);
        }
        self.sync_panels();
        let live = if self.state.wants_live_audio() {
            LiveAudio {
                mic_dbfs: self.actions.mic_level(),
                echo: self.actions.echo_status(),
            }
        } else {
            LiveAudio::default()
        };
        if self.state.busy() {
            ui.ctx().request_repaint_after(Duration::from_millis(120));
        }
        let vm = ViewModel {
            snap: &snap,
            now: Utc::now(),
            config_path: &self.shared.config_path,
            has_log_dir: self.shared.log_dir.is_some(),
            notice: notice.as_deref(),
            live,
        };
        let actions = draw(ui, &vm, &mut self.state);
        for a in actions {
            self.dispatch(a);
        }
        self.sync_panels();
        self.shared
            .publish_panels(!self.state.lines_expanded, self.state.ext_shown);
    }
}

// ---- start error window ----------------------------------------------------------------------

/// Standalone window shown when the app cannot start (second instance, bad config).
pub struct ErrorUi {
    pub title: String,
    pub detail: String,
}

impl eframe::App for ErrorUi {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let p = theme::palette(ui);
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(p.bg)
                    .inner_margin(Margin::same(SP_L as i8)),
            )
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = SP_S;
                ui.horizontal(|ui| {
                    ui.label(icon_fill(ic::WARNING_CIRCLE, 22.0).color(p.err.led));
                    ui.label(strong(&self.title, FS_TITLE + 1.0).color(p.text));
                });
                ui.add(
                    egui::Label::new(RichText::new(&self.detail).size(FS_BODY).color(p.muted))
                        .wrap(),
                );
                ui.add_space(SP_S);
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                    if ui
                        .add(
                            egui::Button::new(strong("Quit", FS_BODY).color(p.on_accent))
                                .fill(p.accent)
                                .stroke(Stroke::NONE)
                                .corner_radius(CornerRadius::same(R_MD))
                                .min_size(egui::vec2(88.0, 30.0)),
                        )
                        .clicked()
                    {
                        ui.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
    }
}

#[cfg(test)]
pub(crate) fn meter_segments_for_tests(db: f32) -> usize {
    audio::meter_segments(db)
}

#[cfg(test)]
pub(crate) fn wizard_needed_for_tests(snap: &Snapshot) -> bool {
    wizard::needs_setup(snap)
}
