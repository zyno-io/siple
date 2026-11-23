//! Interaction checks use the existing egui harness without rendering snapshots or a GPU.
use std::path::Path;

use eframe::egui;
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use siple_core::config::{
    BlfClick, BlfClickInCall, BlfClickRinging, BlfGroupBy, BlfView, TransferEnter,
};
use siple_core::update::{SourceName, UpdateAction};
use siple_sip::{BlfState, CallDirection, CallState, CompleteMode, PickupMode, TransferPhase};

use crate::snapshot::Snapshot;
use crate::ui::{UiAction, UiState, ViewModel, draw};
use crate::visual_tests::{
    base_snapshot, blf, blf_many, call, call_waiting, now, session, transfer_consult, two_calls,
};

struct State {
    snap: Snapshot,
    ui: UiState,
    actions: Vec<UiAction>,
    live: crate::ui::LiveAudio,
    notice: Option<crate::snapshot::Notice>,
}

impl eframe::App for State {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let vm = ViewModel {
            snap: &self.snap,
            now: now(),
            config_path: Path::new("config.toml"),
            has_log_dir: false,
            notice: self.notice.as_ref(),
            live: self.live.clone(),
        };
        self.actions.extend(draw(ui, &vm, &mut self.ui));
    }
}

fn harness(snap: Snapshot, size: egui::Vec2) -> Harness<'static, State> {
    // The first-run flag is already recorded: no wizard decision (and no action) on frame one.
    let mut ui = UiState::default();
    ui.wizard.dismissed = true;
    harness_ui(snap, size, ui)
}

fn harness_ui(snap: Snapshot, size: egui::Vec2, ui: UiState) -> Harness<'static, State> {
    Harness::builder().with_size(size).build_eframe(|cc| {
        crate::theme::install(&cc.egui_ctx);
        State {
            snap,
            ui,
            actions: Vec::new(),
            live: Default::default(),
            notice: None,
        }
    })
}

/// Like [`harness`] with the extensions pane / drawer / tab open (the narrow layout then shows
/// the extensions instead of the stage).
fn harness_ext(snap: Snapshot, size: egui::Vec2) -> Harness<'static, State> {
    let mut h = harness(snap, size);
    h.state_mut().ui.ext_open = Some(true);
    h.state_mut().ui.ext_tab = true;
    h
}

const WIDE: egui::Vec2 = egui::vec2(1240.0, 760.0);
const MEDIUM: egui::Vec2 = egui::vec2(900.0, 760.0);
/// Medium with room for the drawer and a 440 px stage.
const MEDIUM_ROOMY: egui::Vec2 = egui::vec2(1000.0, 760.0);
const NARROW: egui::Vec2 = egui::vec2(420.0, 980.0);

fn click_first(h: &mut Harness<'_, State>, label: &str) {
    h.query_all_by_label_contains(label)
        .next()
        .unwrap_or_else(|| panic!("no widget labelled {label:?}"))
        .click_accesskit();
    // Spinners keep repainting, so run a few steps instead of until idle.
    h.run_steps(4);
}

/// The Lines rail is on screen, collapsed or expanded.
fn rail_shown(h: &Harness<'_, State>) -> bool {
    h.query_by_label("LINES").is_some() || h.query_by_label("Expand lines").is_some()
}

fn take(h: &mut Harness<'_, State>) -> Vec<UiAction> {
    std::mem::take(&mut h.state_mut().actions)
}

// ---- layout ---------------------------------------------------------------------------------

#[test]
fn breakpoints_select_the_layout() {
    use crate::ui::{LayoutMode, layout_for};
    assert_eq!(layout_for(1240.0), LayoutMode::Wide);
    assert_eq!(layout_for(1180.0), LayoutMode::Wide);
    assert_eq!(layout_for(1179.0), LayoutMode::Medium);
    assert_eq!(layout_for(760.0), LayoutMode::Medium);
    assert_eq!(layout_for(759.0), LayoutMode::Narrow);
}

#[test]
fn wide_shows_three_panes_medium_a_drawer_narrow_tabs() {
    // Wide: rail, stage and the extensions pane.
    let mut h = harness(two_calls(), WIDE);
    h.run();
    assert!(rail_shown(&h));
    assert!(h.query_by_label("EXTENSIONS").is_some());
    // Medium: rail and stage; at 780 px the drawer would leave the stage under 440 px, so it
    // starts collapsed and the header chevron opens it.
    let mut h = harness(two_calls(), egui::vec2(780.0, 760.0));
    h.run();
    assert!(rail_shown(&h));
    assert!(h.query_by_label("EXTENSIONS").is_none());
    h.get_by_label_contains("Extensions").click_accesskit();
    h.run();
    assert!(h.query_by_label("EXTENSIONS").is_some());
    // A roomy medium window opens it by default; the chevron collapses it.
    let mut h = harness(two_calls(), MEDIUM_ROOMY);
    h.run();
    assert!(rail_shown(&h));
    assert!(h.query_by_label("EXTENSIONS").is_some());
    h.get_by_label_contains("Extensions").click_accesskit();
    h.run();
    assert!(h.query_by_label("EXTENSIONS").is_none());
    assert!(rail_shown(&h));
    // Narrow: one column with line tabs; no rail.
    let mut h = harness(two_calls(), NARROW);
    h.run();
    assert!(!rail_shown(&h));
    assert!(h.query_by_label("EXTENSIONS").is_none());
    assert!(h.query_by_label_contains("Line 1, Bob Martin").is_some());
    assert!(h.query_by_label_contains("Line 2,").is_some());
    h.get_by_label_contains("Extensions").click_accesskit();
    h.run();
    assert!(h.query_by_label("EXTENSIONS").is_some());
}

// ---- update pill ------------------------------------------------------------------------------

const RELEASE_URL: &str = "https://github.com/zyno-io/siple/releases/tag/v0.2.0";

fn update_status(latest: &str, available: bool) -> siple_core::update::UpdateStatus {
    siple_core::update::UpdateStatus {
        enabled: true,
        channel: "stable".into(),
        current: "0.1.0".into(),
        latest: Some(latest.into()),
        available,
        source: SourceName::Github,
        feed: None,
        action: if available {
            UpdateAction::OpenReleasePage
        } else {
            UpdateAction::None
        },
        release_url: Some(RELEASE_URL.into()),
        store_url: None,
        disabled_reason: None,
        mandatory: None,
        published_at: Some("2026-11-01T10:00:00Z".into()),
        checked_at: Some("2026-11-17T09:00:00Z".into()),
        error: None,
        install: None,
    }
}

#[test]
fn update_pill_opens_the_release_and_skip_hides_that_version() {
    let mut snap = base_snapshot();
    snap.update = update_status("0.2.0", true);
    let mut h = harness(snap, WIDE);
    h.run();
    assert!(h.query_by_label("Siple 0.2.0 is available").is_some());
    // Download and Release notes both open the release page.
    for item in ["Download", "Release notes"] {
        h.get_by_label("Siple 0.2.0 is available").click_accesskit();
        h.run();
        h.get_by_label(item).click_accesskit();
        h.run();
        assert_eq!(
            take(&mut h),
            [UiAction::OpenUrl(RELEASE_URL.into())],
            "{item}"
        );
    }
    // Skip this version: the action is emitted and, once the engine has recorded it, the pill is gone.
    h.get_by_label("Siple 0.2.0 is available").click_accesskit();
    h.run();
    h.get_by_label("Skip this version").click_accesskit();
    h.run();
    assert_eq!(take(&mut h), [UiAction::SkipUpdate("0.2.0".into())]);
    h.state_mut().snap.update_skipped = Some("0.2.0".into());
    h.run();
    assert!(h.query_by_label_contains("is available").is_none());
    // A newer release is offered again.
    h.state_mut().snap.update = update_status("0.3.0", true);
    h.run();
    assert!(h.query_by_label("Siple 0.3.0 is available").is_some());
}

/// The App Store listing says `latest`; the pill opens the store page, never a URL.
fn app_store_status(latest: &str) -> siple_core::update::UpdateStatus {
    siple_core::update::UpdateStatus {
        source: SourceName::AppStore,
        feed: None,
        action: UpdateAction::OpenStore,
        release_url: None,
        store_url: Some(siple_core::update::links::MAS_STORE_URL.into()),
        ..update_status(latest, true)
    }
}

/// The Microsoft Store says only that an update is waiting.
fn ms_store_status(mandatory: bool) -> siple_core::update::UpdateStatus {
    siple_core::update::UpdateStatus {
        source: SourceName::MicrosoftStore,
        feed: None,
        action: UpdateAction::OpenStore,
        latest: None,
        release_url: None,
        published_at: None,
        store_url: Some(siple_core::update::links::MS_STORE_URL.into()),
        mandatory: Some(mandatory),
        ..update_status("", true)
    }
}

#[test]
fn app_store_pill_opens_the_store_page_and_can_skip() {
    let mut snap = base_snapshot();
    snap.update = app_store_status("0.2.0");
    let mut h = harness(snap, WIDE);
    h.run();
    h.get_by_label("Siple 0.2.0 is available").click_accesskit();
    h.run();
    assert!(h.query_by_label("Download").is_none());
    h.get_by_label("Open App Store").click_accesskit();
    h.run();
    assert_eq!(take(&mut h), [UiAction::OpenStorePage]);
    h.get_by_label("Siple 0.2.0 is available").click_accesskit();
    h.run();
    h.get_by_label("Skip this version").click_accesskit();
    h.run();
    assert_eq!(take(&mut h), [UiAction::SkipUpdate("0.2.0".into())]);
}

#[test]
fn microsoft_store_pill_has_no_version_and_not_now_lasts_the_session() {
    let mut snap = base_snapshot();
    snap.update = ms_store_status(false);
    let mut h = harness(snap, WIDE);
    h.run();
    h.get_by_label("A Siple update is available")
        .click_accesskit();
    h.run();
    assert!(h.query_by_label("Skip this version").is_none());
    h.get_by_label("Open Microsoft Store").click_accesskit();
    h.run();
    assert_eq!(take(&mut h), [UiAction::OpenStorePage]);
    h.get_by_label("A Siple update is available")
        .click_accesskit();
    h.run();
    h.get_by_label("Not now").click_accesskit();
    h.run();
    assert!(take(&mut h).is_empty());
    assert!(h.query_by_label_contains("update is available").is_none());
}

#[test]
fn a_mandatory_microsoft_store_update_cannot_be_dismissed() {
    let mut snap = base_snapshot();
    snap.update = ms_store_status(true);
    let mut h = harness(snap, WIDE);
    h.state_mut().ui.update_not_now = true;
    h.run();
    h.get_by_label("A Siple update is available")
        .click_accesskit();
    h.run();
    assert!(h.query_by_label("Not now").is_none());
    assert!(h.query_by_label("Open Microsoft Store").is_some());
}

#[test]
fn an_install_action_is_not_offered_yet() {
    let mut snap = base_snapshot();
    snap.update = siple_core::update::UpdateStatus {
        action: UpdateAction::Install,
        ..update_status("0.2.0", true)
    };
    assert_eq!(snap.update_offer(), None);
}

#[test]
fn settings_of_a_store_edition_name_the_store_and_hide_the_channel() {
    for (status, button) in [
        (app_store_status("0.2.0"), "Open App Store"),
        (ms_store_status(false), "Open Microsoft Store"),
    ] {
        let mut snap = base_snapshot();
        snap.update = status;
        let mut h = harness(snap, WIDE);
        h.state_mut().ui.settings.open = true;
        h.state_mut().ui.settings.section = crate::ui::settings::Section::Advanced;
        h.run();
        assert!(h.query_by_label("Channel").is_none());
        h.get_by_label(button).click_accesskit();
        h.run();
        assert_eq!(take(&mut h), [UiAction::OpenStorePage], "{button}");
    }
}

#[test]
fn settings_without_a_source_say_so() {
    let mut snap = base_snapshot();
    snap.update = siple_core::update::UpdateStatus {
        source: SourceName::None,
        feed: None,
        disabled_reason: Some(siple_core::update::DisabledReason::Sideloaded),
        enabled: false,
        ..update_status("", false)
    };
    let mut h = harness(snap, WIDE);
    h.state_mut().ui.settings.open = true;
    h.state_mut().ui.settings.section = crate::ui::settings::Section::Advanced;
    h.run();
    assert!(
        h.query_by_label("Not available").is_some()
            || h.query_by_label_contains("Not available").is_some()
    );
    assert!(h.query_by_label("Channel").is_none());
}

#[test]
fn settings_of_the_flatpak_edition_say_flatpak_updates_it() {
    let mut snap = base_snapshot();
    snap.update = siple_core::update::UpdateStatus {
        source: SourceName::None,
        feed: None,
        disabled_reason: Some(siple_core::update::DisabledReason::ManagedByFlatpak),
        enabled: false,
        ..update_status("", false)
    };
    let mut h = harness(snap, WIDE);
    h.state_mut().ui.settings.open = true;
    h.state_mut().ui.settings.section = crate::ui::settings::Section::Advanced;
    h.run();
    assert!(
        h.query_by_label("Updated by Flatpak").is_some()
            || h.query_by_label_contains("Updated by Flatpak").is_some()
    );
    assert!(h.query_by_label("Channel").is_none());
}

#[test]
fn no_update_pill_when_up_to_date_disabled_or_not_checked() {
    let mut up_to_date = base_snapshot();
    up_to_date.update = update_status("0.1.0", false);
    let mut off = base_snapshot();
    off.update = update_status("0.2.0", true);
    off.update.enabled = false;
    for snap in [up_to_date, off, base_snapshot()] {
        let mut h = harness(snap, WIDE);
        h.run();
        assert!(h.query_by_label_contains("is available").is_none());
        assert!(h.query_by_label_contains("Update 0").is_none());
    }
}

#[test]
fn settings_show_version_and_check_for_updates() {
    let mut snap = base_snapshot();
    snap.update = update_status("0.2.0", true);
    let mut h = harness(snap, WIDE);
    h.state_mut().ui.settings.open = true;
    h.state_mut().ui.settings.section = crate::ui::settings::Section::Advanced;
    h.run();
    assert!(
        h.query_by_label_contains("Siple 0.2.0 is available")
            .is_some()
    );
    h.get_by_label("Check now").click_accesskit();
    h.run();
    let acts = take(&mut h);
    assert!(
        acts.iter()
            .any(|a| matches!(a, UiAction::Rpc(r) if r.method == "app.check_updates")),
        "{acts:?}"
    );
}

// ---- in-place install states --------------------------------------------------------------

fn install_status(state: siple_core::update::InstallPhase) -> siple_core::update::UpdateStatus {
    siple_core::update::UpdateStatus {
        action: UpdateAction::Install,
        install: Some(siple_core::update::InstallStatus {
            possible: true,
            reason: None,
            detail: None,
            state,
            version: Some("0.2.0".into()),
            bytes: None,
            total: None,
            restart: None,
            error: None,
            error_kind: None,
        }),
        ..update_status("0.2.0", true)
    }
}

fn install_calls(h: &mut Harness<'_, State>) -> Vec<(String, serde_json::Value)> {
    take(h)
        .into_iter()
        .filter_map(|a| match a {
            UiAction::Rpc(r) => Some((r.method.to_string(), r.params)),
            _ => None,
        })
        .collect()
}

#[test]
fn the_pill_starts_an_install_and_asks_to_restart_when_idle() {
    use siple_core::update::InstallPhase as P;
    let mut snap = base_snapshot();
    snap.update = install_status(P::Idle);
    let mut h = harness(snap, WIDE);
    h.run();
    h.get_by_label("Siple 0.2.0 is available").click_accesskit();
    h.run();
    assert!(h.query_by_label("Release notes").is_some());
    h.get_by_label("Install and restart").click_accesskit();
    h.run();
    assert_eq!(
        install_calls(&mut h),
        [(
            "app.install_update".to_string(),
            serde_json::json!({"version": "0.2.0", "restart": "when_idle"})
        )]
    );
    assert_eq!(h.state().ui.update_initiated.as_deref(), Some("0.2.0"));
}

#[test]
fn during_a_call_the_install_waits_for_a_manual_restart() {
    use siple_core::update::InstallPhase as P;
    let mut snap = two_calls();
    snap.update = install_status(P::Idle);
    let mut h = harness(snap, WIDE);
    h.run();
    h.get_by_label("Siple 0.2.0 is available").click_accesskit();
    h.run();
    h.get_by_label("Install, restart after the call")
        .click_accesskit();
    h.run();
    assert_eq!(
        install_calls(&mut h),
        [(
            "app.install_update".to_string(),
            serde_json::json!({"version": "0.2.0", "restart": "manual"})
        )]
    );
}

#[test]
fn progress_shows_a_percentage_and_cancel() {
    use siple_core::update::InstallPhase as P;
    let mut snap = base_snapshot();
    snap.update = install_status(P::Downloading);
    let i = snap.update.install.as_mut().unwrap();
    (i.bytes, i.total) = (Some(450), Some(1000));
    let mut ui = UiState::default();
    ui.wizard.dismissed = true;
    ui.update_initiated = Some("0.2.0".into());
    let mut h = harness_ui(snap, WIDE, ui);
    h.run();
    h.get_by_label("Updating 45%").click_accesskit();
    h.run();
    assert!(h.query_by_label_contains("An agent started").is_none());
    h.get_by_label("Cancel").click_accesskit();
    h.run();
    assert_eq!(
        install_calls(&mut h),
        [("app.cancel_update".to_string(), serde_json::json!({}))]
    );
}

#[test]
fn ready_restarts_now_but_not_during_a_call() {
    use siple_core::update::InstallPhase as P;
    let mut snap = base_snapshot();
    snap.update = install_status(P::Ready);
    let mut ui = UiState::default();
    ui.wizard.dismissed = true;
    ui.update_initiated = Some("0.2.0".into());
    let mut h = harness_ui(snap, WIDE, ui);
    h.run();
    h.get_by_label("Restart to update").click_accesskit();
    h.run();
    // The pill and the menu item carry the same text; the item is the second one.
    let item = h.get_all_by_label("Restart to update").last().unwrap();
    item.click_accesskit();
    h.run();
    assert_eq!(
        install_calls(&mut h),
        [(
            "app.install_update".to_string(),
            serde_json::json!({"version": "0.2.0", "restart": "now"})
        )]
    );
    // A call: the item is disabled and nothing is sent.
    let mut snap = two_calls();
    snap.update = install_status(P::Ready);
    let mut ui = UiState::default();
    ui.wizard.dismissed = true;
    ui.update_initiated = Some("0.2.0".into());
    let mut h = harness_ui(snap, WIDE, ui);
    h.run();
    h.get_by_label("Restart to update").click_accesskit();
    h.run();
    assert!(
        h.query_by_label_contains("restarts when the call is over")
            .is_some()
    );
    let item = h.get_all_by_label("Restart to update").last().unwrap();
    item.click_accesskit();
    h.run();
    assert!(install_calls(&mut h).is_empty());
}

#[test]
fn an_install_started_by_an_agent_is_labelled_and_can_be_cancelled() {
    use siple_core::update::InstallPhase as P;
    let mut snap = base_snapshot();
    snap.update = install_status(P::Ready);
    let mut h = harness(snap, WIDE);
    h.run();
    h.get_by_label("Restart pending (agent)").click_accesskit();
    h.run();
    assert!(
        h.query_by_label_contains("An agent started this update")
            .is_some()
    );
    h.get_by_label("Cancel").click_accesskit();
    h.run();
    assert_eq!(
        install_calls(&mut h),
        [("app.cancel_update".to_string(), serde_json::json!({}))]
    );
}

#[test]
fn a_failed_install_offers_a_retry_and_the_download() {
    use siple_core::update::InstallPhase as P;
    let mut snap = base_snapshot();
    snap.update = install_status(P::Failed);
    snap.update.install.as_mut().unwrap().error =
        Some("the download does not match its checksum".into());
    let mut h = harness(snap, WIDE);
    h.run();
    h.get_by_label("Update failed").click_accesskit();
    h.run();
    assert!(
        h.query_by_label_contains("does not match its checksum")
            .is_some()
    );
    h.get_by_label("Download from GitHub").click_accesskit();
    h.run();
    assert_eq!(take(&mut h), [UiAction::OpenUrl(RELEASE_URL.into())]);
    h.get_by_label("Update failed").click_accesskit();
    h.run();
    h.get_by_label("Try again").click_accesskit();
    h.run();
    assert_eq!(install_calls(&mut h).len(), 1);
}

#[test]
fn restarting_has_no_buttons_and_a_failed_relaunch_says_to_restart_by_hand() {
    use siple_core::update::InstallPhase as P;
    let mut snap = base_snapshot();
    snap.update = install_status(P::Restarting);
    let mut ui = UiState::default();
    ui.wizard.dismissed = true;
    ui.update_initiated = Some("0.2.0".into());
    let mut h = harness_ui(snap, WIDE, ui);
    h.run();
    h.get_by_label("Restarting").click_accesskit();
    h.run();
    assert!(
        h.query_by_label("Cancel").is_none() && h.query_by_label("Skip this version").is_none()
    );
    let mut snap = base_snapshot();
    snap.update = install_status(P::Installed);
    snap.update.install.as_mut().unwrap().error =
        Some("the new version is installed; restart Siple to use it".into());
    let mut h = harness(snap, WIDE);
    h.run();
    h.get_by_label("Restart to finish").click_accesskit();
    h.run();
    assert!(
        h.query_by_label_contains("restart Siple to use it")
            .is_some()
    );
}

#[test]
fn without_an_install_the_pill_gives_the_reason_and_the_release_page() {
    let mut snap = base_snapshot();
    snap.update = update_status("0.2.0", true);
    snap.update.install = Some(siple_core::update::InstallStatus {
        possible: false,
        reason: Some("package_manager".into()),
        detail: None,
        state: siple_core::update::InstallPhase::Idle,
        version: None,
        bytes: None,
        total: None,
        restart: None,
        error: None,
        error_kind: None,
    });
    let mut h = harness(snap, WIDE);
    h.run();
    h.get_by_label("Siple 0.2.0 is available").click_accesskit();
    h.run();
    assert!(h.query_by_label("Install and restart").is_none());
    assert!(h.query_by_label_contains("package manager").is_some());
    h.get_by_label("Download").click_accesskit();
    h.run();
    assert_eq!(take(&mut h), [UiAction::OpenUrl(RELEASE_URL.into())]);
}

#[test]
fn settings_have_the_install_switch_and_start_the_install() {
    use siple_core::update::InstallPhase as P;
    let mut snap = base_snapshot();
    snap.update = install_status(P::Idle);
    let mut h = harness(snap, WIDE);
    h.state_mut().ui.settings.open = true;
    h.state_mut().ui.settings.section = crate::ui::settings::Section::Advanced;
    h.run();
    assert!(
        h.query_all_by_label_contains("Install updates in the app")
            .next()
            .is_some()
    );
    h.get_by_label("Install and restart").click_accesskit();
    h.run();
    assert_eq!(install_calls(&mut h).len(), 1);
}

/// Toggle hints wrap under their label instead of running off the right edge of the window.
#[test]
fn settings_toggle_hints_stay_inside_the_window() {
    use siple_core::update::InstallPhase as P;
    for size in [WIDE, MEDIUM, NARROW] {
        let mut snap = base_snapshot();
        snap.update = install_status(P::Idle);
        let mut h = harness(snap, size);
        h.state_mut().ui.settings.open = true;
        h.state_mut().ui.settings.section = crate::ui::settings::Section::Advanced;
        h.run();
        let hint = h
            .query_all_by_label_contains("checks its signature")
            .next()
            .expect("the install hint");
        let r = hint.rect();
        assert!(
            r.max.x <= size.x + 0.5,
            "{size:?}: hint ends at {} past the window ({r:?})",
            r.max.x
        );
        // The hint sits right under its own row (the switch and its label text share the
        // label), not a full row gap away.
        let row_bottom = h
            .query_all_by_label("Install updates in the app")
            .map(|n| n.rect().max.y)
            .fold(f32::NEG_INFINITY, f32::max);
        let gap = r.min.y - row_bottom;
        assert!(
            (0.0..=8.0).contains(&gap),
            "{size:?}: hint is {gap} px below its toggle row (ends at {row_bottom}; hint {r:?})"
        );
    }
}

/// Launch at login is a switch in the tray and window group; it emits the config edit.
#[test]
fn launch_at_login_toggle_emits_the_config_edit() {
    for size in [WIDE, NARROW] {
        let mut h = settings_harness(base_snapshot(), Section::Notifications, size);
        click_switch(&mut h, "Start Siple when you log in");
        let req = only_edit(take(&mut h));
        assert_eq!(
            rpc::edits_of(&req),
            [ConfigEdit::SetSection {
                section: "ui".into(),
                key: "launch_at_login".into(),
                value: EditValue::Bool(true)
            }]
        );
        // The hint wraps inside the window.
        let hint = h
            .query_all_by_label_contains("Combine with Start hidden")
            .next()
            .expect("the launch-at-login hint");
        assert!(
            hint.rect().max.x <= size.x + 0.5,
            "{size:?}: hint past the window ({:?})",
            hint.rect()
        );
    }
}

#[test]
fn header_badge_counts_ringing_extensions() {
    let mut h = harness(base_snapshot(), MEDIUM);
    h.run();
    assert!(h.query_by_label("Extensions, 1 ringing").is_some());
}

// ---- rail -----------------------------------------------------------------------------------

#[test]
fn rail_click_on_a_held_line_activates_it() {
    for size in [WIDE, MEDIUM, NARROW] {
        let mut h = harness(two_calls(), size);
        h.run();
        // The active line (c1) is selected by default.
        assert_eq!(h.state().ui.selected_line.as_deref(), Some("c1"));
        // Clicking the active line is a no-op.
        h.get_by_label_contains("Line 1, Bob Martin")
            .click_accesskit();
        h.run();
        assert!(h.state().actions.is_empty(), "{:?}", h.state().actions);
        // Clicking the held line: select it and `call.activate`.
        h.get_by_label_contains("Line 2,").click_accesskit();
        h.run();
        assert_eq!(h.state().actions, [UiAction::Activate("c2".into())]);
        assert_eq!(h.state().ui.selected_line.as_deref(), Some("c2"));
    }
}

#[test]
fn rail_click_on_a_ringing_line_only_selects_it() {
    let mut h = harness(call_waiting(), WIDE);
    h.run();
    h.get_by_label_contains("Line 3,").click_accesskit();
    h.run();
    assert!(h.state().actions.is_empty());
    assert_eq!(h.state().ui.selected_line.as_deref(), Some("c3"));
}

#[test]
fn slots_are_stable_and_reused() {
    let mut h = harness(two_calls(), WIDE);
    h.run();
    assert_eq!(h.state().ui.slots.get("c1"), Some(&1));
    assert_eq!(h.state().ui.slots.get("c2"), Some(&2));
    // L1 ends and is gone; the next new call takes the lowest free slot.
    h.state_mut().snap.calls.remove(0);
    h.state_mut().snap.active_line = None;
    h.run();
    let mut w = call(
        "c9",
        CallDirection::Inbound,
        CallState::Ringing,
        ("New", "sip:1@x"),
    );
    w.created_at = now();
    h.state_mut().snap.calls.push(w);
    h.run();
    assert_eq!(h.state().ui.slots.get("c2"), Some(&2));
    assert_eq!(h.state().ui.slots.get("c9"), Some(&1));
    // A ringing call never takes the stage while another line exists.
    assert_eq!(h.state().ui.selected_line.as_deref(), Some("c2"));
}

#[test]
fn new_call_collapses_into_a_button_while_lines_exist() {
    let mut h = harness(two_calls(), WIDE);
    h.run();
    assert!(h.query_by_label("Number or SIP URI").is_none());
    click_first(&mut h, "New call");
    assert!(h.state().ui.new_call_open);
    // Idle stage: the dial field is there from the start.
    let mut idle = harness(base_snapshot(), WIDE);
    idle.run();
    assert!(
        idle.query_by_role(egui::accesskit::Role::TextInput)
            .is_some(),
        "idle stage has the dial field"
    );
}

// ---- collapsible rail -------------------------------------------------------------------------

#[test]
fn rail_is_collapsed_by_default_when_wide_and_medium() {
    for size in [WIDE, MEDIUM] {
        let mut h = harness(two_calls(), size);
        h.run();
        assert!(h.query_by_label("LINES").is_none());
        assert!(h.query_by_label("Expand lines").is_some());
        assert!(h.query_by_label("New call").is_some());
        // One badge per line, with the row's accessible name.
        assert!(h.query_by_label_contains("Line 1, Bob Martin").is_some());
        assert!(h.query_by_label_contains("Line 2,").is_some());
        assert!(!h.state().ui.lines_expanded);
    }
}

#[test]
fn rail_expands_and_collapses_and_flags_the_choice_for_saving() {
    let mut h = harness(two_calls(), WIDE);
    h.run();
    h.get_by_label("Expand lines").click_accesskit();
    h.run();
    assert!(h.query_by_label("LINES").is_some());
    assert!(h.query_by_label("Expand lines").is_none());
    assert!(h.state().ui.lines_expanded);
    assert_eq!(
        h.state_mut().ui.take_panel_save().unwrap().lines_collapsed,
        Some(false)
    );
    assert!(h.query_all_by_label_contains("New call").next().is_some());
    h.get_by_label("Collapse lines").click_accesskit();
    h.run();
    assert!(h.query_by_label("LINES").is_none());
    assert!(h.query_by_label("Expand lines").is_some());
    assert!(!h.state().ui.lines_expanded);
    assert_eq!(
        h.state_mut().ui.take_panel_save().unwrap().lines_collapsed,
        Some(true)
    );
}

#[test]
fn collapsed_badge_selects_its_line_and_plus_opens_the_dial_bar() {
    let mut h = harness(two_calls(), WIDE);
    h.run();
    assert_eq!(h.state().ui.selected_line.as_deref(), Some("c1"));
    h.get_by_label_contains("Line 2,").click_accesskit();
    h.run();
    assert_eq!(h.state().ui.selected_line.as_deref(), Some("c2"));
    // Same as a row click: a held line becomes the active line.
    assert_eq!(h.state().actions, [UiAction::Activate("c2".into())]);
    assert!(h.query_by_label("Number or SIP URI").is_none());
    h.get_by_label("New call").click_accesskit();
    h.run();
    assert!(h.state().ui.new_call_open);
    assert!(h.query_by_label("Number or SIP URI").is_some());
}

#[test]
fn saved_panel_choices_are_restored() {
    let mut ui = UiState::default();
    ui.wizard.dismissed = true;
    ui.lines_expanded = true;
    ui.ext_pref = Some(false);
    let mut h = harness_ui(two_calls(), WIDE, ui);
    h.run();
    assert!(h.query_by_label("LINES").is_some());
    assert!(h.query_by_label("EXTENSIONS").is_none());
    assert!(!h.state().ui.ext_shown);
    // Narrow ignores the remembered extensions choice and keeps its reset-to-default behaviour.
    let mut ui = UiState::default();
    ui.wizard.dismissed = true;
    ui.ext_pref = Some(true);
    let mut h = harness_ui(two_calls(), NARROW, ui);
    h.run();
    assert!(h.query_by_label("EXTENSIONS").is_none());
    // The header chevron stores the choice for saving; narrow's tab does not.
    let mut h = harness(two_calls(), WIDE);
    h.run();
    h.get_by_label_contains("Extensions").click_accesskit();
    h.run();
    assert_eq!(h.state().ui.ext_pref, Some(false));
    assert_eq!(
        h.state_mut().ui.take_panel_save().unwrap().extensions_open,
        Some(false)
    );
    let mut h = harness(two_calls(), NARROW);
    h.run();
    h.get_by_label_contains("Extensions").click_accesskit();
    h.run();
    assert_eq!(h.state().ui.ext_pref, None);
    assert!(h.state_mut().ui.take_panel_save().is_none());
}

// ---- call waiting ---------------------------------------------------------------------------

#[test]
fn call_waiting_banner_buttons_dispatch_the_right_actions() {
    for size in [WIDE, NARROW] {
        let mut h = harness(call_waiting(), size);
        h.run();
        click_first(&mut h, "Answer & hold Bob Martin");
        assert_eq!(take(&mut h), [UiAction::AnswerHold("c3".into())]);
        click_first(&mut h, "Answer & end Bob Martin");
        assert_eq!(
            take(&mut h),
            [UiAction::AnswerEnd("c3".into(), "c1".into())]
        );
        click_first(&mut h, "Decline");
        assert_eq!(take(&mut h), [UiAction::Reject("c3".into())]);
        click_first(&mut h, "Silence");
        assert_eq!(take(&mut h), [UiAction::Silence("c3".into())]);
        assert!(h.state().ui.silenced.contains("c3"));
        assert!(h.query_by_label("Silenced").is_some());
    }
}

#[test]
fn a_single_ringing_call_is_answered_from_the_stage() {
    let mut snap = base_snapshot();
    snap.calls = vec![call(
        "c1",
        CallDirection::Inbound,
        CallState::Ringing,
        ("Reception desk", "sip:1002@pbx.example.test"),
    )];
    let mut h = harness(snap, WIDE);
    h.run();
    click_first(&mut h, "Answer");
    assert_eq!(take(&mut h), [UiAction::Answer("c1".into())]);
    click_first(&mut h, "Reject");
    assert_eq!(take(&mut h), [UiAction::Reject("c1".into())]);
}

#[test]
fn banners_stack_newest_first_with_a_more_counter() {
    let mut snap = two_calls();
    for i in 0..5 {
        let mut w = call(
            &format!("w{i}"),
            CallDirection::Inbound,
            CallState::Ringing,
            (&format!("Caller {i}"), "sip:1@x"),
        );
        w.created_at = now() - chrono::Duration::seconds(10 - i);
        snap.calls.push(w);
    }
    let mut h = harness(snap, WIDE);
    h.run();
    assert_eq!(
        h.query_all_by_label_contains("Answer & hold Bob Martin")
            .count(),
        3
    );
    assert!(h.query_by_label("+2 more incoming").is_some());
}

// ---- transfer -------------------------------------------------------------------------------

fn composer_harness(enter: TransferEnter) -> Harness<'static, State> {
    let mut snap = two_calls();
    snap.ui.transfer_enter = enter;
    let mut h = harness(snap, WIDE);
    h.run();
    click_first(&mut h, "Transfer");
    assert!(h.state().ui.composer.is_some(), "composer opens");
    h
}

#[test]
fn composer_enter_follows_transfer_enter() {
    for (enter, plain, with_mod) in [
        (TransferEnter::Consult, true, false),
        (TransferEnter::Blind, false, true),
    ] {
        let mut h = composer_harness(enter);
        let field = h.get_by_role(egui::accesskit::Role::TextInput);
        field.focus();
        field.type_text("1010");
        h.run();
        h.key_press(egui::Key::Enter);
        h.run();
        let expect = |consult: bool| {
            if consult {
                [UiAction::ConsultStart("c1".into(), "1010".into())]
            } else {
                [UiAction::TransferBlind("c1".into(), "1010".into())]
            }
        };
        assert_eq!(take(&mut h), expect(plain), "{enter:?}: Enter");
        assert!(h.state().ui.composer.is_none(), "composer closes");

        let mut h = composer_harness(enter);
        let field = h.get_by_role(egui::accesskit::Role::TextInput);
        field.focus();
        field.type_text("1010");
        h.run();
        h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Enter);
        h.run();
        assert_eq!(take(&mut h), expect(with_mod), "{enter:?}: Mod+Enter");
    }
}

#[test]
fn composer_buttons_and_blf_suggestions() {
    let mut h = composer_harness(TransferEnter::Consult);
    // Needs a target first.
    assert!(take(&mut h).is_empty());
    // BLF entries are suggested with their live state; picking one fills the field.
    click_first(&mut h, "Transfer to Reception (1002)");
    click_first(&mut h, "Consult first");
    assert_eq!(
        take(&mut h),
        [UiAction::ConsultStart("c1".into(), "1002".into())]
    );
    let mut h = composer_harness(TransferEnter::Consult);
    let field = h.get_by_role(egui::accesskit::Role::TextInput);
    field.focus();
    field.type_text("sip:77@other.example");
    h.run();
    click_first(&mut h, "Transfer now");
    assert_eq!(
        take(&mut h),
        [UiAction::TransferBlind(
            "c1".into(),
            "sip:77@other.example".into()
        )]
    );
    // Close leaves no action.
    let mut h = composer_harness(TransferEnter::Consult);
    click_first(&mut h, "Close");
    assert!(take(&mut h).is_empty());
    assert!(h.state().ui.composer.is_none());
}

#[test]
fn composer_links_an_existing_second_call() {
    let mut h = composer_harness(TransferEnter::Consult);
    // c2 has no display name, so it is listed by its URI.
    click_first(&mut h, "sip:+4930123456@pbx.example.test (L2)");
    assert_eq!(
        take(&mut h),
        [UiAction::ConsultLink("c1".into(), "c2".into())]
    );
}

fn session_harness(phase: TransferPhase, select: &str) -> Harness<'static, State> {
    let mut snap = transfer_consult();
    snap.transfers = vec![session(phase)];
    let mut h = harness(snap, WIDE);
    h.state_mut().ui.selected_line = Some(select.into());
    h.run_steps(4);
    h
}

#[test]
fn session_panel_consult_ringing_offers_blind_and_cancel() {
    let mut h = session_harness(TransferPhase::ConsultRinging, "c1");
    click_first(&mut h, "Transfer now (blind)");
    assert_eq!(
        take(&mut h),
        [UiAction::TransferComplete(
            "t1".into(),
            CompleteMode::BlindNow
        )]
    );
    click_first(&mut h, "Cancel - return to Bob Martin");
    assert_eq!(take(&mut h), [UiAction::TransferCancel("t1".into())]);
    assert!(h.query_by_label_contains("Complete transfer:").is_none());
}

#[test]
fn session_panel_consult_connected_offers_complete_swap_cancel() {
    let mut h = session_harness(TransferPhase::ConsultConnected, "c1");
    click_first(&mut h, "Complete transfer: Bob Martin");
    assert_eq!(
        take(&mut h),
        [UiAction::TransferComplete(
            "t1".into(),
            CompleteMode::Attended
        )]
    );
    click_first(&mut h, "Swap to Bob Martin");
    assert_eq!(take(&mut h), [UiAction::TransferSwap("t1".into())]);
    click_first(&mut h, "Cancel - return to Bob Martin");
    assert_eq!(take(&mut h), [UiAction::TransferCancel("t1".into())]);
    // Swapped (talking to Bob): swap goes to Carol and cancel drops her.
    h.state_mut().snap.transfers[0].talking_to = Some("c1".into());
    h.run();
    click_first(&mut h, "Swap to Carol Diaz");
    assert_eq!(take(&mut h), [UiAction::TransferSwap("t1".into())]);
    click_first(&mut h, "Cancel - drop Carol Diaz");
    assert_eq!(take(&mut h), [UiAction::TransferCancel("t1".into())]);
}

#[test]
fn consult_leg_hang_up_is_relabelled_cancel_return() {
    let mut h = harness(transfer_consult(), WIDE);
    h.run();
    // c2 (the consult call) is the active line and selected.
    assert_eq!(h.state().ui.selected_line.as_deref(), Some("c2"));
    assert!(h.query_by_label("Hang up").is_none());
    click_first(&mut h, "Cancel - return to Bob Martin");
    assert_eq!(take(&mut h), [UiAction::TransferCancel("t1".into())]);
}

#[test]
fn session_panel_disables_actions_while_referring_and_leaves_while_transferring() {
    let h = session_harness(TransferPhase::Referring, "c1");
    assert!(
        h.query_by_label_contains("Sending the transfer request")
            .is_some()
    );
    assert!(h.query_by_label_contains("Complete transfer:").is_none());
    assert!(h.query_by_label_contains("Swap to").is_none());
    // Hold of either leg is disabled (REFER outstanding).
    let hold = h.query_all_by_label_contains("Resume").next();
    assert!(hold.is_some_and(|n| n.accesskit_node().is_disabled()));
    let mut h = session_harness(TransferPhase::Transferring, "c1");
    click_first(&mut h, "Leave now");
    assert_eq!(take(&mut h), [UiAction::Hangup("c1".into())]);
}

#[test]
fn session_panel_unconfirmed_and_failure_messaging() {
    let mut h = session_harness(TransferPhase::Unconfirmed, "c1");
    assert!(
        h.query_by_label_contains("No confirmation from the PBX")
            .is_some()
    );
    click_first(&mut h, "Resume Bob Martin");
    assert_eq!(take(&mut h), [UiAction::Activate("c1".into())]);
    click_first(&mut h, "Hang up Bob Martin");
    assert_eq!(take(&mut h), [UiAction::Hangup("c1".into())]);

    let mut snap = transfer_consult();
    let mut s = session(TransferPhase::Failed);
    s.last_error = Some(siple_sip::TransferError {
        stage: "refer".into(),
        code: 486,
        reason: "Busy Here".into(),
    });
    snap.transfers = vec![s];
    let mut h = harness(snap, WIDE);
    h.state_mut().ui.selected_line = Some("c1".into());
    h.run();
    assert!(h.query_by_label_contains("failed: Busy.").is_some());
    click_first(&mut h, "Dismiss");
    assert!(h.state().ui.dismissed_sessions.contains("t1"));
    h.run();
    assert!(h.query_by_label_contains("failed: Busy").is_none());
}

#[test]
fn transferred_session_stays_until_dismissed_or_30_seconds() {
    let mut snap = base_snapshot();
    snap.transfers = vec![session(TransferPhase::Transferred)];
    let mut h = harness(snap, WIDE);
    h.run();
    assert!(h.query_by_label_contains("Transferred Bob").is_none());
    // Lines are gone but the panel shows the result; the call names fall back to the target.
    assert!(h.query_by_label_contains("The lines are closed").is_some());
    click_first(&mut h, "Dismiss");
    h.run();
    assert!(h.query_by_label_contains("The lines are closed").is_none());
    // Older than 30 s: not shown at all.
    let mut snap = base_snapshot();
    let mut s = session(TransferPhase::Transferred);
    s.ended_at = Some(now() - chrono::Duration::seconds(31));
    snap.transfers = vec![s];
    let mut h = harness(snap, WIDE);
    h.run();
    assert!(h.query_by_label_contains("The lines are closed").is_none());
}

#[test]
fn clicking_the_other_leg_in_a_consult_swaps_the_session() {
    let mut h = harness(transfer_consult(), WIDE);
    h.run();
    // c1 is held, c2 talking: clicking Bob swaps rather than activating.
    h.get_by_label_contains("Line 1, Bob Martin")
        .click_accesskit();
    h.run();
    assert_eq!(take(&mut h), [UiAction::TransferSwap("t1".into())]);
}

// ---- BLF --------------------------------------------------------------------------------------

/// A click on a BLF key places a call to it on its owning account (`blf.dial`); the dial box
/// is left alone.
#[test]
fn blf_click_dials_the_key_on_its_owning_account() {
    for size in [NARROW, WIDE] {
        for (target, enabled) in [
            ("2041", true),
            ("sip:2041@other.example", true),
            ("sips:2041@secure.example", true),
            ("2041", false),
        ] {
            let mut snap = base_snapshot();
            snap.accounts[1].enabled = enabled;
            snap.blf.truncate(1);
            snap.blf[0].account = "lab-tls".into();
            snap.blf[0].uri = target.into();
            let mut h = harness_ext(snap, size);
            h.run();
            h.get_by_label("Reception (lab-tls)").click_accesskit();
            h.run();
            assert_eq!(
                h.state().actions,
                [UiAction::BlfDial("lab-tls".into(), target.into())]
            );
            assert_eq!(h.state().ui.dial_target, "");
            h.run_steps(2);
            assert_eq!(h.state().actions.len(), 1, "one click, one action");
        }
    }
}

#[test]
fn blf_click_prefill_fills_the_dial_field_instead() {
    let mut snap = base_snapshot();
    snap.ui.blf_click = BlfClick::Prefill;
    let mut h = harness_ext(snap, WIDE);
    h.run();
    h.get_by_label("Bob Martin (desk-1001)").click_accesskit();
    h.run();
    assert!(h.state().actions.is_empty(), "{:?}", h.state().actions);
    assert_eq!(h.state().ui.dial_target, "1003");
    assert_eq!(h.state().ui.dial_account.as_deref(), Some("desk-1001"));
}

/// A ringing key whose pickup is configured picks up (key or "Pick up" button); every other
/// key still dials. `blf_click_ringing = "call"` makes the key itself dial.
#[test]
fn ringing_blf_with_pickup_configured_picks_up() {
    for size in [NARROW, WIDE] {
        let mut snap = base_snapshot();
        snap.blf.truncate(4); // Reception idle, Bob busy, Support ringing, Warehouse held
        snap.blf[2].pickup = PickupMode::Prefix;
        // Busy with pickup configured: pickup is only for ringing keys.
        snap.blf[1].pickup = PickupMode::Prefix;
        let mut h = harness_ext(snap, size);
        h.run();
        h.get_by_label("Support queue (desk-1001)")
            .click_accesskit();
        h.run();
        h.state_mut().ui.ext_tab = true; // narrow returns to the stage after an action
        h.run();
        assert_eq!(
            h.state().actions,
            [UiAction::Pickup("desk-1001".into(), "1004".into())]
        );
        // The visible "Pick up" affordance does the same (a new click, not a double click).
        h.state_mut().actions.clear();
        h.state_mut().ui.blf_last_click.clear();
        h.get_by_label("Pick up Support queue (desk-1001)")
            .click_accesskit();
        h.run();
        h.state_mut().ui.ext_tab = true; // narrow returns to the stage after an action
        h.run();
        assert_eq!(
            h.state().actions,
            [UiAction::Pickup("desk-1001".into(), "1004".into())]
        );
        // Busy key: call it, and no pickup button.
        h.state_mut().actions.clear();
        assert!(
            h.query_by_label("Pick up Bob Martin (desk-1001)").is_none(),
            "only ringing keys offer pickup"
        );
        h.get_by_label("Bob Martin (desk-1001)").click_accesskit();
        h.run();
        h.state_mut().ui.ext_tab = true; // narrow returns to the stage after an action
        h.run();
        assert_eq!(
            h.state().actions,
            [UiAction::BlfDial("desk-1001".into(), "1003".into())]
        );
        // Ringing but pickup off: call.
        h.state_mut().actions.clear();
        h.state_mut().ui.blf_last_click.clear();
        h.state_mut().snap.blf[2].pickup = PickupMode::Off;
        h.run();
        assert!(
            h.query_by_label("Pick up Support queue (desk-1001)")
                .is_none()
        );
        h.get_by_label("Support queue (desk-1001)")
            .click_accesskit();
        h.run();
        h.state_mut().ui.ext_tab = true; // narrow returns to the stage after an action
        h.run();
        assert_eq!(
            h.state().actions,
            [UiAction::BlfDial("desk-1001".into(), "1004".into())]
        );
        // Pickup configured but `blf_click_ringing = "call"`: the key dials; the button picks up.
        h.state_mut().actions.clear();
        h.state_mut().ui.blf_last_click.clear();
        h.state_mut().snap.blf[2].pickup = PickupMode::Prefix;
        h.state_mut().snap.ui.blf_click_ringing = BlfClickRinging::Call;
        h.run();
        h.get_by_label("Support queue (desk-1001)")
            .click_accesskit();
        h.run();
        h.state_mut().ui.ext_tab = true; // narrow returns to the stage after an action
        h.run();
        assert_eq!(
            h.state().actions,
            [UiAction::BlfDial("desk-1001".into(), "1004".into())]
        );
    }
}

#[test]
fn blf_tooltips_say_what_a_click_does_and_show_health() {
    let mut snap = base_snapshot();
    snap.blf[2].pickup = PickupMode::Prefix;
    snap.blf[0].stale = true;
    let mut h = harness_ext(snap, WIDE);
    h.run();
    h.get_by_label("Support queue (desk-1001)").hover();
    h.run_steps(60);
    assert!(h.query_by_label_contains("Click to pick up").is_some());
    h.get_by_label("Reception (desk-1001)").hover();
    h.run_steps(60);
    assert!(h.query_by_label_contains("Click to call").is_some());
    assert!(h.query_by_label_contains("Stale:").is_some());
    // Failed: the key reads "Failed" (never differing from Busy by colour alone); the tooltip
    // names the problem and shows the server's answer.
    h.get_by_label("Old lobby (desk-1001)").hover();
    h.run_steps(60);
    assert!(h.query_by_label_contains("Failed").is_some());
    assert!(h.query_by_label_contains("Extension not found").is_some());
    assert!(
        h.query_by_label_contains("Details: 404 Not Found")
            .is_some()
    );
}

/// Accessibility: keys expose their state as the accessible value.
#[test]
fn blf_keys_expose_state_and_staleness_to_assistive_tech() {
    let mut snap = base_snapshot();
    snap.blf[2].stale = true;
    let mut h = harness_ext(snap, WIDE);
    h.run();
    let value =
        |h: &Harness<'_, State>, label: &str| h.get_by_label(label).value().unwrap_or_default();
    assert_eq!(
        value(&h, "Support queue (desk-1001)"),
        "1004, Ringing, with Dana, stale"
    );
    assert_eq!(
        value(&h, "Old lobby (desk-1001)"),
        "1099, Failed, Extension not found"
    );
    assert_eq!(value(&h, "Bob Martin (desk-1001)"), "1003, Busy");
}

fn open_account_menu(h: &mut Harness<'_, State>, label: &str) {
    h.get_by_label_contains(label).click_accesskit();
    h.run();
}

#[test]
fn account_toggle_emits_existing_enablement_setting_for_the_correct_row() {
    let mut h = harness(base_snapshot(), WIDE);
    h.run();
    open_account_menu(&mut h, "Account lab-tls");
    h.get_by_label("Enable lab-tls").click_accesskit();
    h.run();
    assert_eq!(
        h.state().actions,
        [UiAction::SetAccountEnabled("lab-tls".into(), false)]
    );
    h.state_mut().snap.accounts[1].enabled = false;
    h.state_mut().actions.clear();
    h.run();
    h.get_by_label("Enable lab-tls").click_accesskit();
    h.run();
    assert_eq!(
        h.state().actions,
        [UiAction::SetAccountEnabled("lab-tls".into(), true)]
    );
}

#[test]
fn account_menu_has_dnd_registration_details_and_voicemail() {
    let mut h = harness(base_snapshot(), MEDIUM);
    h.run();
    open_account_menu(&mut h, "Account desk-1001");
    assert!(h.query_by_label_contains("Last renewal:").is_some());
    assert!(h.query_by_label_contains("4:00 remaining").is_some());
    assert!(
        h.query_by_label_contains("Voicemail: 2 new / 5 old")
            .is_some()
    );
    h.get_by_label("Do not disturb desk-1001").click_accesskit();
    h.run();
    assert_eq!(
        h.state().actions,
        [UiAction::SetDnd("desk-1001".into(), true)]
    );
    // The header carries the voicemail total.
    assert!(h.query_by_label("VM 2").is_some());
}

#[test]
fn disabled_account_and_config_error_still_show() {
    let mut snap = base_snapshot();
    snap.accounts[1].enabled = false;
    snap.config_error = Some(crate::snapshot::ConfigErrorView {
        at: "2026-10-08T09:29:41+00:00".into(),
        errors: vec!["accounts[2].transport: bad".into()],
    });
    let mut h = harness(snap, WIDE);
    h.run();
    assert!(h.query_by_label("Config not applied").is_some());
    assert!(
        h.query_by_label_contains("Account lab-tls: disabled")
            .is_some()
    );
    click_first(&mut h, "Open config");
    assert_eq!(take(&mut h)[0], UiAction::OpenConfig);
}

/// The footer Log / Open logs / Open config buttons and the log pane need `developer_tools`.
#[test]
fn footer_dev_controls_follow_developer_tools() {
    let mut snap = base_snapshot();
    snap.logs = vec!["INFO hello-log-line".into()];
    let mut h = harness(snap.clone(), WIDE);
    h.run();
    assert!(h.query_by_label_contains("Open config").is_none());
    assert!(h.query_by_label_contains("Log").is_none());

    snap.ui.developer_tools = true;
    let mut h = harness(snap.clone(), WIDE);
    h.run();
    click_first(&mut h, "Log");
    assert_eq!(take(&mut h), [UiAction::LogsVisible(true)]);
    assert!(h.query_by_label_contains("hello-log-line").is_some());
    assert!(h.query_by_label_contains("Open config").is_some());

    // Turning the setting off closes the open pane and stops log capture.
    h.state_mut().snap.ui.developer_tools = false;
    h.run_steps(4);
    assert!(!h.state().ui.show_logs);
    assert_eq!(take(&mut h), [UiAction::LogsVisible(false)]);
    assert!(h.query_by_label_contains("hello-log-line").is_none());
}

/// A double click (two quick pointer clicks) places one call, not two.
#[test]
fn double_click_on_a_blf_key_dials_once() {
    let mut h = harness_ext(base_snapshot(), WIDE);
    h.run();
    h.get_by_label("Reception (desk-1001)").click();
    h.run();
    h.get_by_label("Reception (desk-1001)").click();
    h.run();
    assert_eq!(
        h.state().actions,
        [UiAction::BlfDial("desk-1001".into(), "1002".into())]
    );
}

/// During an active call a click on a plain key opens a menu instead of dial-and-hold.
#[test]
fn blf_click_during_a_call_opens_the_in_call_menu() {
    let mut h = harness_ext(two_calls(), WIDE);
    h.run();
    h.get_by_label("Reception (desk-1001)").click_accesskit();
    h.run();
    assert!(h.state().actions.is_empty(), "{:?}", h.state().actions);
    assert!(h.query_by_label("New call (holds current)").is_some());
    // Consult first (default entry): a transfer session to the key.
    h.get_by_label("Consult 1002").click_accesskit();
    h.run();
    assert_eq!(
        h.state().actions,
        [UiAction::ConsultStart("c1".into(), "1002".into())]
    );
    // Blind transfer of the active line (c1, Bob Martin) to the key.
    h.state_mut().actions.clear();
    h.state_mut().ui.blf_last_click.clear();
    h.get_by_label("Reception (desk-1001)").click_accesskit();
    h.run();
    h.get_by_label("Transfer Bob Martin to 1002")
        .click_accesskit();
    h.run();
    assert_eq!(
        h.state().actions,
        [UiAction::TransferBlind("c1".into(), "1002".into())]
    );
    assert!(
        h.query_by_label("New call (holds current)").is_none(),
        "menu closes"
    );
    // "New call" is the old dial behaviour.
    h.state_mut().actions.clear();
    h.get_by_label("Sales (desk-1001)").click_accesskit();
    h.run();
    h.get_by_label("New call (holds current)").click_accesskit();
    h.run();
    assert_eq!(
        h.state().actions,
        [UiAction::BlfDial("desk-1001".into(), "1010".into())]
    );
}

#[test]
fn blf_click_in_call_consult_or_blind_acts_directly() {
    for (mode, expected) in [
        (
            BlfClickInCall::Consult,
            UiAction::ConsultStart("c1".into(), "1002".into()),
        ),
        (
            BlfClickInCall::Blind,
            UiAction::TransferBlind("c1".into(), "1002".into()),
        ),
    ] {
        let mut snap = two_calls();
        snap.ui.blf_click_in_call = mode;
        let mut h = harness_ext(snap, WIDE);
        h.run();
        h.get_by_label("Reception (desk-1001)").click_accesskit();
        h.run();
        assert_eq!(h.state().actions, [expected]);
        assert!(h.query_by_label("New call (holds current)").is_none());
    }
}

/// In a consult session (B already exists) the popover offers a new call only.
#[test]
fn blf_click_in_a_consult_session_offers_only_a_new_call() {
    let mut snap = transfer_consult();
    snap.ui.blf_click_in_call = BlfClickInCall::Consult;
    let mut h = harness_ext(snap, WIDE);
    h.run();
    h.get_by_label("Reception (desk-1001)").click_accesskit();
    h.run();
    assert!(h.state().actions.is_empty(), "{:?}", h.state().actions);
    assert!(h.query_by_label("New call (holds current)").is_some());
    assert!(h.query_by_label_contains("Consult 1002").is_none());
    assert!(
        h.query_by_label_contains("Transfer Carol Diaz to")
            .is_none()
    );
}

#[test]
fn in_call_menu_closes_on_escape_and_click_outside() {
    let mut h = harness_ext(two_calls(), WIDE);
    h.run();
    h.get_by_label("Reception (desk-1001)").click_accesskit();
    h.run();
    assert!(h.query_by_label("New call (holds current)").is_some());
    h.key_press(egui::Key::Escape);
    h.run();
    assert!(h.query_by_label("New call (holds current)").is_none());
    assert!(h.state().actions.is_empty());
    h.state_mut().ui.blf_last_click.clear();
    h.get_by_label("Reception (desk-1001)").click_accesskit();
    h.run();
    assert!(h.query_by_label("New call (holds current)").is_some());
    h.get_by_label("EXTENSIONS").click();
    h.run();
    assert!(h.query_by_label("New call (holds current)").is_none());
    assert!(h.state().actions.is_empty());
}

/// During a call a ringing key with pickup opens the same menu, "Pick up (holds current)" first;
/// the visible "Pick up" button is explicit and picks up directly.
#[test]
fn ringing_blf_with_pickup_during_a_call_offers_pickup_in_the_menu() {
    let mut snap = two_calls();
    snap.blf[2].pickup = PickupMode::Prefix;
    let mut h = harness_ext(snap, WIDE);
    h.run();
    h.get_by_label("Support queue (desk-1001)")
        .click_accesskit();
    h.run();
    assert!(h.state().actions.is_empty(), "{:?}", h.state().actions);
    assert!(h.query_by_label("Transfer Bob Martin to 1004").is_some());
    assert!(h.query_by_label("New call (holds current)").is_some());
    h.get_by_label("Pick up (holds current)").click_accesskit();
    h.run();
    assert_eq!(
        h.state().actions,
        [UiAction::Pickup("desk-1001".into(), "1004".into())]
    );
    // Keys that are not ringing do not list it.
    h.state_mut().actions.clear();
    h.get_by_label("Reception (desk-1001)").click_accesskit();
    h.run();
    assert!(h.query_by_label("Pick up (holds current)").is_none());
    assert!(h.query_by_label("New call (holds current)").is_some());
    // The visible button is an explicit pickup.
    h.key_press(egui::Key::Escape);
    h.run();
    h.state_mut().ui.blf_last_click.clear();
    h.get_by_label("Pick up Support queue (desk-1001)")
        .click_accesskit();
    h.run();
    assert_eq!(
        h.state().actions,
        [UiAction::Pickup("desk-1001".into(), "1004".into())]
    );
}

/// A call the far end holds (`RemoteHeld`) is still the active line: no silent dial-and-hold.
#[test]
fn blf_click_while_the_active_call_is_remote_held_opens_the_menu() {
    let mut snap = two_calls();
    snap.calls[0].state = siple_sip::CallState::RemoteHeld;
    let mut h = harness_ext(snap, WIDE);
    h.run();
    h.get_by_label("Reception (desk-1001)").click_accesskit();
    h.run();
    assert!(h.state().actions.is_empty(), "{:?}", h.state().actions);
    assert!(h.query_by_label("New call (holds current)").is_some());
    // Without an active line (everything held) a click still dials.
    h.key_press(egui::Key::Escape);
    h.run();
    h.state_mut().snap.active_line = None;
    h.state_mut().ui.blf_last_click.clear();
    h.run();
    h.get_by_label("Reception (desk-1001)").click_accesskit();
    h.run();
    assert_eq!(
        h.state().actions,
        [UiAction::BlfDial("desk-1001".into(), "1002".into())]
    );
}

/// A double click on a key whose menu is open leaves the menu open (and does nothing else).
#[test]
fn double_click_during_a_call_keeps_the_menu_open() {
    let mut h = harness_ext(two_calls(), WIDE);
    h.run();
    h.get_by_label("Reception (desk-1001)").click();
    h.run();
    h.get_by_label("Reception (desk-1001)").click();
    h.run();
    assert!(h.state().actions.is_empty(), "{:?}", h.state().actions);
    assert!(h.query_by_label("New call (holds current)").is_some());
}

/// A stale (or retrying) ringing key does not offer pickup: no button, a click calls.
#[test]
fn stale_ringing_blf_does_not_offer_pickup() {
    let mut snap = base_snapshot();
    snap.blf[2].pickup = PickupMode::Prefix;
    snap.blf[2].stale = true;
    let mut h = harness_ext(snap, WIDE);
    h.run();
    assert!(
        h.query_by_label("Pick up Support queue (desk-1001)")
            .is_none()
    );
    h.get_by_label("Support queue (desk-1001)")
        .click_accesskit();
    h.run();
    assert_eq!(
        h.state().actions,
        [UiAction::BlfDial("desk-1001".into(), "1004".into())]
    );
    h.state_mut().actions.clear();
    h.state_mut().snap.blf[2].stale = false;
    h.state_mut().snap.blf[2].subscription.phase = siple_sip::SubscriptionPhase::Retrying;
    h.run();
    assert!(
        h.query_by_label("Pick up Support queue (desk-1001)")
            .is_none()
    );
}

/// Retrying keeps the last known state; the tooltip names the problem, the code and the retry.
#[test]
fn retrying_blf_tooltip_shows_code_and_keeps_state() {
    let mut snap = base_snapshot();
    let b = &mut snap.blf[2];
    b.stale = true;
    b.subscription.phase = siple_sip::SubscriptionPhase::Retrying;
    b.subscription.last_code = Some(503);
    b.subscription.last_reason = Some("Service Unavailable".into());
    b.subscription.retry_at = Some(chrono::Utc::now() + chrono::Duration::seconds(20));
    let mut h = harness_ext(snap, WIDE);
    h.run();
    h.get_by_label("Support queue (desk-1001)").hover();
    h.run_steps(60);
    assert!(
        h.query_by_label_contains("The server had a problem")
            .is_some()
    );
    assert!(
        h.query_by_label_contains("Details: 503 Service Unavailable, next attempt")
            .is_some()
    );
    assert!(h.query_by_label_contains("Stale:").is_some());
}

#[test]
fn density_switch_changes_view_state_only() {
    let mut h = harness_ext(base_snapshot(), WIDE);
    h.run();
    assert_eq!(h.state().ui.blf_view, None);
    for (label, view) in [
        ("Lamps view", BlfView::Lamps),
        ("List view", BlfView::List),
        ("Keys view", BlfView::Keys),
    ] {
        h.get_by_label(label).click_accesskit();
        h.run();
        assert_eq!(h.state().ui.blf_view, Some(view));
        // Every density keeps the keys reachable with the same names and a click still acts.
        assert!(
            h.query_by_label("Reception (desk-1001)").is_some(),
            "{label}"
        );
        assert_eq!(
            h.state().snap.ui.blf_view,
            BlfView::Keys,
            "config untouched"
        );
    }
    // A click in the list density behaves like a key click.
    h.get_by_label("List view").click_accesskit();
    h.run();
    h.get_by_label("Reception (desk-1001)").click_accesskit();
    h.run();
    assert_eq!(
        h.state().actions,
        [UiAction::BlfDial("desk-1001".into(), "1002".into())]
    );
}

#[test]
fn lamp_and_list_densities_pick_up_ringing_keys_too() {
    for view in [BlfView::Lamps, BlfView::List] {
        let mut snap = base_snapshot();
        snap.ui.blf_view = view;
        snap.blf[2].pickup = PickupMode::Prefix;
        let mut h = harness_ext(snap, WIDE);
        h.run();
        h.get_by_label("Support queue (desk-1001)")
            .click_accesskit();
        h.run();
        assert_eq!(
            h.state().actions,
            [UiAction::Pickup("desk-1001".into(), "1004".into())],
            "{view:?}"
        );
    }
    // The list also has the visible button.
    let mut snap = base_snapshot();
    snap.ui.blf_view = BlfView::List;
    snap.blf[2].pickup = PickupMode::Prefix;
    let mut h = harness_ext(snap, WIDE);
    h.run();
    assert!(
        h.query_by_label("Pick up Support queue (desk-1001)")
            .is_some()
    );
}

#[test]
fn filter_box_and_chips_appear_with_many_entries() {
    // Few entries: no filter box.
    let mut h = harness_ext(base_snapshot(), WIDE);
    h.run();
    assert!(h.query_by_label("Filter extensions").is_none());
    let mut snap = base_snapshot();
    snap.blf = blf_many();
    let mut h = harness_ext(snap, WIDE);
    h.run();
    assert!(h.query_by_label("Filter extensions").is_some());
    assert!(h.query_by_label_contains("Problems").is_some());
    let field = h.get_by_label("Filter extensions");
    field.focus();
    field.type_text("queue");
    h.run();
    assert!(h.query_by_label("Support queue (desk-1001)").is_some());
    assert!(h.query_by_label("Reception (desk-1001)").is_none());
    assert!(h.query_by_label_contains("1 of 14").is_some());
}

#[test]
fn groups_follow_blf_group_by_and_collapse() {
    let mut snap = base_snapshot();
    snap.blf = blf_many();
    snap.ui.blf_group_by = BlfGroupBy::Group;
    let mut h = harness_ext(snap, WIDE);
    h.run();
    assert!(h.query_by_label_contains("Group Sales,").is_some());
    assert!(h.query_by_label_contains("Group Support,").is_some());
    assert!(h.query_by_label_contains("Group desk-1001,").is_none());
    h.get_by_label_contains("Group Sales,").click_accesskit();
    h.run();
    // Sales holds Reception (index 0): collapsed away.
    assert!(h.query_by_label("Reception (desk-1001)").is_none());
    assert!(h.query_by_label("Support queue (desk-1001)").is_some());
    // By account: one group per account.
    let mut snap = base_snapshot();
    snap.blf = blf_many();
    snap.ui.blf_group_by = BlfGroupBy::Account;
    let mut h = harness_ext(snap, WIDE);
    h.run();
    assert!(h.query_by_label_contains("Group desk-1001,").is_some());
    // No grouping: no headers.
    let mut snap = base_snapshot();
    snap.blf = blf_many();
    snap.ui.blf_group_by = BlfGroupBy::None;
    let mut h = harness_ext(snap, WIDE);
    h.run();
    assert!(h.query_by_label_contains("Group ").is_none());
}

#[test]
fn many_identical_failures_collapse_into_one_banner() {
    let mut snap = base_snapshot();
    snap.blf = (0..6)
        .map(|i| {
            let mut b = blf(&format!("20{i}"), &format!("Ext {i}"), BlfState::Failed);
            b.subscription.last_code = Some(489);
            b
        })
        .collect();
    let mut h = harness_ext(snap, WIDE);
    h.run();
    assert!(
        h.query_by_label_contains("desk-1001: Line status not available")
            .is_some()
    );
    assert!(
        h.query_by_label_contains("6 extensions are affected.")
            .is_some()
    );
    // A single failure shows no banner.
    let mut h = harness_ext(base_snapshot(), WIDE);
    h.run();
    assert!(
        h.query_by_label_contains("extensions are affected")
            .is_none()
    );
}

/// Local failures (code 0) group by problem too: six keys refused with "noresource".
#[test]
fn many_local_failures_collapse_into_one_banner() {
    let mut snap = base_snapshot();
    snap.blf = (0..6)
        .map(|i| {
            let mut b = blf(&format!("20{i}"), &format!("Ext {i}"), BlfState::Failed);
            b.subscription.last_code = None;
            b.subscription.last_reason = Some("subscription terminated: noresource".into());
            b
        })
        .collect();
    let mut h = harness_ext(snap, WIDE);
    h.run();
    assert!(
        h.query_by_label_contains("desk-1001: Extension not found")
            .is_some()
    );
}

/// `other` and `refused` cover unrelated causes: they never collapse into one banner.
#[test]
fn unclassified_failures_keep_their_own_lamps() {
    let mut snap = base_snapshot();
    snap.blf = (0..6)
        .map(|i| {
            let mut b = blf(&format!("20{i}"), &format!("Ext {i}"), BlfState::Failed);
            b.subscription.last_code = None;
            b.subscription.last_reason = Some(format!("gremlin {i}"));
            b
        })
        .collect();
    let mut h = harness_ext(snap, WIDE);
    h.run();
    assert!(
        h.query_by_label_contains("extensions are affected")
            .is_none()
    );
}

#[test]
fn narrow_returns_to_the_stage_after_a_blf_action() {
    let mut h = harness_ext(base_snapshot(), NARROW);
    h.run();
    assert!(h.query_by_label("EXTENSIONS").is_some());
    h.get_by_label("Reception (desk-1001)").click_accesskit();
    h.run();
    assert_eq!(
        h.state().actions,
        [UiAction::BlfDial("desk-1001".into(), "1002".into())]
    );
    assert!(!h.state().ui.ext_tab);
}

// ---- setup links -------------------------------------------------------------------------------

use crate::ui::wizard::{Offer, Origin, Step as WStep};
use siple_core::config::{Profile, SrtpPolicy, Transport};
use siple_core::provision::{SetupAccount, SetupLink};

fn link_for(username: &str, domain: &str) -> SetupLink {
    SetupLink::new(SetupAccount {
        username: username.into(),
        domain: domain.into(),
        display_name: "Front desk".into(),
        registrar: "pbx.example.test:5061".into(),
        transport: Some(Transport::Tls),
        srtp: Some(SrtpPolicy::Mandatory),
        profile: Some(Profile::Yealink),
        ..Default::default()
    })
}

fn offer(
    h: &mut Harness<'_, State>,
    link: SetupLink,
    origin: Origin,
    at: chrono::DateTime<chrono::Utc>,
) -> Offer {
    let snap = h.state().snap.clone();
    let r = h.state_mut().ui.wizard.offer_link(link, origin, &snap, at);
    // The Review screen keeps repainting while Continue is locked.
    h.run_steps(4);
    r
}

fn secs(n: i64) -> chrono::Duration {
    chrono::Duration::seconds(n)
}

#[test]
fn a_setup_link_only_opens_the_review_and_does_nothing_by_itself() {
    let mut h = harness_ui(wizard_snapshot(), WIDE, UiState::default());
    h.run();
    assert_eq!(
        offer(
            &mut h,
            link_for("1001", "example.test"),
            Origin::Opened,
            now()
        ),
        Offer::Opened
    );
    h.run_steps(10);
    assert!(h.query_by_label("Check this setup link").is_some());
    assert!(h.query_by_label("Step 1 of 3").is_some());
    // The server leads, the password is not in the link.
    assert!(h.query_by_label("Server: pbx.example.test:5061").is_some());
    assert!(h.query_by_label_contains("not in the link").is_some());
    assert!(
        take(&mut h).is_empty(),
        "no action without a click: {:?}",
        h.state().actions
    );
    // Continue is locked for a second after the screen appeared.
    assert!(h.get_by_label("Continue").accesskit_node().is_disabled());
    h.get_by_label("Continue").click_accesskit();
    h.run();
    assert!(
        h.query_by_label("Check this setup link").is_some(),
        "locked Continue must not act"
    );
    assert!(take(&mut h).is_empty());
    assert!(h.state().ui.wizard.form.username.is_empty());
}

#[test]
fn continue_is_enabled_once_the_delay_has_passed() {
    let mut h = harness_ui(wizard_snapshot(), WIDE, UiState::default());
    h.run();
    offer(
        &mut h,
        link_for("1001", "example.test"),
        Origin::Opened,
        now() - secs(2),
    );
    assert!(!h.get_by_label("Continue").accesskit_node().is_disabled());
    // Right on the edge.
    let mut h = harness_ui(wizard_snapshot(), WIDE, UiState::default());
    h.run();
    offer(
        &mut h,
        link_for("1001", "example.test"),
        Origin::Opened,
        now() - chrono::Duration::milliseconds(999),
    );
    assert!(h.get_by_label("Continue").accesskit_node().is_disabled());
}

#[test]
fn the_full_flow_needs_every_step_and_ends_in_the_audio_step() {
    let mut h = harness_ui(wizard_snapshot(), WIDE, UiState::default());
    h.run();
    offer(
        &mut h,
        link_for("1001", "example.test"),
        Origin::Opened,
        now() - secs(2),
    );
    click_first(&mut h, "Continue");
    // The form is prefilled and the advanced block shows the registrar; no password yet.
    assert!(h.query_by_label("Welcome to Siple").is_some());
    assert_eq!(h.state().ui.wizard.step, WStep::Account);
    assert_eq!(h.state().ui.wizard.form.username, "1001");
    assert_eq!(h.state().ui.wizard.form.registrar, "pbx.example.test:5061");
    assert!(h.state().ui.wizard.form.password.is_empty());
    assert!(h.query_by_label("Registrar").is_some());
    assert!(
        h.query_by_label_contains("Filled in from a setup link")
            .is_some()
    );
    assert!(take(&mut h).is_empty(), "Continue sends nothing");
    // Test needs the password step; the draft carries the link fields plus transport.
    type_into(&mut h, "Password", "pw-typed");
    h.get_by_label("Test & continue").click_accesskit();
    h.run();
    let req = only_edit(take(&mut h));
    assert_eq!(req.method, "account.test");
    let f = &req.params["draft"]["fields"];
    assert_eq!(f["username"], "1001");
    assert_eq!(f["domain"], "example.test");
    assert_eq!(f["registrar"], "pbx.example.test:5061");
    assert_eq!(f["transport"], "tls");
    assert_eq!(f["srtp"], "mandatory");
    assert_eq!(f["profile"], "yealink");
    assert_eq!(f["display_name"], "Front desk");
    assert_eq!(f["password"], "pw-typed");
    assert_eq!(req.params["draft"]["id"], "1001.example.test");
    deliver(
        &mut h,
        RpcTag::TestDraft,
        Ok(json!({"diagnosis": "ok", "ok": true, "message": "Registered"})),
    );
    let edits = rpc::edits_of(&only_edit(take(&mut h)));
    let [
        ConfigEdit::AddAccount {
            draft,
            replace_example: true,
        },
    ] = &edits[..]
    else {
        panic!("{edits:?}")
    };
    assert_eq!(
        draft.fields.get(&AccountField::Registrar),
        Some(&EditValue::Str("pbx.example.test:5061".into()))
    );
    assert_eq!(
        draft.fields.get(&AccountField::Transport),
        Some(&EditValue::Str("tls".into()))
    );
    deliver(&mut h, RpcTag::WizardAdd, Ok(json!({"applied": true})));
    assert!(h.query_by_label("Step 2 of 3").is_some());
    assert!(h.state().ui.wizard.form.password.is_empty());
}

#[test]
fn test_and_save_waits_for_the_password_of_a_link_account() {
    let mut h = harness(base_snapshot(), WIDE);
    h.run();
    offer(
        &mut h,
        link_for("2001", "example.test"),
        Origin::Opened,
        now() - secs(2),
    );
    click_first(&mut h, "Continue");
    assert!(h.get_by_label("Test & save").accesskit_node().is_disabled());
    h.get_by_label("Test & save").click_accesskit();
    h.run();
    assert!(
        take(&mut h).is_empty(),
        "no account.test without a password"
    );
    type_into(&mut h, "Password", "pw");
    assert!(!h.get_by_label("Test & save").accesskit_node().is_disabled());
}

/// First run: after Continue a new link is refused, so Cancel must exist to drop the current one.
#[test]
fn first_run_link_can_be_cancelled_to_open_a_corrected_one() {
    let mut h = harness_ui(wizard_snapshot(), WIDE, UiState::default());
    h.run();
    offer(
        &mut h,
        link_for("1001", "example.test"),
        Origin::Opened,
        now() - secs(2),
    );
    click_first(&mut h, "Continue");
    assert!(h.query_by_label("Test & continue").is_some());
    assert!(h.query_by_label("Skip, I'll edit config.toml").is_some());
    h.get_by_label("Cancel").click_accesskit();
    h.run();
    assert!(h.state().ui.wizard.link.is_none());
    assert!(
        h.query_by_label("Cancel").is_none(),
        "plain form: no Cancel"
    );
    assert!(take(&mut h).is_empty());
    // The corrected link opens its own Review.
    assert_eq!(
        offer(
            &mut h,
            link_for("1002", "example.test"),
            Origin::Opened,
            now()
        ),
        Offer::Opened
    );
    assert!(h.query_by_label("Check this setup link").is_some());
}

#[test]
fn a_link_on_a_configured_install_is_a_short_flow_that_ends_with_close() {
    let mut h = harness(base_snapshot(), WIDE);
    h.run();
    offer(
        &mut h,
        link_for("2001", "example.test"),
        Origin::Opened,
        now() - secs(2),
    );
    assert!(h.query_by_label("Set up an account").is_some());
    assert!(h.query_by_label_contains("Step 1 of 3").is_none());
    click_first(&mut h, "Continue");
    type_into(&mut h, "Password", "pw");
    h.get_by_label("Test & save").click_accesskit();
    h.run();
    assert_eq!(only_edit(take(&mut h)).method, "account.test");
    deliver(
        &mut h,
        RpcTag::TestDraft,
        Ok(json!({"diagnosis": "ok", "ok": true, "message": "Registered"})),
    );
    let edit = only_edit(take(&mut h));
    assert_eq!(edit.method, "config.edit");
    // A link account is an addition: it never takes the example account's slot.
    assert!(matches!(
        &rpc::edits_of(&edit)[..],
        [ConfigEdit::AddAccount {
            replace_example: false,
            ..
        }]
    ));
    deliver(&mut h, RpcTag::WizardAdd, Ok(json!({"applied": true})));
    assert!(h.query_by_label("Account added").is_some());
    assert!(h.state().ui.wizard.form.password.is_empty());
    // A new link is not taken while the done screen is up.
    let r = offer(
        &mut h,
        link_for("3001", "example.test"),
        Origin::Opened,
        now(),
    );
    assert!(matches!(r, Offer::Refused(_)));
    h.get_by_label("Close").click_accesskit();
    h.run();
    assert!(h.query_by_label("Account added").is_none());
    assert!(rail_shown(&h), "the main window is back");
}

#[test]
fn cancel_at_review_and_at_the_form_sends_nothing_and_drops_the_password() {
    let mut h = harness(base_snapshot(), WIDE);
    h.run();
    offer(
        &mut h,
        link_for("2001", "example.test"),
        Origin::Opened,
        now() - secs(2),
    );
    h.get_by_label("Cancel").click_accesskit();
    h.run();
    assert!(take(&mut h).is_empty());
    assert!(rail_shown(&h));
    assert!(h.state().ui.wizard.link.is_none());
    // At the form, with a password typed.
    offer(
        &mut h,
        link_for("2001", "example.test"),
        Origin::Opened,
        now() - secs(2),
    );
    click_first(&mut h, "Continue");
    type_into(&mut h, "Password", "pw-secret");
    h.get_by_label("Cancel").click_accesskit();
    h.run();
    assert!(take(&mut h).is_empty());
    assert!(h.state().ui.wizard.form.password.is_empty());
    assert!(h.state().ui.wizard.link.is_none());
    assert!(rail_shown(&h));
    // First run: Cancel at Review returns to the plain account form.
    let mut h = harness_ui(wizard_snapshot(), WIDE, UiState::default());
    h.run();
    offer(
        &mut h,
        link_for("1001", "example.test"),
        Origin::Opened,
        now() - secs(2),
    );
    h.get_by_label("Cancel").click_accesskit();
    h.run();
    assert!(h.query_by_label("Add your SIP account").is_some());
    assert!(h.state().ui.wizard.form.username.is_empty());
    assert!(take(&mut h).is_empty());
}

#[test]
fn a_link_for_an_account_you_already_have_cannot_be_added() {
    let mut h = harness(base_snapshot(), WIDE);
    h.run();
    // desk-1001 is user 1001 at pbx.example.test (domain compared case-insensitively).
    offer(
        &mut h,
        link_for("1001", "PBX.Example.Test"),
        Origin::Opened,
        now() - secs(2),
    );
    assert!(
        h.query_by_label_contains("You already have this account")
            .is_some()
    );
    assert!(h.query_by_label("Continue").is_none());
    h.get_by_label("Open Settings").click_accesskit();
    h.run();
    assert!(h.state().ui.settings.open);
    assert_eq!(h.state().ui.settings.account.as_deref(), Some("desk-1001"));
    assert!(h.state().ui.wizard.link.is_none());
    assert!(take(&mut h).is_empty());
}

#[test]
fn a_link_never_replaces_work_in_progress() {
    let mut h = harness_ui(wizard_snapshot(), WIDE, UiState::default());
    h.run();
    type_into(&mut h, "Password", "half-typed");
    let before = h.state().ui.wizard.form.clone();
    let r = offer(
        &mut h,
        link_for("1001", "example.test"),
        Origin::Opened,
        now(),
    );
    assert!(matches!(r, Offer::Refused(_)), "{r:?}");
    assert_eq!(h.state().ui.wizard.form, before, "the form is unchanged");
    assert!(h.query_by_label("Check this setup link").is_none());
    // A pasted one is the user's own doing: it may replace the form.
    let r = offer(
        &mut h,
        link_for("1001", "example.test"),
        Origin::Pasted,
        now(),
    );
    assert_eq!(r, Offer::Opened);
    assert!(h.state().ui.wizard.form.password.is_empty());
    // After Continue and typing a password, links are refused again.
    let mut h = harness_ui(wizard_snapshot(), WIDE, UiState::default());
    h.run();
    offer(
        &mut h,
        link_for("1001", "example.test"),
        Origin::Opened,
        now() - secs(2),
    );
    click_first(&mut h, "Continue");
    type_into(&mut h, "Password", "typed");
    let r = offer(&mut h, link_for("9999", "evil.test"), Origin::Opened, now());
    assert!(matches!(r, Offer::Refused(_)));
    assert_eq!(h.state().ui.wizard.form.username, "1001");
    assert_eq!(h.state().ui.wizard.form.password, "typed");
    // Pasted links too: once continued, the form is not replaced.
    let r = offer(&mut h, link_for("9999", "evil.test"), Origin::Pasted, now());
    assert_eq!(
        r,
        Offer::Refused("Finish or cancel the current setup first")
    );
    assert_eq!(h.state().ui.wizard.form.username, "1001");
    assert_eq!(h.state().ui.wizard.form.password, "typed");
    assert_eq!(
        h.state().ui.wizard.link.as_ref().unwrap().account.username,
        "1001"
    );
    // Audio and done steps and a running test refuse too.
    for step in [WStep::Audio, WStep::Done] {
        h.state_mut().ui.wizard.step = step;
        let r = offer(&mut h, link_for("9999", "evil.test"), Origin::Pasted, now());
        assert!(matches!(r, Offer::Refused(_)), "{step:?}");
    }
    h.state_mut().ui.wizard.step = WStep::Account;
    h.run();
    h.get_by_label("Test & continue").click_accesskit();
    h.run();
    take(&mut h);
    let r = offer(&mut h, link_for("9999", "evil.test"), Origin::Pasted, now());
    assert!(matches!(r, Offer::Refused(_)), "a test is running");
    assert_eq!(h.state().ui.wizard.form.username, "1001");
}

#[test]
fn replacing_the_link_restarts_the_delay_and_warns() {
    let mut h = harness_ui(wizard_snapshot(), WIDE, UiState::default());
    h.run();
    offer(
        &mut h,
        link_for("1001", "example.test"),
        Origin::Opened,
        now() - secs(2),
    );
    assert!(!h.get_by_label("Continue").accesskit_node().is_disabled());
    assert!(h.query_by_label_contains("link changed").is_none());
    let r = offer(
        &mut h,
        link_for("1001", "other.test"),
        Origin::Opened,
        now(),
    );
    assert_eq!(r, Offer::Replaced);
    assert!(h.get_by_label("Continue").accesskit_node().is_disabled());
    assert!(
        h.query_by_label_contains("The setup link changed")
            .is_some()
    );
    assert!(h.query_by_label("Server: pbx.example.test:5061").is_some());
    assert_eq!(
        h.state().ui.wizard.link.as_ref().unwrap().account.domain,
        "other.test"
    );
}

#[test]
fn a_window_that_was_away_starts_the_delay_again() {
    let mut h = harness_ui(wizard_snapshot(), WIDE, UiState::default());
    h.run();
    offer(
        &mut h,
        link_for("1001", "example.test"),
        Origin::Opened,
        now(),
    );
    // Drawn, then not drawn for a while, while Continue was still locked: the delay starts again.
    h.state_mut().ui.wizard.review_seen = Some(now() - secs(30));
    h.state_mut().ui.wizard.review_at = Some(now() - chrono::Duration::milliseconds(900));
    h.run_steps(2);
    assert_eq!(h.state().ui.wizard.review_at, Some(now()));
    assert!(h.get_by_label("Continue").accesskit_node().is_disabled());
}

#[test]
fn the_wizard_paste_field_opens_the_review_or_says_what_is_wrong() {
    let mut h = harness_ui(wizard_snapshot(), WIDE, UiState::default());
    h.run();
    h.get_by_label("Paste a setup link").click_accesskit();
    h.run();
    type_into(&mut h, "Setup link", "https://example.test/not-a-link");
    h.get_by_label("Review link").click_accesskit();
    h.run();
    assert!(h.state().ui.wizard.paste_error.is_some());
    assert!(h.query_by_label("Check this setup link").is_none());
    let good = link_for("1001", "example.test").encode().unwrap();
    h.state_mut().ui.wizard.paste = Some(format!("<{good}>"));
    h.run();
    h.get_by_label("Review link").click_accesskit();
    h.run();
    assert!(h.query_by_label("Check this setup link").is_some());
    assert!(h.state().ui.wizard.paste.is_none());
    assert!(take(&mut h).is_empty());
    // A call link is not a setup link.
    let mut h = harness_ui(wizard_snapshot(), WIDE, UiState::default());
    h.run();
    h.state_mut().ui.wizard.paste = Some("sip:bob@example.test".into());
    h.run();
    h.get_by_label("Review link").click_accesskit();
    h.run();
    assert!(
        h.state()
            .ui
            .wizard
            .paste_error
            .as_deref()
            .unwrap()
            .contains("call link")
    );
}

#[test]
fn settings_copy_setup_link_asks_for_a_link_without_a_password() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    h.get_by_label("Copy setup link").click_accesskit();
    h.run();
    let req = only_edit(take(&mut h));
    assert_eq!(req.method, "provision.build");
    assert_eq!(req.params, json!({"account": "desk-1001"}));
    assert_eq!(req.tag, RpcTag::ShareLink("desk-1001".into()));
    deliver(
        &mut h,
        RpcTag::ShareLink("desk-1001".into()),
        Ok(
            json!({"link": "siple:provision?d=e30", "length": 21, "fields": {}, "omitted": ["password", "bind_address"]}),
        ),
    );
    assert_eq!(
        h.state().ui.settings.copy_pending,
        None,
        "copied on the frame after the reply"
    );
    assert!(
        h.query_by_label_contains("Setup link copied. It has no password.")
            .is_some()
    );
    assert!(
        h.query_by_label_contains("Not included: password, bind_address")
            .is_some()
    );
    // Unsaved edits disable it, like Test.
    h.state_mut().ui.settings.set_field(
        "desk-1001",
        AccountField::AutoAnswer,
        &EditValue::Bool(false),
        EditValue::Bool(true),
        "hash-0",
    );
    h.run();
    assert!(
        h.get_by_label("Copy setup link")
            .accesskit_node()
            .is_disabled()
    );
}

#[test]
fn settings_add_from_setup_link_opens_the_link_only_wizard() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    h.get_by_label("Add from setup link").click_accesskit();
    h.run();
    let good = link_for("2001", "example.test").encode().unwrap();
    type_into(&mut h, "Setup link", &good);
    h.get_by_label("Review link").click_accesskit();
    h.run();
    assert!(h.query_by_label("Check this setup link").is_some());
    assert!(h.state().ui.wizard.link_only);
    assert_eq!(
        h.state().ui.settings.select_after.as_deref(),
        Some("2001.example.test")
    );
    assert!(take(&mut h).is_empty());
    // Cancel returns to Settings.
    h.get_by_label("Cancel").click_accesskit();
    h.run();
    assert!(h.query_by_label("Check this setup link").is_none());
    assert!(
        h.query_by_label("Run setup again").is_some(),
        "Settings is back"
    );
}

#[test]
fn an_unparsable_config_blocks_continue() {
    let mut snap = wizard_snapshot();
    snap.config_unparsable = true;
    let mut h = harness_ui(snap, WIDE, UiState::default());
    h.run();
    offer(
        &mut h,
        link_for("1001", "example.test"),
        Origin::Opened,
        now() - secs(2),
    );
    assert!(
        h.query_by_label_contains("Fix the config file first")
            .is_some()
    );
    assert!(h.get_by_label("Continue").accesskit_node().is_disabled());
}

#[test]
fn a_notice_is_visible_while_the_wizard_is_up() {
    let mut h = harness_ui(wizard_snapshot(), WIDE, UiState::default());
    h.state_mut().notice = Some(crate::snapshot::Notice::plain(
        "Ignored link: this is not a Siple setup link",
    ));
    h.run();
    assert!(
        h.query_by_label_contains("Ignored link: this is not a Siple setup link")
            .is_some()
    );
}

// ---- review fixes -----------------------------------------------------------------------------

fn consult_with(phase: TransferPhase, select: &str) -> Harness<'static, State> {
    let mut snap = transfer_consult();
    snap.transfers = vec![session(phase)];
    let mut h = harness(snap, WIDE);
    h.state_mut().ui.selected_line = Some(select.into());
    h.run_steps(4);
    h
}

fn is_disabled(h: &Harness<'_, State>, label: &str) -> bool {
    h.query_all_by_label_contains(label)
        .next()
        .unwrap_or_else(|| panic!("no widget labelled {label:?}"))
        .accesskit_node()
        .is_disabled()
}

/// 1: the dialing consult leg's Cancel is the session cancel, labelled like the panel's.
#[test]
fn dialing_consult_leg_cancel_is_the_session_cancel() {
    let mut snap = transfer_consult();
    snap.calls[1].state = CallState::Dialing;
    snap.calls[1].answered_at = None;
    snap.transfers = vec![session(TransferPhase::ConsultDialing)];
    let mut h = harness(snap, WIDE);
    h.state_mut().ui.selected_line = Some("c2".into());
    h.run_steps(4);
    // The stage card and the panel both say it.
    assert_eq!(
        h.query_all_by_label_contains("Cancel - return to Bob Martin")
            .count(),
        2
    );
    let stage_btn = h
        .query_all_by_label_contains("Cancel - return to Bob Martin")
        .last()
        .unwrap();
    stage_btn.click_accesskit();
    h.run_steps(4);
    assert_eq!(take(&mut h), [UiAction::TransferCancel("t1".into())]);
}

/// 2: in a live session only a connected consult swaps; every other phase only selects.
#[test]
fn rail_click_in_a_live_session_never_activates() {
    for phase in [
        TransferPhase::Referring,
        TransferPhase::Transferring,
        TransferPhase::Unconfirmed,
        TransferPhase::ConsultDialing,
        TransferPhase::ConsultRinging,
        TransferPhase::ConsultFailed,
    ] {
        let mut h = consult_with(phase, "c2");
        h.get_by_label_contains("Line 1, Bob Martin")
            .click_accesskit();
        h.run_steps(4);
        assert!(take(&mut h).is_empty(), "{phase:?}");
        assert_eq!(h.state().ui.selected_line.as_deref(), Some("c1"));
    }
}

/// 3: resuming a held line goes through activate (or the session swap), never a raw unhold.
#[test]
fn resume_uses_activate_or_swap_not_a_raw_unhold() {
    let mut h = consult_with(TransferPhase::Unconfirmed, "c1");
    click_first(&mut h, "Resume Bob Martin");
    assert_eq!(take(&mut h), [UiAction::Activate("c1".into())]);
    // Connected consult, A held: the stage's Resume swaps the session.
    let mut h = consult_with(TransferPhase::ConsultConnected, "c1");
    click_first(&mut h, "Resume");
    assert_eq!(take(&mut h), [UiAction::TransferSwap("t1".into())]);
    // No session: activate.
    let mut h = harness(two_calls(), WIDE);
    h.state_mut().ui.selected_line = Some("c2".into());
    h.run();
    click_first(&mut h, "Resume");
    assert_eq!(take(&mut h), [UiAction::Activate("c2".into())]);
}

/// 4: a failed REFER that left the consult connected says so inline.
#[test]
fn connected_consult_shows_a_failed_refer() {
    let mut snap = transfer_consult();
    let mut s = session(TransferPhase::ConsultConnected);
    s.last_error = Some(siple_sip::TransferError {
        stage: "refer".into(),
        code: 486,
        reason: "Busy Here".into(),
    });
    snap.transfers = vec![s];
    let mut h = harness(snap, WIDE);
    h.run();
    assert!(
        h.query_by_label_contains("Transfer failed: Busy. You're still talking to Carol Diaz")
            .is_some()
    );
}

/// 5: prefill opens the dial bar even while lines exist.
#[test]
fn blf_prefill_opens_the_dial_bar_with_lines() {
    let mut snap = two_calls();
    snap.ui.blf_click = BlfClick::Prefill;
    snap.ui.blf_click_in_call = BlfClickInCall::Menu;
    // Everything held: a click is not an in-call click.
    snap.calls[0].state = CallState::Held;
    snap.calls[0].hold.local = true;
    snap.active_line = None;
    let mut h = harness_ext(snap, WIDE);
    h.run();
    assert!(h.query_by_role(egui::accesskit::Role::TextInput).is_none());
    h.get_by_label("Reception (desk-1001)").click_accesskit();
    h.run();
    assert!(h.state().actions.is_empty());
    assert_eq!(h.state().ui.dial_target, "1002");
    assert!(h.state().ui.new_call_open);
    assert!(h.query_by_role(egui::accesskit::Role::TextInput).is_some());
}

/// A link (`sip:` / `tel:` click) prefills the dial field, opens the bar even with lines up,
/// focuses it, and never dials by itself.
#[test]
fn link_prefill_fills_and_focuses_the_dial_field_without_dialling() {
    let mut h = harness(two_calls(), WIDE);
    h.run();
    assert!(h.query_by_role(egui::accesskit::Role::TextInput).is_none());
    {
        let ui = &mut h.state_mut().ui;
        ui.dial_target = "sip:bob@example.test".into();
        ui.new_call_open = true;
        ui.focus_dial = true;
    }
    h.run();
    assert!(h.state().actions.is_empty(), "a link must not dial");
    assert_eq!(h.state().ui.dial_target, "sip:bob@example.test");
    let edit = h
        .query_by_role(egui::accesskit::Role::TextInput)
        .expect("dial field is open");
    assert!(edit.is_focused());
    assert!(!h.state().ui.focus_dial, "focus request is consumed");
}

/// 6: the narrow extensions tab keeps the pinned banners.
#[test]
fn narrow_extensions_tab_keeps_call_waiting_and_config_banners() {
    let mut snap = call_waiting();
    snap.config_error = Some(crate::snapshot::ConfigErrorView {
        at: "2026-10-08T09:29:41+00:00".into(),
        errors: vec!["bad".into()],
    });
    let mut h = harness_ext(snap, NARROW);
    h.run();
    assert!(h.query_by_label("EXTENSIONS").is_some());
    assert!(h.query_by_label("Config not applied").is_some());
    click_first(&mut h, "Answer & hold Bob Martin");
    assert_eq!(take(&mut h), [UiAction::AnswerHold("c3".into())]);
}

/// 7: "Leave now" hangs up the leg the REFER went out on.
#[test]
fn leave_now_hangs_up_the_refer_leg() {
    let mut snap = transfer_consult();
    let mut s = session(TransferPhase::Transferring);
    s.refer_call = Some("c2".into());
    snap.transfers = vec![s];
    let mut h = harness(snap, WIDE);
    h.state_mut().ui.selected_line = Some("c1".into());
    h.run_steps(4);
    click_first(&mut h, "Leave now");
    assert_eq!(take(&mut h), [UiAction::Hangup("c2".into())]);
}

/// 8: nobody talking (both held): explicit Resume / Talk to buttons and rail clicks activate.
#[test]
fn consult_with_both_legs_held_offers_explicit_resume() {
    let mut snap = transfer_consult();
    snap.calls[1].state = CallState::Held;
    snap.calls[1].hold.local = true;
    snap.active_line = None;
    let mut s = session(TransferPhase::ConsultConnected);
    s.talking_to = None;
    snap.transfers = vec![s];
    let mut h = harness(snap, WIDE);
    h.state_mut().ui.selected_line = Some("c1".into());
    h.run();
    assert!(h.query_by_label_contains("Swap to").is_none());
    click_first(&mut h, "Resume Bob Martin");
    assert_eq!(take(&mut h), [UiAction::Activate("c1".into())]);
    click_first(&mut h, "Talk to Carol Diaz");
    assert_eq!(take(&mut h), [UiAction::Activate("c2".into())]);
    h.get_by_label_contains("Line 2,").click_accesskit();
    h.run();
    assert_eq!(take(&mut h), [UiAction::Activate("c2".into())]);
}

/// 9: the composer closes with its call or when the call enters a session.
#[test]
fn composer_closes_when_its_call_ends_or_enters_a_session() {
    let mut h = composer_harness(TransferEnter::Consult);
    h.state_mut().snap.calls[0].state = CallState::Ended;
    h.state_mut().snap.calls[0].ended_at = Some(now());
    h.run();
    assert!(h.state().ui.composer.is_none());
    let mut h = composer_harness(TransferEnter::Consult);
    h.state_mut().snap.transfers = vec![session(TransferPhase::ConsultRinging)];
    h.run_steps(3);
    assert!(h.state().ui.composer.is_none());
}

/// 10: suggestions only list keys of the call's own account.
#[test]
fn composer_suggests_only_the_calls_account() {
    let mut snap = two_calls();
    snap.blf[0].account = "lab-tls".into();
    let mut h = harness(snap, WIDE);
    h.run();
    click_first(&mut h, "Transfer");
    assert!(h.query_by_label_contains("Transfer to Reception").is_none());
    assert!(
        h.query_by_label_contains("Transfer to Bob Martin")
            .is_some()
    );
}

/// 11: a muted held line can still be unmuted.
#[test]
fn mute_stays_enabled_on_a_muted_held_line() {
    let mut snap = two_calls();
    snap.calls[1].muted = true;
    let mut h = harness(snap, WIDE);
    h.state_mut().ui.selected_line = Some("c2".into());
    h.run();
    assert!(!is_disabled(&h, "Unmute"));
}

// ---- settings and wizard --------------------------------------------------------------------

use crate::ui::rpc::{self, RpcError, RpcReply, RpcRequest, RpcTag};
use crate::ui::settings::{CONFLICT_MSG, Section};
use serde_json::json;
use siple_core::config::BlfConfig;
use siple_core::config::edit::{AccountField, ConfigEdit, EditValue};

fn settings_harness(snap: Snapshot, section: Section, size: egui::Vec2) -> Harness<'static, State> {
    let mut h = harness(snap, size);
    h.state_mut().ui.settings.open = true;
    h.state_mut().ui.settings.section = section;
    h.run();
    h
}

fn deliver(h: &mut Harness<'_, State>, tag: RpcTag, result: Result<serde_json::Value, RpcError>) {
    let snap = h.state().snap.clone();
    h.state_mut().ui.deliver(RpcReply { tag, result }, &snap);
    h.run();
}

/// Clicks the switch / checkbox named `label` (its visible caption is a second node).
fn click_switch(h: &mut Harness<'_, State>, label: &str) {
    h.get_by_role_and_label(egui::accesskit::Role::CheckBox, label)
        .click_accesskit();
    h.run();
}

/// Types into the first password field (the add form sits above the selected account's form).
fn type_into_first_password(h: &mut Harness<'_, State>, text: &str) {
    let n = h
        .query_all_by_role(egui::accesskit::Role::PasswordInput)
        .next()
        .expect("password field");
    n.focus();
    n.type_text(text);
    h.run();
}

fn type_into(h: &mut Harness<'_, State>, label: &str, text: &str) {
    let n = h.get_by_label(label);
    n.focus();
    n.type_text(text);
    h.run();
}

/// The one `config.edit` request among the actions.
fn only_edit(actions: Vec<UiAction>) -> RpcRequest {
    let mut reqs: Vec<RpcRequest> = actions
        .into_iter()
        .filter_map(|a| match a {
            UiAction::Rpc(r) => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    reqs.remove(0)
}

fn set_account(id: &str, field: AccountField, value: EditValue) -> ConfigEdit {
    ConfigEdit::SetAccount {
        id: id.into(),
        field,
        value,
    }
}

#[test]
fn gear_opens_settings_and_back_returns() {
    let mut h = harness(base_snapshot(), WIDE);
    h.run();
    h.get_by_label("Settings").click_accesskit();
    h.run();
    assert!(h.state().ui.settings.open);
    assert!(!rail_shown(&h));
    for s in [
        "Accounts",
        "Audio",
        "Calls & lines",
        "Extensions",
        "Shortcuts",
        "Appearance",
        "Notifications",
        "Advanced",
        "Logging & privacy",
        "Developer",
    ] {
        assert!(
            h.query_by_role_and_label(egui::accesskit::Role::Button, s)
                .is_some(),
            "section {s}"
        );
    }
    h.get_by_label_contains("Back").click_accesskit();
    h.run();
    assert!(!h.state().ui.settings.open);
    assert!(rail_shown(&h));
}

#[test]
fn section_list_follows_keyboard_focus() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    h.get_by_label("Audio").focus();
    h.run();
    assert_eq!(h.state().ui.settings.section, Section::Audio);
    assert!(h.query_by_label("Devices").is_some());
    h.get_by_label("Appearance").click_accesskit();
    h.run();
    assert_eq!(h.state().ui.settings.section, Section::Appearance);
}

#[test]
fn live_call_strip_stays_in_settings() {
    let mut h = settings_harness(two_calls(), Section::Accounts, WIDE);
    h.get_by_label("Unmute").click_accesskit();
    h.get_by_label_contains("Hang up").click_accesskit();
    h.run();
    assert_eq!(
        take(&mut h),
        [
            UiAction::Mute("c1".into(), false),
            UiAction::Hangup("c1".into())
        ]
    );
}

#[test]
fn apply_batches_one_edit_list_and_revert_restores() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    type_into(&mut h, "Display name", " X");
    click_switch(&mut h, "Auto-answer");
    assert!(take(&mut h).is_empty(), "nothing is sent before Apply");
    assert!(h.query_by_label_contains("2 unsaved changes").is_some());
    // Revert drops the staged edits.
    h.get_by_label("Revert").click_accesskit();
    h.run();
    assert!(!h.state().ui.settings.dirty());
    assert!(take(&mut h).is_empty());
    // Stage again and apply: one config.edit with the file hash and both edits.
    type_into(&mut h, "Display name", " X");
    click_switch(&mut h, "Auto-answer");
    h.get_by_label("Apply").click_accesskit();
    h.run();
    let req = only_edit(take(&mut h));
    assert_eq!(
        (req.method, req.params["base_hash"].as_str()),
        ("config.edit", Some("hash-0"))
    );
    assert_eq!(
        rpc::edits_of(&req),
        [
            set_account(
                "desk-1001",
                AccountField::DisplayName,
                EditValue::Str("Desk 1001 X".into())
            ),
            set_account("desk-1001", AccountField::AutoAnswer, EditValue::Bool(true)),
        ]
    );
    // The success reply clears the form.
    deliver(&mut h, RpcTag::Apply, Ok(json!({"applied": true})));
    assert!(!h.state().ui.settings.dirty());
}

#[test]
fn a_value_edited_back_is_not_staged() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    click_switch(&mut h, "Auto-answer");
    assert!(h.state().ui.settings.dirty());
    click_switch(&mut h, "Auto-answer");
    assert!(!h.state().ui.settings.dirty());
}

#[test]
fn password_is_staged_and_goes_into_the_same_edit() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    type_into(&mut h, "Password", "s3cret");
    h.get_by_label("Apply").click_accesskit();
    h.run();
    let req = only_edit(take(&mut h));
    assert_eq!(
        rpc::edits_of(&req),
        [set_account(
            "desk-1001",
            AccountField::Password,
            EditValue::Str("s3cret".into())
        )]
    );
}

fn keyring_snapshot() -> Snapshot {
    let mut snap = base_snapshot();
    let mut cfg = (*snap.cfg).clone();
    cfg.accounts[0].password_store = siple_core::config::PasswordStore::Keyring;
    snap.cfg = std::sync::Arc::new(cfg);
    snap
}

fn set_password_req(actions: &[UiAction]) -> Option<&RpcRequest> {
    actions.iter().find_map(|a| match a {
        UiAction::Rpc(r) if r.method == "account.set_password" => Some(r),
        _ => None,
    })
}

#[test]
fn keyring_password_is_stored_before_the_edit_for_an_account_already_on_keyring() {
    let mut h = settings_harness(keyring_snapshot(), Section::Accounts, WIDE);
    assert!(h.query_by_label_contains("Stored in Keychain").is_some());
    type_into(&mut h, "Password", "pw2");
    type_into(&mut h, "Display name", "!");
    h.get_by_label("Apply").click_accesskit();
    h.run();
    // The keychain is written first, so the new fields never register with the old password.
    let acts = take(&mut h);
    assert_eq!(acts.len(), 1, "{acts:?}");
    let r = set_password_req(&acts).expect("set_password first");
    assert_eq!(r.params, json!({"account": "desk-1001", "password": "pw2"}));
    deliver(
        &mut h,
        RpcTag::SetPassword("desk-1001".into()),
        Ok(json!({"ok": true})),
    );
    let req = only_edit(take(&mut h));
    assert_eq!(req.method, "config.edit");
    assert!(rpc::edits_of(&req).iter().all(|e| !matches!(
        e,
        ConfigEdit::SetAccount {
            field: AccountField::Password,
            ..
        }
    )));
    deliver(&mut h, RpcTag::Apply, Ok(json!({"applied": true})));
    assert!(take(&mut h).is_empty(), "no second set_password");
    assert!(!h.state().ui.settings.dirty());
}

#[test]
fn a_failed_keychain_write_stops_the_apply_and_keeps_the_edits() {
    let mut h = settings_harness(keyring_snapshot(), Section::Accounts, WIDE);
    type_into(&mut h, "Password", "pw2");
    type_into(&mut h, "Display name", "!");
    h.get_by_label("Apply").click_accesskit();
    h.run();
    take(&mut h);
    deliver(
        &mut h,
        RpcTag::SetPassword("desk-1001".into()),
        Err(RpcError::new(
            "invalid_state",
            "keyring unavailable",
            json!(null),
        )),
    );
    assert!(take(&mut h).is_empty(), "the edit is not sent");
    assert!(h.state().ui.settings.dirty());
    assert!(!h.state().ui.settings.applying);
}

#[test]
fn switching_to_keyring_in_an_apply_stores_the_password_after_the_switch() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    click_switch(&mut h, "Store password in system keychain");
    type_into(&mut h, "Password", "pw3");
    h.get_by_label("Apply").click_accesskit();
    h.run();
    let acts = take(&mut h);
    assert!(
        set_password_req(&acts).is_none(),
        "the edit (which moves the old password) goes first"
    );
    assert_eq!(only_edit(acts).method, "config.edit");
    deliver(&mut h, RpcTag::Apply, Ok(json!({"applied": true})));
    let acts = take(&mut h);
    assert_eq!(set_password_req(&acts).unwrap().params["password"], "pw3");
}

#[test]
fn a_reverted_password_is_never_written_by_a_later_add() {
    let mut h = settings_harness(keyring_snapshot(), Section::Accounts, WIDE);
    type_into(&mut h, "Password", "reverted-pw");
    type_into(&mut h, "Display name", "!");
    h.get_by_label("Apply").click_accesskit();
    h.run();
    take(&mut h);
    deliver(
        &mut h,
        RpcTag::SetPassword("desk-1001".into()),
        Ok(json!({"ok": true})),
    );
    take(&mut h);
    deliver(
        &mut h,
        RpcTag::Apply,
        Err(RpcError::new(
            "invalid_params",
            "invalid",
            json!({"reason": "invalid", "errors": [{"path": "account[0].display_name", "message": "bad"}]}),
        )),
    );
    h.get_by_label("Revert").click_accesskit();
    h.run();
    h.get_by_label("New account").click_accesskit();
    h.run();
    type_into(&mut h, "Username / extension", "4001");
    type_into(&mut h, "Domain / server", "pbx2.example.test");
    h.get_by_label("Add account").click_accesskit();
    h.run();
    take(&mut h);
    deliver(&mut h, RpcTag::AddAccount, Ok(json!({"applied": true})));
    let acts = take(&mut h);
    assert!(set_password_req(&acts).is_none(), "{acts:?}");
}

#[test]
fn an_in_flight_add_keeps_its_keychain_password_when_an_apply_fails() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    h.get_by_label("New account").click_accesskit();
    h.run();
    type_into(&mut h, "Username / extension", "4001");
    type_into_first_password(&mut h, "addpw");
    type_into(&mut h, "Domain / server", "pbx2.example.test");
    h.query_all_by_role_and_label(
        egui::accesskit::Role::CheckBox,
        "Store password in system keychain",
    )
    .next()
    .expect("add form switch")
    .click_accesskit();
    h.run();
    h.get_by_label("Add account").click_accesskit();
    h.run();
    take(&mut h);
    // An unrelated failed Apply must not clear the add's password.
    deliver(
        &mut h,
        RpcTag::Apply,
        Err(RpcError::new(
            "invalid_params",
            "invalid",
            json!({"reason": "invalid", "errors": []}),
        )),
    );
    deliver(&mut h, RpcTag::AddAccount, Ok(json!({"applied": true})));
    let acts = take(&mut h);
    assert_eq!(set_password_req(&acts).unwrap().params["password"], "addpw");
}

#[test]
fn the_form_is_locked_while_an_apply_is_in_flight() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    type_into(&mut h, "Display name", "!");
    h.get_by_label("Apply").click_accesskit();
    h.run();
    assert!(
        h.get_by_label("Display name")
            .accesskit_node()
            .is_disabled()
    );
}

#[test]
fn instant_toggles_dispatch_immediately() {
    let mut h = settings_harness(base_snapshot(), Section::Notifications, WIDE);
    click_switch(&mut h, "Desktop notifications");
    let req = only_edit(take(&mut h));
    assert_eq!(req.params.get("base_hash"), None);
    assert_eq!(
        rpc::edits_of(&req),
        [ConfigEdit::SetSection {
            section: "ui".into(),
            key: "notifications".into(),
            value: EditValue::Bool(false)
        }]
    );
    assert!(!h.state().ui.settings.dirty());
    // Appearance: reduced motion is a picker; open it and choose "On".
    let mut h = settings_harness(base_snapshot(), Section::Appearance, WIDE);
    h.get_by_label("Reduced motion").click_accesskit();
    h.run();
    h.get_by_label("On").click_accesskit();
    h.run();
    let req = only_edit(take(&mut h));
    assert_eq!(
        rpc::edits_of(&req),
        [ConfigEdit::SetSection {
            section: "ui".into(),
            key: "reduced_motion".into(),
            value: EditValue::Str("on".into())
        }]
    );
}

#[test]
fn conflict_keeps_the_edits_and_says_so() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    type_into(&mut h, "Display name", "!");
    h.get_by_label("Apply").click_accesskit();
    h.run();
    take(&mut h);
    deliver(
        &mut h,
        RpcTag::Apply,
        Err(RpcError::new(
            "invalid_state",
            "the config file changed",
            json!({"reason": "conflict", "current_hash": "hash-9"}),
        )),
    );
    assert!(h.query_by_label(CONFLICT_MSG).is_some());
    let s = &h.state().ui.settings;
    assert!(s.dirty(), "edits are kept");
    assert_eq!(s.base_hash.as_deref(), Some("hash-9"));
    // Applying again uses the new hash.
    h.get_by_label("Apply").click_accesskit();
    h.run();
    assert_eq!(only_edit(take(&mut h)).params["base_hash"], "hash-9");
}

#[test]
fn an_external_change_while_editing_offers_reload_or_keep() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    type_into(&mut h, "Display name", "!");
    h.state_mut().snap.config_hash = "hash-1".into();
    h.run();
    assert!(h.query_by_label("The config file changed.").is_some());
    h.get_by_label("Keep mine").click_accesskit();
    h.run();
    assert!(h.query_by_label("The config file changed.").is_none());
    assert!(h.state().ui.settings.dirty());
    h.state_mut().snap.config_hash = "hash-2".into();
    h.run();
    h.get_by_label("Reload").click_accesskit();
    h.run();
    assert!(!h.state().ui.settings.dirty());
}

#[test]
fn unparsable_file_makes_settings_read_only() {
    let mut snap = base_snapshot();
    snap.config_unparsable = true;
    snap.config_error = Some(crate::snapshot::ConfigErrorView {
        at: "2026-10-08T09:29:41.512+00:00".into(),
        errors: vec!["expected `=` at line 3".into()],
    });
    let h = settings_harness(snap, Section::Accounts, WIDE);
    assert!(h.query_by_label("Config not applied").is_some());
    assert!(h.query_by_label_contains("read-only").is_some());
    assert!(
        h.get_by_label("Display name")
            .accesskit_node()
            .is_disabled()
    );
    assert!(h.get_by_label("New account").accesskit_node().is_disabled());
    // A file that parses but is invalid stays editable.
    let mut snap = base_snapshot();
    snap.config_error = Some(crate::snapshot::ConfigErrorView {
        at: "2026-10-08T09:29:41.512+00:00".into(),
        errors: vec!["bad".into()],
    });
    let h = settings_harness(snap, Section::Accounts, WIDE);
    assert!(
        !h.get_by_label("Display name")
            .accesskit_node()
            .is_disabled()
    );
}

#[test]
fn engine_errors_show_next_to_the_field() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    type_into(&mut h, "Username", "x");
    h.get_by_label("Apply").click_accesskit();
    h.run();
    take(&mut h);
    deliver(
        &mut h,
        RpcTag::Apply,
        Err(RpcError::new(
            "invalid_params",
            "invalid",
            json!({"reason": "invalid", "errors": [{"path": "account[0].username", "message": "must not contain spaces"}]}),
        )),
    );
    assert!(h.query_by_label("must not contain spaces").is_some());
    assert!(h.state().ui.settings.dirty(), "the edits stay for a fix");
}

#[test]
fn test_registration_renders_each_diagnosis() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    h.get_by_label("Test registration").click_accesskit();
    h.run();
    let req = only_edit(take(&mut h));
    assert_eq!(
        (req.method, &req.params),
        ("account.test", &json!({"account": "desk-1001"}))
    );
    for (diag, msg) in [
        ("ok", "Registered via UDP in 0.8 s"),
        (
            "auth_failed",
            "Wrong username or password (the server answered 403)",
        ),
        ("not_found", "User not found on this server"),
        (
            "timeout",
            "No response from the server; with UDP, try TCP or TLS",
        ),
        ("dns_failed", "Cannot find the server: nxdomain"),
        (
            "network_unreachable",
            "No network route to the server: unreachable",
        ),
    ] {
        deliver(
            &mut h,
            RpcTag::TestAccount("desk-1001".into()),
            Ok(json!({"diagnosis": diag, "ok": diag == "ok", "message": msg})),
        );
        assert!(h.query_by_label(msg).is_some(), "{diag}");
        assert!(h.query_by_label("Trust this certificate").is_none());
    }
}

#[test]
fn trust_this_certificate_writes_tls_fingerprint() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    let fp = "ab".repeat(32);
    deliver(
        &mut h,
        RpcTag::TestAccount("desk-1001".into()),
        Ok(
            json!({"diagnosis": "tls_untrusted", "ok": false, "fingerprint": fp,
            "message": format!("The server's TLS certificate is not trusted; its SHA-256 fingerprint is {fp}")}),
        ),
    );
    h.get_by_label("Trust this certificate").click_accesskit();
    h.run();
    let req = only_edit(take(&mut h));
    assert_eq!(
        rpc::edits_of(&req),
        [set_account(
            "desk-1001",
            AccountField::TlsFingerprint,
            EditValue::Str(fp.clone())
        )]
    );
    h.get_by_label_contains("Turn off verification")
        .click_accesskit();
    h.run();
    assert_eq!(
        rpc::edits_of(&only_edit(take(&mut h))),
        [set_account(
            "desk-1001",
            AccountField::TlsVerify,
            EditValue::Bool(false)
        )]
    );
}

#[test]
fn staged_changes_disable_the_registration_test() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    type_into(&mut h, "Display name", "!");
    assert!(
        h.get_by_label("Test registration")
            .accesskit_node()
            .is_disabled()
    );
}

#[test]
fn add_account_form_sends_add_account() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    h.get_by_label("New account").click_accesskit();
    h.run();
    type_into(&mut h, "Username / extension", "4001");
    type_into(&mut h, "Domain / server", "pbx2.example.test");
    h.get_by_label("Add account").click_accesskit();
    h.run();
    let req = only_edit(take(&mut h));
    let edits = rpc::edits_of(&req);
    let [
        ConfigEdit::AddAccount {
            draft,
            replace_example,
        },
    ] = &edits[..]
    else {
        panic!("{edits:?}")
    };
    assert_eq!(
        (draft.id.as_str(), *replace_example),
        ("4001.pbx2.example.test", false)
    );
    assert_eq!(
        draft.fields.get(&AccountField::Username),
        Some(&EditValue::Str("4001".into()))
    );
}

#[test]
fn move_passwords_to_keychain_is_one_instant_edit() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    h.get_by_label("Move passwords to keychain")
        .click_accesskit();
    h.run();
    let edits = rpc::edits_of(&only_edit(take(&mut h)));
    assert_eq!(edits.len(), 3);
    assert!(edits.iter().all(|e| matches!(e, ConfigEdit::SetAccount { field: AccountField::PasswordStore, value: EditValue::Str(s), .. } if s == "keyring")));
}

fn blf_ids(h: &Harness<'_, State>) -> Vec<String> {
    h.state()
        .ui
        .settings
        .blf
        .values()
        .next()
        .map(|l| l.iter().map(|e| e.uri.clone()).collect())
        .unwrap_or_default()
}

#[test]
fn extensions_reorder_with_buttons_and_keyboard() {
    let mut h = settings_harness(base_snapshot(), Section::Extensions, WIDE);
    h.get_by_label("Move down Reception").click_accesskit();
    h.run();
    assert_eq!(blf_ids(&h), ["1003", "1002", "1004"]);
    h.get_by_label("Move up Support queue").click_accesskit();
    h.run();
    assert_eq!(blf_ids(&h), ["1003", "1004", "1002"]);
    // Keyboard: Alt+Down in the first entry's field.
    let f = h.get_by_label("Extension 1");
    f.focus();
    h.run();
    h.key_press_modifiers(egui::Modifiers::ALT, egui::Key::ArrowDown);
    h.run();
    assert_eq!(blf_ids(&h), ["1004", "1003", "1002"]);
    h.get_by_label("Apply").click_accesskit();
    h.run();
    let edits = rpc::edits_of(&only_edit(take(&mut h)));
    let [ConfigEdit::SetBlf { account, entries }] = &edits[..] else {
        panic!("{edits:?}")
    };
    assert_eq!(account, "desk-1001");
    let want: Vec<BlfConfig> = ["1004", "1003", "1002"]
        .iter()
        .map(|u| {
            let mut e = base_blf("desk-1001")
                .into_iter()
                .find(|e| e.uri == *u)
                .unwrap();
            e.uri = (*u).into();
            e
        })
        .collect();
    assert_eq!(entries, &want);
}

fn base_blf(account: &str) -> Vec<BlfConfig> {
    crate::visual_tests::base_config()
        .accounts
        .into_iter()
        .find(|a| a.id == account)
        .unwrap()
        .blf
}

#[test]
fn extensions_reverting_the_order_unstages_it() {
    let mut h = settings_harness(base_snapshot(), Section::Extensions, WIDE);
    h.get_by_label("Move down Reception").click_accesskit();
    h.run();
    assert!(h.state().ui.settings.dirty());
    h.get_by_label("Move up Reception").click_accesskit();
    h.run();
    assert!(!h.state().ui.settings.dirty());
}

#[test]
fn extension_field_errors_show_by_path() {
    let mut h = settings_harness(base_snapshot(), Section::Extensions, WIDE);
    type_into(&mut h, "Extension 2", "x");
    h.get_by_label("Apply").click_accesskit();
    h.run();
    take(&mut h);
    deliver(
        &mut h,
        RpcTag::Apply,
        Err(RpcError::new(
            "invalid_params",
            "invalid",
            json!({"reason": "invalid", "errors": [{"path": "account[0].blf[1].uri", "message": "duplicate uri"}]}),
        )),
    );
    assert!(h.query_all_by_label("duplicate uri").count() >= 1);
}

#[test]
fn audio_settings_show_the_live_meter_and_run_the_echo_test() {
    let mut h = settings_harness(base_snapshot(), Section::Audio, WIDE);
    h.state_mut().live.mic_dbfs = Some(-30.0);
    h.run_steps(4);
    assert!(h.query_by_label("Microphone level: -30 dBFS").is_some());
    assert_eq!(crate::ui::meter_segments_for_tests(-30.0), 12);
    assert_eq!(crate::ui::meter_segments_for_tests(-90.0), 0);
    assert_eq!(crate::ui::meter_segments_for_tests(3.0), 24);
    h.get_by_label_contains("Test speaker").click_accesskit();
    h.get_by_label_contains("Echo test").click_accesskit();
    h.run_steps(4);
    let acts = take(&mut h);
    assert_eq!(acts[0], UiAction::TestSpeaker);
    let UiAction::Rpc(r) = &acts[1] else {
        panic!("{acts:?}")
    };
    assert_eq!(
        (r.method, &r.params),
        ("audio.echo_test", &json!({"action": "start", "seconds": 3}))
    );
    // Progress and a silent result are explained.
    h.state_mut().live.echo.phase = siple_core::media::EchoPhase::Recording;
    h.run_steps(4);
    assert!(h.query_by_label_contains("Recording").is_some());
    h.state_mut().live.echo.phase = siple_core::media::EchoPhase::Done;
    h.state_mut().live.echo.silent = true;
    h.run_steps(4);
    assert!(h.query_by_label_contains("Nothing was recorded").is_some());
}

#[test]
fn calls_page_edits_apply_instantly() {
    let mut h = settings_harness(base_snapshot(), Section::Calls, WIDE);
    click_switch(&mut h, "Resume the held call after a failed transfer");
    assert_eq!(
        rpc::edits_of(&only_edit(take(&mut h))),
        [ConfigEdit::SetSection {
            section: "calls".into(),
            key: "resume_after_failed_transfer".into(),
            value: EditValue::Bool(false)
        }]
    );
}

#[test]
fn advanced_lists_file_only_keys_and_developer_shows_the_mcp_snippet() {
    let mut h = settings_harness(base_snapshot(), Section::Advanced, WIDE);
    assert!(h.query_by_label_contains("extra_headers").is_some());
    h.get_by_label("Open config").click_accesskit();
    h.run();
    assert_eq!(take(&mut h), [UiAction::OpenConfig]);
    let h = settings_harness(base_snapshot(), Section::Developer, WIDE);
    assert!(h.query_by_label_contains("siple-cli").is_some());
}

// ---- wizard -----------------------------------------------------------------------------------

fn wizard_snapshot() -> Snapshot {
    let mut s = base_snapshot();
    s.accounts.clear();
    s.blf.clear();
    let text = siple_core::config::EXAMPLE_CONFIG;
    s.cfg = std::sync::Arc::new(siple_core::config::Config::parse(text).unwrap());
    s
}

fn wizard_to_audio(h: &mut Harness<'_, State>) {
    type_into(h, "Username / extension", "1001");
    type_into(h, "Password", "pw");
    type_into(h, "Domain / server", "pbx.example.com");
    h.get_by_label("Test & continue").click_accesskit();
    h.run();
    let req = only_edit(take(h));
    assert_eq!(req.method, "account.test");
    deliver(
        h,
        RpcTag::TestDraft,
        Ok(json!({"diagnosis": "ok", "ok": true, "message": "Registered via UDP in 0.3 s"})),
    );
    assert!(h.query_by_label("Registered via UDP in 0.3 s").is_some());
}

#[test]
fn wizard_shows_without_a_real_account_and_replaces_the_example() {
    let mut h = harness_ui(wizard_snapshot(), WIDE, UiState::default());
    h.run();
    assert!(h.query_by_label("Welcome to Siple").is_some());
    assert!(h.query_by_label("Step 1 of 3").is_some());
    // Settings and the shell are not reachable meanwhile.
    assert!(h.query_by_label("Settings").is_none());
    wizard_to_audio(&mut h);
    // A successful test writes the account, replacing the untouched example.
    let req = only_edit(take(&mut h));
    assert_eq!(req.method, "config.edit");
    let edits = rpc::edits_of(&req);
    let [
        ConfigEdit::AddAccount {
            draft,
            replace_example: true,
        },
    ] = &edits[..]
    else {
        panic!("{edits:?}")
    };
    assert_eq!(
        draft.fields.get(&AccountField::Password),
        Some(&EditValue::Str("pw".into()))
    );
    assert_eq!(draft.fields.get(&AccountField::PasswordStore), None);
    deliver(&mut h, RpcTag::WizardAdd, Ok(json!({"applied": true})));
    assert!(h.query_by_label("Step 2 of 3").is_some());
    // The account now exists in the config, yet the wizard stays until it is finished.
    h.state_mut().snap = base_snapshot();
    h.run();
    assert!(h.query_by_label("Step 2 of 3").is_some());
    h.get_by_label("Continue").click_accesskit();
    h.run();
    assert!(h.query_by_label("Step 3 of 3").is_some());
    click_switch(&mut h, "Show in the tray / menu bar");
    assert_eq!(
        rpc::edits_of(&only_edit(take(&mut h))),
        [ConfigEdit::SetSection {
            section: "ui".into(),
            key: "tray".into(),
            value: EditValue::Bool(false)
        }]
    );
    h.get_by_label("Finish").click_accesskit();
    h.run();
    assert_eq!(take(&mut h), [UiAction::FirstRunDone]);
    assert!(rail_shown(&h), "the main window is back");
}

#[test]
fn wizard_keychain_option_keeps_the_password_out_of_the_edit() {
    let mut h = harness_ui(wizard_snapshot(), WIDE, UiState::default());
    h.run();
    click_switch(&mut h, "Store in system keychain");
    wizard_to_audio(&mut h);
    let edits = rpc::edits_of(&only_edit(take(&mut h)));
    let [ConfigEdit::AddAccount { draft, .. }] = &edits[..] else {
        panic!("{edits:?}")
    };
    assert_eq!(draft.fields.get(&AccountField::Password), None);
    assert_eq!(
        draft.fields.get(&AccountField::PasswordStore),
        Some(&EditValue::Str("keyring".into()))
    );
    deliver(&mut h, RpcTag::WizardAdd, Ok(json!({"applied": true})));
    let acts = take(&mut h);
    let Some(UiAction::Rpc(r)) = acts.last() else {
        panic!("{acts:?}")
    };
    assert_eq!(r.method, "account.set_password");
    assert_eq!(r.params["password"], "pw");
}

#[test]
fn wizard_failed_test_explains_and_trust_retests_with_the_fingerprint() {
    let mut h = harness_ui(wizard_snapshot(), WIDE, UiState::default());
    h.run();
    type_into(&mut h, "Username / extension", "1001");
    type_into(&mut h, "Domain / server", "pbx.example.com");
    h.get_by_label("Test & continue").click_accesskit();
    h.run();
    take(&mut h);
    deliver(
        &mut h,
        RpcTag::TestDraft,
        Ok(
            json!({"diagnosis": "auth_failed", "ok": false, "message": "Wrong username or password (the server answered 403)"}),
        ),
    );
    assert!(
        h.query_by_label("Wrong username or password (the server answered 403)")
            .is_some()
    );
    assert!(take(&mut h).is_empty(), "a failed test writes nothing");
    let fp = "cd".repeat(32);
    deliver(
        &mut h,
        RpcTag::TestDraft,
        Ok(
            json!({"diagnosis": "tls_untrusted", "ok": false, "fingerprint": fp, "message": "not trusted"}),
        ),
    );
    h.get_by_label("Trust this server").click_accesskit();
    h.run();
    let req = only_edit(take(&mut h));
    assert_eq!(req.method, "account.test");
    assert_eq!(req.params["draft"]["fields"]["tls_fingerprint"], json!(fp));
}

#[test]
fn wizard_skip_records_the_flag_and_is_not_shown_again() {
    let mut h = harness_ui(wizard_snapshot(), WIDE, UiState::default());
    h.run();
    h.get_by_label_contains("Skip").click_accesskit();
    h.run();
    assert_eq!(take(&mut h), [UiAction::FirstRunDone]);
    assert!(h.query_by_label("Welcome to Siple").is_none());
    assert!(rail_shown(&h));
    // Settings can run it again.
    h.state_mut().ui.settings.open = true;
    h.run();
    h.get_by_label("Run setup again").click_accesskit();
    h.run();
    assert!(h.query_by_label("Welcome to Siple").is_some());
}

#[test]
fn wizard_is_not_shown_with_a_real_account_or_when_dismissed() {
    assert!(!crate::ui::wizard_needed_for_tests(&base_snapshot()));
    assert!(crate::ui::wizard_needed_for_tests(&wizard_snapshot()));
    let mut ui = UiState::default();
    ui.wizard.dismissed = true;
    let mut h = harness_ui(wizard_snapshot(), WIDE, ui);
    h.run();
    assert!(h.query_by_label("Welcome to Siple").is_none());
}

// ---- review fixes -----------------------------------------------------------------------------

#[test]
fn wizard_saves_the_draft_that_was_tested_and_locks_the_form_meanwhile() {
    let mut h = harness_ui(wizard_snapshot(), WIDE, UiState::default());
    h.run();
    type_into(&mut h, "Username / extension", "1001");
    type_into(&mut h, "Domain / server", "pbx.example.com");
    h.get_by_label("Test & continue").click_accesskit();
    h.run();
    take(&mut h);
    assert!(
        h.get_by_label("Domain / server")
            .accesskit_node()
            .is_disabled()
    );
    // Even if the form changed behind the test's back, the tested draft is what gets saved.
    h.state_mut().ui.wizard.form.domain = "evil.example.com".into();
    deliver(
        &mut h,
        RpcTag::TestDraft,
        Ok(json!({"diagnosis": "ok", "ok": true, "message": "Registered"})),
    );
    let edits = rpc::edits_of(&only_edit(take(&mut h)));
    let [ConfigEdit::AddAccount { draft, .. }] = &edits[..] else {
        panic!("{edits:?}")
    };
    assert_eq!(
        draft.fields.get(&AccountField::Domain),
        Some(&EditValue::Str("pbx.example.com".into()))
    );
}

#[test]
fn the_wizard_is_decided_once_and_does_not_pop_up_when_the_last_account_is_disabled() {
    let mut h = harness_ui(base_snapshot(), WIDE, UiState::default());
    h.run();
    // A real account exists and no flag was recorded: record it now.
    assert_eq!(take(&mut h), [UiAction::FirstRunDone]);
    let mut cfg = (*h.state().snap.cfg).clone();
    for a in &mut cfg.accounts {
        a.enabled = false;
    }
    h.state_mut().snap.cfg = std::sync::Arc::new(cfg);
    h.run();
    assert!(h.query_by_label("Welcome to Siple").is_none());
    assert!(rail_shown(&h));
}

#[test]
fn an_account_added_while_the_wizard_is_open_does_not_close_it_midway() {
    let mut h = harness_ui(wizard_snapshot(), WIDE, UiState::default());
    h.run();
    h.state_mut().snap = base_snapshot();
    h.run();
    assert!(h.query_by_label("Welcome to Siple").is_some());
}

#[test]
fn a_ringing_call_can_be_answered_from_the_wizard() {
    let mut snap = wizard_snapshot();
    snap.calls = vec![call(
        "c1",
        CallDirection::Inbound,
        CallState::Ringing,
        ("Reception", "sip:1002@pbx.example.test"),
    )];
    let mut h = harness_ui(snap, WIDE, UiState::default());
    h.run();
    assert!(h.query_by_label("Welcome to Siple").is_some());
    h.get_by_label_contains("Answer").click_accesskit();
    h.run();
    assert_eq!(take(&mut h), [UiAction::Answer("c1".into())]);
}

#[test]
fn an_own_instant_edit_does_not_make_staged_edits_stale() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    type_into(&mut h, "Display name", "!");
    // An instant edit succeeds; the file now has a new hash.
    deliver(
        &mut h,
        RpcTag::Instant,
        Ok(json!({"applied": true, "hash": "hash-1"})),
    );
    h.state_mut().snap.config_hash = "hash-1".into();
    h.run();
    assert!(h.query_by_label("The config file changed.").is_none());
    h.get_by_label("Apply").click_accesskit();
    h.run();
    assert_eq!(only_edit(take(&mut h)).params["base_hash"], "hash-1");
}

#[test]
fn removing_an_account_with_staged_edits_forgets_their_base_hash() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    type_into(&mut h, "Display name", "!");
    h.get_by_label("Remove account").click_accesskit();
    h.run();
    h.get_by_label("Remove account").click_accesskit();
    h.run();
    assert!(!h.state().ui.settings.dirty());
    assert_eq!(h.state().ui.settings.base_hash, None);
}

#[test]
fn keep_mine_after_a_conflict_uses_the_conflicts_hash() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    type_into(&mut h, "Display name", "!");
    h.get_by_label("Apply").click_accesskit();
    h.run();
    take(&mut h);
    deliver(
        &mut h,
        RpcTag::Apply,
        Err(RpcError::new(
            "invalid_state",
            "changed",
            json!({"reason": "conflict", "current_hash": "hash-9"}),
        )),
    );
    // The snapshot has not caught up yet: the banner offers Keep mine, which must use hash-9.
    h.get_by_label("Keep mine").click_accesskit();
    h.run();
    assert_eq!(h.state().ui.settings.base_hash.as_deref(), Some("hash-9"));
}

#[test]
fn add_errors_the_form_does_not_show_go_to_the_notice() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    h.get_by_label("New account").click_accesskit();
    h.run();
    type_into(&mut h, "Username / extension", "1001");
    type_into(&mut h, "Domain / server", "pbx.example.test");
    h.get_by_label("Add account").click_accesskit();
    h.run();
    take(&mut h);
    deliver(
        &mut h,
        RpcTag::AddAccount,
        Err(RpcError::new(
            "invalid_params",
            "invalid",
            json!({"reason": "invalid", "errors": [{"path": "account.id", "message": "id already in use"}]}),
        )),
    );
    assert!(h.query_all_by_label_contains("id already in use").count() >= 1);
}

#[test]
fn a_successful_instant_edit_keeps_the_errors_of_a_failed_apply() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    type_into(&mut h, "Username", "x");
    h.get_by_label("Apply").click_accesskit();
    h.run();
    take(&mut h);
    deliver(
        &mut h,
        RpcTag::Apply,
        Err(RpcError::new(
            "invalid_params",
            "invalid",
            json!({"reason": "invalid", "errors": [{"path": "account[0].username", "message": "must not contain spaces"}]}),
        )),
    );
    deliver(
        &mut h,
        RpcTag::Instant,
        Ok(json!({"applied": true, "hash": "hash-1"})),
    );
    assert!(h.query_by_label("must not contain spaces").is_some());
}

#[test]
fn echo_errors_in_the_wizard_show_in_the_wizard() {
    let mut h = harness_ui(wizard_snapshot(), WIDE, UiState::default());
    h.state_mut().ui.wizard.restart();
    h.state_mut().ui.wizard.step = crate::ui::wizard::Step::Audio;
    h.run_steps(3);
    deliver(
        &mut h,
        RpcTag::Echo,
        Err(RpcError::new(
            "invalid_state",
            "a call is in progress",
            json!(null),
        )),
    );
    assert!(h.state().ui.settings.notice.is_none());
    assert!(h.query_by_label("a call is in progress").is_some());
}

#[test]
fn a_decline_code_drag_writes_once_when_it_ends() {
    let mut h = settings_harness(base_snapshot(), Section::Calls, WIDE);
    let n = h.get_by_role_and_label(egui::accesskit::Role::SpinButton, "Decline code");
    n.focus();
    h.run();
    h.get_by_role_and_label(egui::accesskit::Role::SpinButton, "Decline code")
        .type_text("5");
    h.run();
    assert!(take(&mut h).is_empty(), "typing does not write the file");
    h.get_by_label("Hold scope").focus();
    h.run();
    let edits: Vec<_> = take(&mut h)
        .into_iter()
        .filter(|a| matches!(a, UiAction::Rpc(_)))
        .collect();
    assert_eq!(edits.len(), 1, "{edits:?}");
}

#[test]
fn unreadable_file_errors_arrive_as_plain_strings_and_are_shown() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    type_into(&mut h, "Display name", "!");
    h.get_by_label("Apply").click_accesskit();
    h.run();
    take(&mut h);
    deliver(
        &mut h,
        RpcTag::Apply,
        Err(RpcError::new(
            "invalid_state",
            "bad file",
            json!({"reason": "file_invalid", "errors": ["expected `=` at line 3"]}),
        )),
    );
    assert!(
        h.query_all_by_label_contains("expected `=` at line 3")
            .count()
            >= 1
    );
}

#[test]
fn secrets_never_appear_in_debug_output() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    type_into(&mut h, "Password", "hunter2-secret");
    assert!(
        !h.state().ui.settings.passwords.is_empty(),
        "the password was typed"
    );
    let s = format!("{:?}", h.state().ui);
    assert!(!s.contains("hunter2-secret"), "{s}");
    let req = RpcRequest {
        tag: RpcTag::SetPassword("a".into()),
        method: "account.set_password",
        params: json!({"account": "a", "password": "hunter2-secret"}),
    };
    assert!(!format!("{req:?}").contains("hunter2-secret"));
    let form = crate::ui::draft::DraftForm {
        password: "hunter2-secret".into(),
        ..Default::default()
    };
    assert!(!format!("{form:?}").contains("hunter2-secret"));
}

#[test]
fn passwords_are_dropped_when_the_wizard_ends_or_the_add_form_is_left() {
    let mut h = harness_ui(wizard_snapshot(), WIDE, UiState::default());
    h.run();
    type_into(&mut h, "Password", "pw-in-wizard");
    h.get_by_label_contains("Skip").click_accesskit();
    h.run();
    assert!(h.state().ui.wizard.form.password.is_empty());
    // Ending a link flow also drops the password and the link.
    let mut h = harness(base_snapshot(), WIDE);
    h.run();
    offer(
        &mut h,
        link_for("2001", "example.test"),
        Origin::Opened,
        now() - secs(2),
    );
    click_first(&mut h, "Continue");
    type_into(&mut h, "Password", "pw-from-link-flow");
    h.get_by_label("Cancel").click_accesskit();
    h.run();
    let w = &h.state().ui.wizard;
    assert!(w.form.password.is_empty() && w.link.is_none() && !w.link_only);
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    h.get_by_label("New account").click_accesskit();
    h.run();
    type_into_first_password(&mut h, "pw-in-add");
    h.get_by_label_contains("Back").click_accesskit();
    h.run();
    assert!(h.state().ui.settings.add.is_none());
}

#[test]
fn shortcuts_page_shows_effective_chords() {
    use crate::ui::settings::pages::chord_text;
    use siple_core::config::shortcuts::{Chord, Shortcut};
    let mac = cfg!(target_os = "macos");
    let mut snap = base_snapshot();
    snap.ui.shortcuts.insert("mute".into(), "Alt+M".into());
    let h = settings_harness(snap, Section::Shortcuts, WIDE);
    assert!(h.query_by_label_contains("Planned").is_none());
    assert!(h.query_by_label_contains("not active").is_none());
    // Every action is listed with its default chord, the overridden one with the override.
    for s in Shortcut::all() {
        let chord = if s == Shortcut::Mute {
            "Alt+M  (custom)".to_string()
        } else {
            chord_text(&Chord::parse(s.default_chord()).unwrap(), mac)
        };
        let has = |l: &str| h.query_all_by_label(l).next().is_some();
        assert!(has(&s.label()), "{}", s.name());
        assert!(has(&chord), "{}: {chord}", s.name());
    }
    assert!(
        h.query_by_label(if mac { "Cmd+Shift+A" } else { "Ctrl+Shift+A" })
            .is_some()
    );
}

// ---- keyboard shortcuts and typed digits ------------------------------------------------------

const MOD: egui::Modifiers = egui::Modifiers::COMMAND;
const MOD_SHIFT: egui::Modifiers = egui::Modifiers {
    shift: true,
    ..egui::Modifiers::COMMAND
};

fn press(h: &mut Harness<'_, State>, mods: egui::Modifiers, key: egui::Key) -> Vec<UiAction> {
    h.key_press_modifiers(mods, key);
    h.run_steps(3);
    take(h)
}

fn type_text(h: &mut Harness<'_, State>, text: &str) -> Vec<UiAction> {
    h.event(egui::Event::Text(text.into()));
    h.run_steps(3);
    take(h)
}

fn ringing_only() -> Snapshot {
    let mut s = base_snapshot();
    s.calls = vec![call(
        "c1",
        CallDirection::Inbound,
        CallState::Ringing,
        ("Reception desk", "sip:1002@pbx.example.test"),
    )];
    s
}

fn selected<'a>(h: &'a Harness<'_, State>) -> Option<&'a str> {
    h.state().ui.selected_line.as_deref()
}

#[test]
fn answer_shortcut_answers_the_selected_or_topmost_ringing_call() {
    use egui::Key::A;
    // A lone ringing call (selected by default): plain answer.
    let mut h = harness(ringing_only(), WIDE);
    h.run();
    assert_eq!(press(&mut h, MOD_SHIFT, A), [UiAction::Answer("c1".into())]);
    // Call waiting over an active line: answer and hold the active line.
    let mut h = harness(call_waiting(), WIDE);
    h.run();
    assert_eq!(selected(&h), Some("c1"));
    assert_eq!(
        press(&mut h, MOD_SHIFT, A),
        [UiAction::AnswerHold("c3".into())]
    );
    // Two waiting calls: the newest (top banner) wins; selecting the older one picks it.
    let mut snap = call_waiting();
    let mut w = call(
        "c4",
        CallDirection::Inbound,
        CallState::Ringing,
        ("Dave", "sip:2042@lab.example.test"),
    );
    w.created_at = now() - chrono::Duration::seconds(2);
    snap.calls.push(w);
    let mut h = harness(snap, WIDE);
    h.run();
    assert_eq!(
        press(&mut h, MOD_SHIFT, A),
        [UiAction::AnswerHold("c4".into())]
    );
    h.state_mut().ui.selected_line = Some("c3".into());
    h.run();
    assert_eq!(
        press(&mut h, MOD_SHIFT, A),
        [UiAction::AnswerHold("c3".into())]
    );
    // Nothing ringing, or no calls at all: no-op.
    for snap in [two_calls(), base_snapshot()] {
        let mut h = harness(snap, WIDE);
        h.run();
        assert!(press(&mut h, MOD_SHIFT, A).is_empty());
    }
}

#[test]
fn hangup_shortcut_follows_the_button_rules() {
    use egui::Key::E;
    // Established line: hang up.
    let mut h = harness(two_calls(), WIDE);
    h.run();
    assert_eq!(press(&mut h, MOD_SHIFT, E), [UiAction::Hangup("c1".into())]);
    // A ringing call selected: decline (reject).
    let mut h = harness(ringing_only(), WIDE);
    h.run();
    assert_eq!(press(&mut h, MOD_SHIFT, E), [UiAction::Reject("c1".into())]);
    // Plain outgoing call still dialing: cancel is a hangup.
    let mut snap = base_snapshot();
    snap.calls = vec![call(
        "c1",
        CallDirection::Outbound,
        CallState::Dialing,
        ("Bob", "sip:1003@pbx.example.test"),
    )];
    let mut h = harness(snap, WIDE);
    h.run();
    assert_eq!(press(&mut h, MOD_SHIFT, E), [UiAction::Hangup("c1".into())]);
    // A dialing consult leg: the session cancel.
    let mut snap = transfer_consult();
    snap.calls[1].state = CallState::Dialing;
    snap.calls[1].answered_at = None;
    snap.transfers = vec![session(TransferPhase::ConsultDialing)];
    let mut h = harness(snap, WIDE);
    h.state_mut().ui.selected_line = Some("c2".into());
    h.run();
    assert_eq!(
        press(&mut h, MOD_SHIFT, E),
        [UiAction::TransferCancel("t1".into())]
    );
    // A connected consult: the consult leg cancels the session, the original hangs up.
    let mut h = consult_with(TransferPhase::ConsultConnected, "c2");
    assert_eq!(
        press(&mut h, MOD_SHIFT, E),
        [UiAction::TransferCancel("t1".into())]
    );
    let mut h = consult_with(TransferPhase::ConsultConnected, "c1");
    assert_eq!(press(&mut h, MOD_SHIFT, E), [UiAction::Hangup("c1".into())]);
    // During call waiting the selected (current) line hangs up; the waiting call keeps ringing.
    let mut h = harness(call_waiting(), WIDE);
    h.run();
    assert_eq!(press(&mut h, MOD_SHIFT, E), [UiAction::Hangup("c1".into())]);
    // An ended line, or no calls: no-op.
    let mut snap = two_calls();
    snap.calls[0].state = CallState::Ended;
    snap.calls[0].ended_at = Some(now());
    let mut h = harness(snap, WIDE);
    h.state_mut().ui.selected_line = Some("c1".into());
    h.run();
    assert!(press(&mut h, MOD_SHIFT, E).is_empty());
    let mut h = harness(base_snapshot(), WIDE);
    h.run();
    assert!(press(&mut h, MOD_SHIFT, E).is_empty());
}

#[test]
fn mute_and_hold_shortcuts_dispatch_the_button_actions() {
    use egui::Key::{H, M};
    // two_calls: c1 active + muted.
    let mut h = harness(two_calls(), WIDE);
    h.run();
    assert_eq!(
        press(&mut h, MOD_SHIFT, M),
        [UiAction::Mute("c1".into(), false)]
    );
    assert_eq!(
        press(&mut h, MOD_SHIFT, H),
        [UiAction::Hold("c1".into(), true)]
    );
    // A held line cannot be muted; its hold key resumes through activate.
    h.state_mut().ui.selected_line = Some("c2".into());
    h.run();
    assert!(press(&mut h, MOD_SHIFT, M).is_empty());
    assert_eq!(
        press(&mut h, MOD_SHIFT, H),
        [UiAction::Activate("c2".into())]
    );
    // Connected consult: Resume of the held original is the session swap; the talking leg mutes
    // and holds like any line.
    let mut h = consult_with(TransferPhase::ConsultConnected, "c1");
    assert_eq!(
        press(&mut h, MOD_SHIFT, H),
        [UiAction::TransferSwap("t1".into())]
    );
    let mut h = consult_with(TransferPhase::ConsultConnected, "c2");
    assert_eq!(
        press(&mut h, MOD_SHIFT, M),
        [UiAction::Mute("c2".into(), true)]
    );
    // While a REFER is outstanding, Hold is unavailable (same as the greyed button).
    let mut h = consult_with(TransferPhase::Referring, "c2");
    assert!(press(&mut h, MOD_SHIFT, H).is_empty());
    // No calls: no-ops.
    let mut h = harness(base_snapshot(), WIDE);
    h.run();
    assert!(press(&mut h, MOD_SHIFT, M).is_empty());
    assert!(press(&mut h, MOD_SHIFT, H).is_empty());
}

#[test]
fn transfer_shortcut_opens_the_composer_when_the_button_would() {
    use egui::Key::T;
    let mut h = harness(two_calls(), WIDE);
    h.run();
    assert!(press(&mut h, MOD_SHIFT, T).is_empty());
    assert_eq!(
        h.state().ui.composer.as_ref().map(|c| c.call_id.as_str()),
        Some("c1")
    );
    // In a live session the transfer button is disabled, so is the shortcut.
    let mut h = consult_with(TransferPhase::ConsultConnected, "c2");
    press(&mut h, MOD_SHIFT, T);
    assert!(h.state().ui.composer.is_none());
    // Ringing call or nothing: no composer.
    for snap in [ringing_only(), base_snapshot()] {
        let mut h = harness(snap, WIDE);
        h.run();
        press(&mut h, MOD_SHIFT, T);
        assert!(h.state().ui.composer.is_none());
    }
}

#[test]
fn mod_digit_selects_a_line_and_swaps_a_held_one() {
    use egui::Key::{Num1, Num2, Num3, Num9};
    let mut h = harness(two_calls(), WIDE);
    h.run();
    // L2 is held: selecting it makes it the active line.
    assert_eq!(press(&mut h, MOD, Num2), [UiAction::Activate("c2".into())]);
    assert_eq!(selected(&h), Some("c2"));
    // L1 is the active line: select only.
    assert!(press(&mut h, MOD, Num1).is_empty());
    assert_eq!(selected(&h), Some("c1"));
    // Empty slots do nothing.
    assert!(press(&mut h, MOD, Num3).is_empty());
    assert!(press(&mut h, MOD, Num9).is_empty());
    assert_eq!(selected(&h), Some("c1"));
    // Connected consult with the other leg talking: the held leg's slot swaps the session.
    let mut h = consult_with(TransferPhase::ConsultConnected, "c2");
    assert_eq!(
        press(&mut h, MOD, Num1),
        [UiAction::TransferSwap("t1".into())]
    );
    assert_eq!(selected(&h), Some("c1"));
    // Any other live phase only selects.
    let mut h = consult_with(TransferPhase::ConsultRinging, "c2");
    assert!(press(&mut h, MOD, Num1).is_empty());
    assert_eq!(selected(&h), Some("c1"));
    // During call waiting a slot selects the ringing call without answering.
    let mut h = harness(call_waiting(), WIDE);
    h.run();
    assert!(press(&mut h, MOD, Num3).is_empty());
    assert_eq!(selected(&h), Some("c3"));
    // No calls: no-op.
    let mut h = harness(base_snapshot(), WIDE);
    h.run();
    assert!(press(&mut h, MOD, Num1).is_empty());
}

#[test]
fn previous_next_line_only_select_and_wrap() {
    use egui::Key::{CloseBracket, OpenBracket};
    let mut h = harness(call_waiting(), WIDE);
    h.run();
    assert_eq!(selected(&h), Some("c1"));
    assert!(press(&mut h, MOD, CloseBracket).is_empty());
    assert_eq!(selected(&h), Some("c2"));
    assert!(press(&mut h, MOD, CloseBracket).is_empty());
    assert_eq!(selected(&h), Some("c3"));
    assert!(press(&mut h, MOD, CloseBracket).is_empty());
    assert_eq!(selected(&h), Some("c1"));
    assert!(press(&mut h, MOD, OpenBracket).is_empty());
    assert_eq!(selected(&h), Some("c3"));
    // No calls: no-op.
    let mut h = harness(base_snapshot(), WIDE);
    h.run();
    assert!(press(&mut h, MOD, CloseBracket).is_empty());
    assert_eq!(selected(&h), None);
}

#[test]
fn focus_dial_opens_the_collapsed_dial_bar() {
    use egui::Key::L;
    let mut h = harness(two_calls(), WIDE);
    h.run();
    assert!(!h.state().ui.new_call_open);
    assert!(h.query_by_label("Number or SIP URI").is_none());
    press(&mut h, MOD, L);
    assert!(h.state().ui.new_call_open);
    assert!(h.ctx.text_edit_focused());
    // Idle: the field is already there and takes focus.
    let mut h = harness(base_snapshot(), WIDE);
    h.run();
    press(&mut h, MOD, L);
    assert!(h.ctx.text_edit_focused());
}

#[test]
fn digits_send_dtmf_only_on_an_active_line_without_text_focus() {
    // Active line selected: DTMF, including * and #.
    let mut h = harness(two_calls(), WIDE);
    h.run();
    assert_eq!(type_text(&mut h, "5"), [UiAction::Dtmf("c1".into(), '5')]);
    assert_eq!(type_text(&mut h, "*"), [UiAction::Dtmf("c1".into(), '*')]);
    assert_eq!(type_text(&mut h, "#"), [UiAction::Dtmf("c1".into(), '#')]);
    assert_eq!(h.state().ui.dial_target, "");
    // Letters are not digits.
    assert!(type_text(&mut h, "x").is_empty());
    // A focused dial field takes its own text: no tones.
    press(&mut h, MOD, egui::Key::L);
    assert!(h.ctx.text_edit_focused());
    assert!(type_text(&mut h, "7").is_empty());
    assert_eq!(h.state().ui.dial_target, "7");
    // Held line selected: digits type into the dial field, opening it.
    let mut h = harness(two_calls(), WIDE);
    h.state_mut().ui.selected_line = Some("c2".into());
    h.run();
    assert!(type_text(&mut h, "5").is_empty());
    assert!(h.state().ui.new_call_open);
    assert_eq!(h.state().ui.dial_target, "5");
    assert!(h.ctx.text_edit_focused());
    assert!(type_text(&mut h, "6").is_empty());
    assert_eq!(h.state().ui.dial_target, "56");
    // Idle: the dial field.
    let mut h = harness(base_snapshot(), WIDE);
    h.run();
    assert!(type_text(&mut h, "#").is_empty());
    assert_eq!(h.state().ui.dial_target, "#");
    // A ringing call selected is not Active either.
    let mut h = harness(ringing_only(), WIDE);
    h.run();
    assert!(type_text(&mut h, "1").is_empty());
    assert_eq!(h.state().ui.dial_target, "1");
    // The extensions tab hides the stage: DTMF does not leak to the call behind it.
    let mut h = harness_ext(two_calls(), NARROW);
    h.run();
    assert!(type_text(&mut h, "4").is_empty());
    assert_eq!(h.state().ui.dial_target, "4");
}

#[test]
fn escape_closes_the_composer_and_never_ends_a_call() {
    use egui::Key::Escape;
    for snap in [two_calls(), call_waiting(), transfer_consult()] {
        let mut h = harness(snap, WIDE);
        h.run();
        assert!(press(&mut h, egui::Modifiers::NONE, Escape).is_empty());
        assert!(press(&mut h, MOD, Escape).is_empty());
    }
    // A live dialing consult survives Esc.
    let mut snap = transfer_consult();
    snap.calls[1].state = CallState::Dialing;
    snap.transfers = vec![session(TransferPhase::ConsultDialing)];
    let mut h = harness(snap, WIDE);
    h.state_mut().ui.selected_line = Some("c2".into());
    h.run();
    assert!(press(&mut h, egui::Modifiers::NONE, Escape).is_empty());
    // The composer closes.
    let mut h = harness(two_calls(), WIDE);
    h.run();
    press(&mut h, MOD_SHIFT, egui::Key::T);
    assert!(h.state().ui.composer.is_some());
    assert!(press(&mut h, egui::Modifiers::NONE, Escape).is_empty());
    assert!(h.state().ui.composer.is_none());
}

#[test]
fn config_shortcuts_override_the_defaults_and_hot_reload() {
    use egui::Key::{F9, H, M};
    let mut snap = two_calls();
    snap.ui.shortcuts.insert("hangup".into(), "F9".into());
    snap.ui.shortcuts.insert("mute".into(), "Mod+M".into());
    let mut h = harness(snap, WIDE);
    h.run();
    // The override replaces the default chord.
    assert_eq!(
        press(&mut h, egui::Modifiers::NONE, F9),
        [UiAction::Hangup("c1".into())]
    );
    assert!(press(&mut h, MOD_SHIFT, egui::Key::E).is_empty());
    assert_eq!(press(&mut h, MOD, M), [UiAction::Mute("c1".into(), false)]);
    // Shift must match exactly: Mod+Shift+M is not Mod+M.
    assert!(press(&mut h, MOD_SHIFT, M).is_empty());
    // Untouched actions keep their defaults.
    assert_eq!(
        press(&mut h, MOD_SHIFT, H),
        [UiAction::Hold("c1".into(), true)]
    );
    // Hot reload: a new snapshot (as the poller publishes after a config reload) takes effect.
    h.state_mut()
        .snap
        .ui
        .shortcuts
        .insert("mute".into(), "Alt+M".into());
    h.state_mut().snap.ui.shortcuts.remove("hangup");
    h.run();
    assert!(press(&mut h, MOD, M).is_empty());
    assert_eq!(
        press(&mut h, egui::Modifiers::ALT, M),
        [UiAction::Mute("c1".into(), false)]
    );
    assert!(press(&mut h, egui::Modifiers::NONE, F9).is_empty());
    assert_eq!(
        press(&mut h, MOD_SHIFT, egui::Key::E),
        [UiAction::Hangup("c1".into())]
    );
    // An override that does not parse falls back to the default.
    h.state_mut()
        .snap
        .ui
        .shortcuts
        .insert("mute".into(), "Mod+".into());
    h.run();
    assert_eq!(
        press(&mut h, MOD_SHIFT, M),
        [UiAction::Mute("c1".into(), false)]
    );
}

#[test]
fn mod_is_command_on_macos_and_ctrl_elsewhere() {
    use egui::Key::M;
    let mac = egui::Modifiers {
        shift: true,
        ..(egui::Modifiers::MAC_CMD | egui::Modifiers::COMMAND)
    };
    let other = egui::Modifiers {
        shift: true,
        ..(egui::Modifiers::CTRL | egui::Modifiers::COMMAND)
    };
    for mods in [mac, other] {
        let mut h = harness(two_calls(), WIDE);
        h.run();
        assert_eq!(press(&mut h, mods, M), [UiAction::Mute("c1".into(), false)]);
    }
    // Ctrl without the platform's command meaning (Ctrl on macOS) is not Mod.
    let mut h = harness(two_calls(), WIDE);
    h.run();
    let ctrl_only = egui::Modifiers {
        shift: true,
        ..egui::Modifiers::CTRL
    };
    assert!(press(&mut h, ctrl_only, M).is_empty());
}

fn with_filter() -> Snapshot {
    let mut s = base_snapshot();
    s.blf = blf_many();
    s
}

#[test]
fn slash_focuses_the_extension_filter_unless_typing() {
    let mut h = harness(with_filter(), WIDE);
    h.run();
    assert!(!h.ctx.text_edit_focused());
    press(&mut h, egui::Modifiers::NONE, egui::Key::Slash);
    assert!(h.ctx.text_edit_focused());
    // The configured chord applies; the old one no longer does.
    let mut snap = with_filter();
    snap.ui
        .shortcuts
        .insert("filter_extensions".into(), "Mod+K".into());
    let mut h = harness(snap, WIDE);
    h.run();
    press(&mut h, egui::Modifiers::NONE, egui::Key::Slash);
    assert!(!h.ctx.text_edit_focused());
    press(&mut h, MOD, egui::Key::K);
    assert!(h.ctx.text_edit_focused());
}

#[test]
fn settings_shortcut_follows_the_config() {
    let mut snap = base_snapshot();
    snap.ui.shortcuts.insert("settings".into(), "Mod+P".into());
    let mut h = harness(snap, WIDE);
    h.run();
    press(&mut h, MOD, egui::Key::Comma);
    assert!(!h.state().ui.settings.open);
    press(&mut h, MOD, egui::Key::P);
    assert!(h.state().ui.settings.open);
    // And Mod+, is the default.
    let mut h = harness(base_snapshot(), WIDE);
    h.run();
    press(&mut h, MOD, egui::Key::Comma);
    assert!(h.state().ui.settings.open);
}

// ---- keyboard review fixes --------------------------------------------------------------------

fn key_event(
    key: egui::Key,
    physical: Option<egui::Key>,
    modifiers: egui::Modifiers,
    repeat: bool,
) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: physical,
        pressed: true,
        repeat,
        modifiers,
    }
}

#[test]
fn slash_works_where_it_needs_shift() {
    // DE / FR / ES / IT: `/` is Shift+7, so the event carries Shift.
    let mut h = harness(with_filter(), WIDE);
    h.run();
    let shift = egui::Modifiers::SHIFT;
    press(&mut h, shift, egui::Key::Slash);
    assert!(h.ctx.text_edit_focused());
    // Letters and digits still need the exact Shift state: Mod+Shift+M is not Mod+M.
    let mut snap = two_calls();
    snap.ui.shortcuts.insert("mute".into(), "Mod+M".into());
    let mut h = harness(snap, WIDE);
    h.run();
    assert!(press(&mut h, MOD_SHIFT, egui::Key::M).is_empty());
}

#[test]
fn default_chords_fire_on_layouts_that_move_the_key() {
    // `Mod+]` arrives as Key::Plus on DE / ES / IT; the physical key is the bracket. The press is
    // consumed, so egui's own zoom shortcut does not also see it.
    let mut h = harness(two_calls(), WIDE);
    h.run();
    h.event(key_event(
        egui::Key::Plus,
        Some(egui::Key::CloseBracket),
        MOD,
        false,
    ));
    h.run_steps(3);
    assert_eq!(selected(&h), Some("c2"));
    assert!((h.ctx.zoom_factor() - 1.0).abs() < f32::EPSILON);
    // AZERTY: Cmd+2 reports the logical key of the symbol; the physical key is the digit.
    let mut h = harness(two_calls(), WIDE);
    h.run();
    h.event(key_event(
        egui::Key::Quote,
        Some(egui::Key::Num2),
        MOD,
        false,
    ));
    h.run_steps(3);
    assert_eq!(take(&mut h), [UiAction::Activate("c2".into())]);
    // A physical match never applies to letter chords.
    h.event(key_event(
        egui::Key::Comma,
        Some(egui::Key::M),
        MOD_SHIFT,
        false,
    ));
    h.run_steps(3);
    assert!(take(&mut h).is_empty());
}

#[test]
fn shortcuts_page_warns_about_conflicts() {
    let mut snap = base_snapshot();
    snap.ui
        .shortcuts
        .insert("mute".into(), "Mod+Shift+E".into());
    let h = settings_harness(snap, Section::Shortcuts, WIDE);
    assert!(
        h.query_by_label_contains("Conflict: hangup and mute")
            .is_some()
    );
    let h = settings_harness(base_snapshot(), Section::Shortcuts, WIDE);
    assert!(h.query_by_label_contains("Conflict").is_none());
    if !cfg!(target_os = "macos") {
        let mut snap = base_snapshot();
        snap.ui.shortcuts.insert("mute".into(), "Cmd+M".into());
        let h = settings_harness(snap, Section::Shortcuts, WIDE);
        assert!(h.query_by_label_contains("never fires").is_some());
    }
}

#[test]
fn transfer_shortcut_moves_focus_to_the_composer() {
    let mut h = harness(two_calls(), WIDE);
    h.run();
    press(&mut h, MOD, egui::Key::L);
    assert!(h.ctx.text_edit_focused());
    press(&mut h, MOD_SHIFT, egui::Key::T);
    assert!(type_text(&mut h, "123").is_empty());
    assert_eq!(h.state().ui.dial_target, "");
    assert_eq!(
        h.state().ui.composer.as_ref().map(|c| c.target.as_str()),
        Some("123")
    );
    // Enter transfers (consult by default); it does not dial.
    h.key_press(egui::Key::Enter);
    h.run_steps(3);
    assert_eq!(
        take(&mut h),
        [UiAction::ConsultStart("c1".into(), "123".into())]
    );
}

#[test]
fn option_3_hash_is_typed_and_repeats_are_not() {
    let mut h = harness(two_calls(), WIDE);
    h.run();
    // A platform delivers a key press and its text in the same frame.
    let same_frame = |h: &mut Harness<'_, State>, key: egui::Event, text: &str| {
        if let egui::Event::Key { modifiers, .. } = &key {
            h.input_mut().modifiers = *modifiers;
        }
        h.input_mut().events.push(key);
        h.input_mut().events.push(egui::Event::Text(text.into()));
        h.run_steps(2);
        h.input_mut().modifiers = egui::Modifiers::NONE;
    };
    // macOS UK / Swiss: Option+3 is `#`.
    same_frame(
        &mut h,
        key_event(
            egui::Key::Num3,
            Some(egui::Key::Num3),
            egui::Modifiers::ALT,
            false,
        ),
        "#",
    );
    assert_eq!(take(&mut h), [UiAction::Dtmf("c1".into(), '#')]);
    // Holding a digit sends the tone once: egui marks the later presses as repeats.
    same_frame(
        &mut h,
        key_event(egui::Key::Num5, None, egui::Modifiers::NONE, false),
        "5",
    );
    assert_eq!(take(&mut h), [UiAction::Dtmf("c1".into(), '5')]);
    for _ in 0..3 {
        same_frame(
            &mut h,
            key_event(egui::Key::Num5, None, egui::Modifiers::NONE, true),
            "5",
        );
    }
    let got = take(&mut h);
    assert!(got.is_empty(), "{got:?}");
}

#[test]
fn unbindable_overrides_fall_back_to_the_defaults() {
    let mut snap = two_calls();
    snap.ui.shortcuts.insert("hangup".into(), "Escape".into());
    snap.ui.shortcuts.insert("line_2".into(), "2".into());
    let mut h = harness(snap, WIDE);
    h.run();
    // Esc does nothing; the default hangup chord still works.
    assert!(press(&mut h, egui::Modifiers::NONE, egui::Key::Escape).is_empty());
    assert_eq!(
        press(&mut h, MOD_SHIFT, egui::Key::E),
        [UiAction::Hangup("c1".into())]
    );
    // A bare digit is DTMF only: it does not also select / activate line 2.
    assert_eq!(type_text(&mut h, "2"), [UiAction::Dtmf("c1".into(), '2')]);
    assert_eq!(
        press(&mut h, MOD, egui::Key::Num2),
        [UiAction::Activate("c2".into())]
    );
}

#[test]
fn line_shortcuts_close_the_extensions_tab_when_narrow() {
    let mut h = harness_ext(two_calls(), NARROW);
    h.run();
    assert!(h.state().ui.ext_tab);
    press(&mut h, MOD, egui::Key::Num2);
    assert!(!h.state().ui.ext_tab);
    let mut h = harness_ext(two_calls(), NARROW);
    h.run();
    press(&mut h, MOD, egui::Key::CloseBracket);
    assert!(!h.state().ui.ext_tab);
}

// ---- UX feedback round 1 ----------------------------------------------------------------------

#[test]
fn account_move_buttons_issue_move_account_and_disable_at_the_ends() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    let ids: Vec<String> = h
        .state()
        .snap
        .cfg
        .accounts
        .iter()
        .map(|a| a.id.clone())
        .collect();
    assert!(ids.len() >= 2, "{ids:?}");
    let (first, second) = (&ids[0], &ids[1]);
    assert!(
        is_disabled(&h, &format!("Move {first} up")),
        "first row cannot move up"
    );
    let last = ids.last().unwrap();
    assert!(
        is_disabled(&h, &format!("Move {last} down")),
        "last row cannot move down"
    );
    take(&mut h);
    h.get_by_label(&format!("Move {first} down"))
        .click_accesskit();
    h.run();
    let req = only_edit(take(&mut h));
    assert_eq!(req.method, "config.edit");
    assert_eq!(
        rpc::edits_of(&req),
        [ConfigEdit::MoveAccount {
            id: first.clone(),
            index: 1
        }]
    );
    h.get_by_label(&format!("Move {second} up"))
        .click_accesskit();
    h.run();
    let req = only_edit(take(&mut h));
    assert_eq!(
        rpc::edits_of(&req),
        [ConfigEdit::MoveAccount {
            id: second.clone(),
            index: 0
        }]
    );
}

#[test]
fn dial_row_widgets_share_one_height() {
    for (name, mut h) in [
        ("idle", harness(base_snapshot(), WIDE)),
        ("dial bar", harness(two_calls(), WIDE)),
    ] {
        if name == "dial bar" {
            h.run();
            click_first(&mut h, "New call");
        }
        h.run();
        let field = h.get_by_label("Number or SIP URI");
        let combo = h
            .query_by_role(egui::accesskit::Role::ComboBox)
            .expect("account picker");
        // The Call button is disabled while the field is empty; the rect is the same.
        let call = h.get_by_label("Call");
        let (f, c, b) = (field.rect(), combo.rect(), call.rect());
        let near = |a: f32, b: f32| (a - b).abs() < 0.6;
        assert!(
            near(f.height(), c.height()),
            "{name}: field {f:?} combo {c:?}"
        );
        assert!(
            near(f.height(), b.height()),
            "{name}: field {f:?} call {b:?}"
        );
        assert!(
            near(f.center().y, c.center().y) && near(f.center().y, b.center().y),
            "{name}: not aligned"
        );
    }
}

#[test]
fn extensions_panel_is_open_by_default_on_wide_and_roomy_medium_with_a_chevron() {
    for size in [WIDE, MEDIUM_ROOMY] {
        let mut h = harness(base_snapshot(), size);
        h.run();
        assert!(h.query_by_label("EXTENSIONS").is_some(), "{size:?}");
        h.get_by_label_contains("Extensions").click_accesskit();
        h.run();
        assert!(h.query_by_label("EXTENSIONS").is_none(), "{size:?}");
        assert_eq!(h.state().ui.ext_open, Some(false));
    }
    // Narrow: closed by default, text tab.
    let mut h = harness(base_snapshot(), NARROW);
    h.run();
    assert!(h.query_by_label("EXTENSIONS").is_none());
}

#[test]
fn narrow_actions_do_not_pin_the_panel_closed_for_a_wider_window() {
    let mut h = harness(two_calls(), NARROW);
    h.state_mut().ui.ext_tab = true;
    h.run();
    press(&mut h, MOD, egui::Key::Num2);
    assert!(!h.state().ui.ext_tab);
    assert_eq!(h.state().ui.ext_open, None);
    h.set_size(WIDE);
    h.run();
    assert!(h.query_by_label("EXTENSIONS").is_some());
}

#[test]
fn the_narrow_tab_and_the_wide_panel_are_separate_states() {
    // Tab open in narrow, then widen: the panel follows the saved choice / default, not the tab.
    let mut h = harness(two_calls(), NARROW);
    h.run();
    h.get_by_label_contains("Extensions").click_accesskit();
    h.run();
    assert!(h.state().ui.ext_tab);
    assert_eq!((h.state().ui.ext_open, h.state().ui.ext_pref), (None, None));
    h.set_size(WIDE);
    h.run();
    assert!(
        h.query_by_label("EXTENSIONS").is_some(),
        "wide default is open"
    );
    let mut ui = UiState::default();
    ui.wizard.dismissed = true;
    ui.ext_pref = Some(false);
    let mut h = harness_ui(two_calls(), NARROW, ui);
    h.run();
    h.get_by_label_contains("Extensions").click_accesskit();
    h.run();
    assert!(h.state().ui.ext_tab);
    h.set_size(WIDE);
    h.run();
    assert!(
        h.query_by_label("EXTENSIONS").is_none(),
        "saved choice wins over the tab"
    );
    // Panel open in wide, then narrow: the stage shows, not the tab.
    let mut h = harness(two_calls(), WIDE);
    h.run();
    assert!(h.query_by_label("EXTENSIONS").is_some());
    h.set_size(NARROW);
    h.run();
    assert!(h.query_by_label("EXTENSIONS").is_none());
    assert!(h.query_by_label_contains("Line 1, Bob Martin").is_some());
    assert!(!h.state().ui.ext_tab);
}

#[test]
fn medium_default_uses_the_actual_rail_width_and_latches_on_rail_toggle() {
    // 900 px: 56 + 300 + 440 fits with the collapsed rail.
    let mut h = harness(two_calls(), MEDIUM);
    h.run();
    assert!(h.query_by_label("EXTENSIONS").is_some());
    // Expanding the rail (208 px) would not fit, but the panel does not jump closed.
    h.get_by_label("Expand lines").click_accesskit();
    h.run();
    assert!(h.query_by_label("EXTENSIONS").is_some());
    assert_eq!(h.state().ui.ext_open, Some(true));
    assert_eq!(h.state().ui.ext_pref, None, "a latch is not a saved choice");
    // Below 796 px even the collapsed rail leaves too little room.
    let mut h = harness(two_calls(), egui::vec2(780.0, 760.0));
    h.run();
    assert!(h.query_by_label("EXTENSIONS").is_none());
}

#[test]
fn a_saved_open_choice_does_not_override_the_medium_stage_minimum() {
    let mut ui = UiState::default();
    ui.wizard.dismissed = true;
    ui.ext_pref = Some(true);
    let mut h = harness_ui(two_calls(), egui::vec2(780.0, 760.0), ui);
    h.run();
    assert!(h.query_by_label("EXTENSIONS").is_none());
    // Wide always honours it.
    let mut ui = UiState::default();
    ui.wizard.dismissed = true;
    ui.ext_pref = Some(true);
    let mut h = harness_ui(two_calls(), WIDE, ui);
    h.run();
    assert!(h.query_by_label("EXTENSIONS").is_some());
}

#[test]
fn panel_saves_are_per_setting_and_skip_unchanged_values() {
    let mut st = UiState::default();
    st.restore_panels(siple_core::state::GuiPanels::default());
    assert_eq!(st.take_panel_save(), None);
    // Hiding the extensions saves only that choice.
    st.set_ext_panel(false);
    let got = st.take_panel_save().unwrap();
    assert_eq!(got.extensions_open, Some(false));
    assert_eq!(got.lines_collapsed, None);
    assert_eq!(st.take_panel_save(), None);
    // Expanding the rail saves only the rail.
    st.set_lines_expanded(true);
    let got = st.take_panel_save().unwrap();
    assert_eq!(got.lines_collapsed, Some(false));
    assert_eq!(got.extensions_open, None);
    // Re-asserting the same value writes nothing.
    st.set_lines_expanded(true);
    assert_eq!(st.take_panel_save(), None);
}

#[test]
fn set_panels_requests_apply_and_save_without_a_frame() {
    // The shell drives this from `logic`, which still runs while the window is hidden.
    let shared = crate::snapshot::Shared::new(
        base_snapshot(),
        std::path::PathBuf::from("config.toml"),
        None,
    );
    let mut st = UiState::default();
    st.restore_panels(siple_core::state::GuiPanels::default());
    shared.request_panels(Some(false), Some(false));
    let (lines, ext) = shared.take_panel_requests();
    st.apply_panel_request(lines, ext);
    assert!(st.lines_expanded);
    assert_eq!(st.ext_open, Some(false));
    let got = st.take_panel_save().unwrap();
    assert_eq!(got.lines_collapsed, Some(false));
    assert_eq!(got.extensions_open, Some(false));
    // Narrow stays as it is.
    assert!(!st.ext_tab);
    assert_eq!(shared.take_panel_requests(), (None, None));
}

#[test]
fn composer_actions_fit_inside_a_medium_call_card() {
    let mut h = harness(two_calls(), MEDIUM);
    h.state_mut().ui.composer = Some(crate::ui::Composer {
        call_id: "c1".into(),
        target: "10".into(),
    });
    h.run();
    let field = h.get_by_role(egui::accesskit::Role::TextInput).rect();
    let close = h.get_by_label_contains("Close").rect();
    assert!(
        close.right() <= field.right() + 1.0,
        "Close is clipped: {close:?} vs field {field:?}"
    );
}

// ---- problems: plain-language failures --------------------------------------------------------

#[test]
fn failed_account_menu_names_the_problem_its_hint_and_the_servers_answer() {
    let mut h = harness(base_snapshot(), WIDE);
    h.run();
    // The chip's accessible name carries the title, not "failed: 403 Forbidden".
    assert!(
        h.query_by_label_contains("Account legacy-fs: Wrong username or password")
            .is_some()
    );
    open_account_menu(&mut h, "Account legacy-fs");
    assert!(h.query_by_label("Wrong username or password").is_some());
    assert!(
        h.query_by_label("Check the account's username, auth username and password.")
            .is_some()
    );
    assert!(h.query_by_label_contains("403 Forbidden").is_some());
    assert!(h.query_by_label_contains("next attempt").is_some());
    assert!(h.query_by_label_contains("failed: 403").is_none());
}

#[test]
fn ended_call_shows_the_problem_chip_with_details_and_the_hint() {
    let mut snap = two_calls();
    snap.calls[0].state = CallState::Ended;
    snap.calls[0].ended_at = Some(now());
    snap.calls[0].end = Some(siple_sip::CallEnd {
        code: 486,
        reason: "Busy Here".into(),
        by: siple_sip::EndedBy::Remote,
        missed: false,
        q850_cause: None,
        cause: siple_sip::EndCause::Rejected,
    });
    let mut h = harness(snap, WIDE);
    h.state_mut().ui.selected_line = Some("c1".into());
    h.run();
    assert!(h.query_by_label("Busy").is_some());
    assert!(
        h.query_by_label("The other side is on another call; try again later.")
            .is_some()
    );
    // The raw answer is on hover, no longer the chip text.
    assert!(h.query_by_label("486 Busy Here").is_none());
    h.get_by_label("Busy").hover();
    h.run_steps(60);
    assert!(
        h.query_by_label_contains("Details: 486 Busy Here")
            .is_some()
    );
}

#[test]
fn ended_call_without_a_failure_shows_no_hint() {
    let mut snap = two_calls();
    snap.calls[0].state = CallState::Ended;
    snap.calls[0].ended_at = Some(now());
    snap.calls[0].end = Some(siple_sip::CallEnd {
        code: 200,
        reason: "OK".into(),
        by: siple_sip::EndedBy::Remote,
        missed: false,
        q850_cause: None,
        cause: siple_sip::EndCause::RemoteHangup,
    });
    let mut h = harness(snap, WIDE);
    h.state_mut().ui.selected_line = Some("c1".into());
    h.run();
    assert!(h.query_by_label_contains("try again later").is_none());
    assert!(h.query_by_label_contains("Call back").is_some());
}

#[test]
fn account_test_with_a_problem_shows_title_hint_and_evidence() {
    let mut h = settings_harness(base_snapshot(), Section::Accounts, WIDE);
    deliver(
        &mut h,
        RpcTag::TestAccount("desk-1001".into()),
        Ok(json!({"diagnosis": "auth_failed", "ok": false, "code": 403,
            "problem": siple_sip::problem::Problem::AuthFailed,
            "message": "Wrong username or password (the server answered 403)"})),
    );
    assert!(h.query_by_label("Wrong username or password").is_some());
    assert!(
        h.query_by_label("Check the account's username, auth username and password.")
            .is_some()
    );
    assert!(h.query_by_label("Server answer: 403").is_some());
    assert!(
        h.query_by_label_contains("the server answered 403")
            .is_none()
    );
    // The fingerprint stays visible before it is trusted.
    let fp = "cd".repeat(32);
    deliver(
        &mut h,
        RpcTag::TestAccount("desk-1001".into()),
        Ok(
            json!({"diagnosis": "tls_untrusted", "ok": false, "fingerprint": fp,
            "problem": siple_sip::problem::Problem::TlsUntrusted,
            "message": "The server's TLS certificate is not trusted"}),
        ),
    );
    assert!(
        h.query_by_label("The server's certificate is not trusted")
            .is_some()
    );
    assert!(
        h.query_by_label(&format!("Certificate SHA-256: {fp}"))
            .is_some()
    );
    assert!(h.query_by_label("Trust this certificate").is_some());
    // `other`, or a server without problems: the one-line message as before.
    deliver(
        &mut h,
        RpcTag::TestAccount("desk-1001".into()),
        Ok(json!({"diagnosis": "other", "ok": false,
            "problem": siple_sip::problem::Problem::Other,
            "message": "503 Service Unavailable"})),
    );
    assert!(h.query_by_label("503 Service Unavailable").is_some());
    assert!(h.query_by_label("Something went wrong").is_none());
}

#[test]
fn notice_shows_its_text_and_the_raw_answer_on_hover() {
    let mut h = harness(base_snapshot(), WIDE);
    h.state_mut().notice = Some(crate::snapshot::Notice {
        text: "Dial failed: Busy. The other side is on another call; try again later.".into(),
        detail: Some("486 Busy Here".into()),
    });
    h.run();
    // The footer settles its height after the first frames: hover where the label is now.
    h.run_steps(5);
    h.get_by_label_contains("Dial failed: Busy.").hover();
    h.run_steps(60);
    assert!(
        h.query_by_label_contains("Details: 486 Busy Here")
            .is_some()
    );
    // A plain notice has no details.
    h.state_mut().notice = Some(crate::snapshot::Notice::plain(
        "Dial failed: account lab-tls is not registered",
    ));
    h.run_steps(5);
    h.get_by_label_contains("Dial failed: account").hover();
    h.run_steps(60);
    assert!(h.query_by_label_contains("Details:").is_none());
}
