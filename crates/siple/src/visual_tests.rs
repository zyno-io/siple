//! Snapshot tests: render the main window from fake data (no engine) with wgpu, headless, and
//! compare against `tests/snapshots/*.png`. Regenerate with `UPDATE_SNAPSHOTS=1 cargo test -p
//! siple visual`.

use std::path::PathBuf;

use chrono::{DateTime, Duration, TimeZone, Utc};
use eframe::egui;
use egui_kittest::Harness;
use siple_core::audio::{ActiveDevice, AudioStatus, DeviceInfo, DeviceList, MicPermission};
use siple_core::config::Transport;
use siple_core::state::{DeviceSource, EffectiveDevices, EffectiveSlot};
use siple_sip::{
    AccountStatus, BlfDialog, BlfState, BlfStatus, BlfSubscription, CallDirection, CallInfo,
    CallState, DialogDirection, DialogPhase, HoldFlags, MwiSummary, NegotiatedSummary, RegState,
    SubscriptionPhase, TransferKind, TransferPhase, TransferRef, TransferRole, TransferSession,
};

use crate::snapshot::{ConfigErrorView, Snapshot};
use crate::theme;
use crate::ui::{UiState, ViewModel, draw};

pub(crate) fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 8, 9, 30, 0).unwrap()
}

fn account(id: &str, transport: Transport, aor: &str, registration: RegState) -> AccountStatus {
    AccountStatus {
        id: id.into(),
        enabled: true,
        transport,
        transport_effective: Some(transport),
        registrar: "pbx.example.test".into(),
        aor: aor.into(),
        contact: None,
        registration,
        local_addr: None,
        public_addr: None,
        trace_enabled: false,
        pending_rebuild: false,
        srtp_optional_downgraded: Vec::new(),
        dnd: false,
        mwi: None,
    }
}

fn accounts() -> Vec<AccountStatus> {
    let mut desk = account(
        "desk-1001",
        Transport::Udp,
        "sip:1001@pbx.example.test",
        RegState::Registered {
            registered_at: now() - Duration::seconds(60),
            last_renewed_at: now() - Duration::seconds(60),
            expires_at: now() + Duration::seconds(240),
            granted_secs: 300,
        },
    );
    desk.mwi = Some(MwiSummary {
        waiting: true,
        new: 2,
        old: 5,
    });
    let mut tls = account(
        "lab-tls",
        Transport::Tls,
        "sip:2040@lab.example.test",
        RegState::Registering,
    );
    tls.pending_rebuild = true;
    let mut legacy = account(
        "legacy-fs",
        Transport::Tcp,
        "sip:3300@fs-legacy.example.test",
        RegState::Failed {
            code: 403,
            reason: "Forbidden".into(),
            retry_at: now() + Duration::seconds(30),
        },
    );
    legacy.dnd = true;
    vec![desk, tls, legacy]
}

pub(crate) fn blf(uri: &str, label: &str, state: BlfState) -> BlfStatus {
    let dialogs = if state == BlfState::Ringing {
        vec![BlfDialog {
            id: "d1".into(),
            call_id: None,
            local_tag: None,
            remote_tag: None,
            direction: Some(DialogDirection::Recipient),
            state: DialogPhase::Early,
            remote_identity: Some("sip:1019@pbx.example.test".into()),
            remote_display: Some("Dana".into()),
            local_identity: None,
            local_target: None,
            remote_target: None,
            rendering: None,
        }]
    } else {
        Vec::new()
    };
    BlfStatus {
        account: "desk-1001".into(),
        uri: uri.into(),
        label: label.into(),
        state,
        dialogs,
        updated_at: now(),
        group: String::new(),
        pickup: siple_sip::PickupMode::Off,
        subscription: BlfSubscription {
            phase: if state == BlfState::Failed {
                SubscriptionPhase::Failed
            } else {
                SubscriptionPhase::Active
            },
            last_code: (state == BlfState::Failed).then_some(404),
            last_reason: (state == BlfState::Failed).then(|| "Not Found".to_string()),
            expires_at: None,
            retry_at: None,
        },
        stale: false,
    }
}

fn blf_list() -> Vec<BlfStatus> {
    vec![
        blf("1002", "Reception", BlfState::Idle),
        blf("1003", "Bob Martin", BlfState::Busy),
        blf("1004", "Support queue", BlfState::Ringing),
        blf("1005", "Warehouse", BlfState::Held),
        blf("1006", "", BlfState::Dialing),
        blf("1007", "Conference room", BlfState::Unknown),
        blf("1099", "Old lobby", BlfState::Failed),
        blf("1010", "Sales", BlfState::Idle),
    ]
}

fn dev(name: &str, is_default: bool) -> DeviceInfo {
    DeviceInfo {
        name: name.into(),
        is_default,
        default_sample_rate: 48_000,
        channels: 2,
    }
}

fn active(name: &str) -> ActiveDevice {
    ActiveDevice {
        name: name.into(),
        sample_rate: 48_000,
        channels: 1,
        fallback_to_default: false,
        sample_format: "F32".into(),
        buffer_frames: Some(480),
    }
}

pub(crate) fn base_snapshot() -> Snapshot {
    let slot = |name: &str, source| EffectiveSlot {
        name: name.into(),
        source,
    };
    Snapshot {
        accounts: accounts(),
        blf: blf_list(),
        calls: Vec::new(),
        transfers: Vec::new(),
        active_line: None,
        devices: DeviceList {
            inputs: vec![
                dev("MacBook Pro Microphone", true),
                dev("Jabra Evolve2 65", false),
            ],
            outputs: vec![
                dev("MacBook Pro Speakers", true),
                dev("Jabra Evolve2 65", false),
                dev("Studio Display Speakers", false),
            ],
        },
        effective: EffectiveDevices {
            input: slot("Jabra Evolve2 65", DeviceSource::Config),
            output: slot("Jabra Evolve2 65", DeviceSource::Config),
            ringer: slot("Studio Display Speakers", DeviceSource::Config),
        },
        audio: Some(AudioStatus {
            input: Some(active("Jabra Evolve2 65")),
            output: Some(active("Jabra Evolve2 65")),
            ringer: Some(active("Studio Display Speakers")),
            ..Default::default()
        }),
        mic: MicPermission::Granted,
        config_error: None,
        ui: siple_core::config::UiConfig::default(),
        cfg: std::sync::Arc::new(base_config()),
        config_hash: "hash-0".into(),
        provenance: serde_json::json!({"account": {"lab-tls": {"extra_headers": "file", "tls_server_name": "file"}}}),
        config_unparsable: false,
        logs: Vec::new(),
        update: siple_core::update::UpdateStatus::disabled("stable"),
        update_skipped: None,
    }
}

/// The config behind [`accounts`]: the same three accounts, with extensions on the first.
pub(crate) fn base_config() -> siple_core::config::Config {
    siple_core::config::Config::parse(
        r#"
[[account]]
id = "desk-1001"
display_name = "Desk 1001"
username = "1001"
password = "pw"
domain = "pbx.example.test"
profile = "yealink"

[[account.blf]]
uri = "1002"
label = "Reception"

[[account.blf]]
uri = "1003"
label = "Bob Martin"
group = "Sales"

[[account.blf]]
uri = "1004"
label = "Support queue"

[[account]]
id = "lab-tls"
username = "2040"
password_store = "file"
password = "pw"
domain = "lab.example.test"
transport = "tls"

[[account]]
id = "legacy-fs"
username = "3300"
password = "pw"
domain = "fs-legacy.example.test"
transport = "tcp"
"#,
    )
    .expect("test config")
}

pub(crate) fn call(id: &str, dir: CallDirection, state: CallState, who: (&str, &str)) -> CallInfo {
    CallInfo {
        id: id.into(),
        account: "desk-1001".into(),
        direction: dir,
        state,
        remote_uri: who.1.into(),
        remote_display: Some(who.0.into()).filter(|s: &String| !s.is_empty()),
        local_uri: "sip:1001@pbx.example.test".into(),
        sip_call_id: format!("{id}@siple"),
        created_at: now() - Duration::seconds(14),
        answered_at: None,
        ended_at: None,
        end: None,
        hold: HoldFlags::default(),
        hold_changed_at: None,
        pending: None,
        muted: false,
        negotiated: None,
        early_media: false,
        alert_info: None,
        pai: None,
        rpid: None,
        srtp_fallback: false,
        transferred_from: None,
        replaces: None,
        transfer: None,
        reconnecting: false,
    }
}

pub(crate) fn negotiated(codec: &str, srtp: Option<&str>) -> NegotiatedSummary {
    NegotiatedSummary {
        codec: codec.into(),
        pt: 0,
        clock_rate: 8000,
        ptime_ms: 20,
        srtp_suite: srtp.map(Into::into),
        remote_rtp: "10.0.0.5:16384".into(),
        rfc4733: true,
        rtcp_mux: true,
    }
}

// ---- scenarios -------------------------------------------------------------------------------

fn idle() -> Snapshot {
    base_snapshot()
}

fn incoming() -> Snapshot {
    let mut s = base_snapshot();
    s.calls = vec![call(
        "c1",
        CallDirection::Inbound,
        CallState::Ringing,
        ("Reception desk", "sip:1002@pbx.example.test"),
    )];
    s
}

pub(crate) fn two_calls() -> Snapshot {
    let mut s = base_snapshot();
    let mut a = call(
        "c1",
        CallDirection::Outbound,
        CallState::Active,
        ("Bob Martin", "sip:1003@pbx.example.test"),
    );
    a.created_at = now() - Duration::seconds(290);
    a.answered_at = Some(now() - Duration::seconds(272));
    a.negotiated = Some(negotiated("opus", Some("AES_CM_128_HMAC_SHA1_80")));
    a.muted = true;
    let mut h = call(
        "c2",
        CallDirection::Inbound,
        CallState::Held,
        ("", "sip:+4930123456@pbx.example.test"),
    );
    h.created_at = now() - Duration::seconds(1300);
    h.answered_at = Some(now() - Duration::seconds(1291));
    h.hold.local = true;
    h.negotiated = Some(negotiated("pcmu", None));
    s.calls = vec![a, h];
    s.active_line = Some("c1".into());
    s
}

/// A third call rings while Bob is active and Alice-like caller is held.
pub(crate) fn call_waiting() -> Snapshot {
    let mut s = two_calls();
    let mut w = call(
        "c3",
        CallDirection::Inbound,
        CallState::Ringing,
        ("Carol Diaz", "sip:2041@lab.example.test"),
    );
    w.account = "lab-tls".into();
    w.created_at = now() - Duration::seconds(6);
    s.calls.push(w);
    s
}

pub(crate) fn session(phase: TransferPhase) -> TransferSession {
    TransferSession {
        id: "t1".into(),
        kind: TransferKind::Attended,
        phase,
        account: "desk-1001".into(),
        original: "c1".into(),
        consult: Some("c2".into()),
        target_uri: "sip:1002@pbx.example.test".into(),
        refer_call: None,
        talking_to: Some("c2".into()),
        we_held_original: true,
        last_notify: None,
        last_error: None,
        started_at: now() - Duration::seconds(60),
        ended_at: phase.is_terminal().then(|| now() - Duration::seconds(2)),
    }
}

/// Bob (c1) is on hold and we consult Carol (c2), who answered.
pub(crate) fn transfer_consult() -> Snapshot {
    let mut s = base_snapshot();
    let mut a = call(
        "c1",
        CallDirection::Outbound,
        CallState::Held,
        ("Bob Martin", "sip:1003@pbx.example.test"),
    );
    a.created_at = now() - Duration::seconds(310);
    a.answered_at = Some(now() - Duration::seconds(300));
    a.hold.local = true;
    a.hold_changed_at = Some(now() - Duration::seconds(45));
    a.negotiated = Some(negotiated("opus", Some("AES_CM_128_HMAC_SHA1_80")));
    a.transfer = Some(TransferRef {
        session_id: "t1".into(),
        role: TransferRole::Original,
    });
    let mut b = call(
        "c2",
        CallDirection::Outbound,
        CallState::Active,
        ("Carol Diaz", "sip:1002@pbx.example.test"),
    );
    b.created_at = now() - Duration::seconds(44);
    b.answered_at = Some(now() - Duration::seconds(41));
    b.negotiated = Some(negotiated("opus", Some("AES_CM_128_HMAC_SHA1_80")));
    b.transfer = Some(TransferRef {
        session_id: "t1".into(),
        role: TransferRole::Consult,
    });
    s.calls = vec![a, b];
    s.active_line = Some("c2".into());
    s.transfers = vec![session(TransferPhase::ConsultConnected)];
    s
}

/// 14 extensions in three groups: enough for the filter box.
pub(crate) fn blf_many() -> Vec<BlfStatus> {
    let mut v = blf_list();
    let groups = ["Sales", "Support", "Warehouse"];
    for (i, b) in v.iter_mut().enumerate() {
        b.group = groups[i % 3].into();
        b.dialogs = if b.state == BlfState::Busy {
            vec![BlfDialog {
                id: "d2".into(),
                call_id: None,
                local_tag: None,
                remote_tag: None,
                direction: Some(DialogDirection::Initiator),
                state: DialogPhase::Confirmed,
                remote_identity: Some("sip:2041@lab.example.test".into()),
                remote_display: Some("Carol Diaz".into()),
                local_identity: None,
                local_target: None,
                remote_target: None,
                rendering: None,
            }]
        } else {
            std::mem::take(&mut b.dialogs)
        };
    }
    v[2].pickup = siple_sip::PickupMode::Prefix;
    v[7].stale = true;
    for (i, (uri, label, st)) in [
        ("1011", "Dock", BlfState::Idle),
        ("1012", "Yard", BlfState::Busy),
        ("1013", "Fork-1", BlfState::Idle),
        ("1014", "Fork-2", BlfState::Unknown),
        ("1015", "Gate", BlfState::Idle),
        ("1016", "Reception 2", BlfState::Idle),
    ]
    .into_iter()
    .enumerate()
    {
        let mut b = blf(uri, label, st);
        b.group = groups[i % 3].into();
        v.push(b);
    }
    v
}

fn blf_scenario(view: siple_core::config::BlfView) -> Snapshot {
    let mut s = base_snapshot();
    s.blf = blf_many();
    s.ui.blf_view = view;
    s.ui.blf_group_by = siple_core::config::BlfGroupBy::Group;
    s
}

fn config_error() -> Snapshot {
    let mut s = base_snapshot();
    s.config_error = Some(ConfigErrorView {
        at: "2026-10-08T09:29:41.512+00:00".into(),
        errors: vec![
            "accounts[2].transport: unknown variant `tlss`, expected one of `udp`, `tcp`, `tls`"
                .into(),
            "accounts[0].blf[3]: duplicate uri `1005`".into(),
        ],
    });
    s
}

// ---- harness ---------------------------------------------------------------------------------

struct TestApp {
    snap: Snapshot,
    state: UiState,
    config_path: PathBuf,
    notice: Option<String>,
    live: crate::ui::LiveAudio,
}

impl eframe::App for TestApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let vm = ViewModel {
            snap: &self.snap,
            now: now(),
            config_path: &self.config_path,
            has_log_dir: true,
            notice: self.notice.as_deref(),
            live: self.live.clone(),
        };
        let _ = draw(ui, &vm, &mut self.state);
    }
}

#[derive(Clone, Copy)]
enum Width {
    Narrow,
    Medium,
    Wide,
}

fn harness(snap: Snapshot, theme: egui::Theme, width: Width) -> Harness<'static, TestApp> {
    // Wide and medium render at 1x to keep the baselines small; narrow at 2x.
    let (size, ppp) = match width {
        Width::Narrow => (egui::vec2(420.0, 980.0), 2.0),
        Width::Medium => (egui::vec2(900.0, 760.0), 1.0),
        Width::Wide => (egui::vec2(1240.0, 760.0), 1.0),
    };
    Harness::builder()
        .with_size(size)
        .with_pixels_per_point(ppp)
        // Keep the clock near zero so blinking lamps are captured in their lit phase.
        .with_step_dt(1.0 / 240.0)
        .with_theme(theme)
        .wgpu()
        .build_eframe(move |cc| {
            theme::install(&cc.egui_ctx);
            TestApp {
                snap,
                state: UiState::default(),
                config_path: PathBuf::from(
                    "/Users/engineer/Library/Application Support/Siple/config.toml",
                ),
                notice: None,
                live: Default::default(),
            }
        })
}

fn variants() -> [(egui::Theme, &'static str, Width, &'static str); 6] {
    [
        (egui::Theme::Light, "light", Width::Narrow, "narrow"),
        (egui::Theme::Light, "light", Width::Medium, "medium"),
        (egui::Theme::Light, "light", Width::Wide, "wide"),
        (egui::Theme::Dark, "dark", Width::Narrow, "narrow"),
        (egui::Theme::Dark, "dark", Width::Medium, "medium"),
        (egui::Theme::Dark, "dark", Width::Wide, "wide"),
    ]
}

fn render_all(name: &str, make: fn() -> Snapshot, prepare: impl Fn(&mut Harness<'_, TestApp>)) {
    let mut results = egui_kittest::SnapshotResults::new();
    for (theme, tname, width, wname) in variants() {
        let mut h = harness(make(), theme, width);
        h.run();
        prepare(&mut h);
        h.run();
        h.remove_cursor();
        h.run();
        results.add(h.try_snapshot(format!("{name}_{tname}_{wname}")));
    }
    results.unwrap();
}

#[test]
fn visual_idle() {
    render_all("idle", idle, |_| {});
}

#[test]
fn visual_account_disabled() {
    render_all(
        "account_disabled",
        || {
            let mut snap = base_snapshot();
            snap.accounts[1].enabled = false;
            snap.accounts[1].registration = RegState::Disabled;
            snap.accounts[1].pending_rebuild = false;
            snap
        },
        |_| {},
    );
}

#[test]
fn visual_incoming_call() {
    render_all("incoming", incoming, |_| {});
}

#[test]
fn visual_two_lines() {
    render_all("two_lines", two_calls, |_| {});
}

/// The Lines rail is collapsed by default; this keeps the expanded rail covered.
#[test]
fn visual_two_lines_rail_expanded() {
    render_all("two_lines_rail_expanded", two_calls, |h| {
        h.state_mut().state.lines_expanded = true;
    });
}

#[test]
fn visual_call_waiting() {
    render_all("call_waiting", call_waiting, |_| {});
}

#[test]
fn visual_transfer_consult() {
    render_all("transfer_consult", transfer_consult, |_| {});
}

#[test]
fn visual_transfer_composer() {
    render_all("transfer_composer", two_calls, |h| {
        h.state_mut().state.composer = Some(crate::ui::Composer {
            call_id: "c1".into(),
            target: "10".into(),
        });
    });
}

/// The extensions pane / drawer / tab open in each density.
fn render_blf(name: &str, view: siple_core::config::BlfView) {
    let mut results = egui_kittest::SnapshotResults::new();
    for (theme, tname, width, wname) in variants() {
        let mut h = harness(blf_scenario(view), theme, width);
        h.state_mut().state.ext_open = Some(true);
        h.state_mut().state.ext_tab = true;
        h.run();
        h.remove_cursor();
        h.run();
        results.add(h.try_snapshot(format!("{name}_{tname}_{wname}")));
    }
    results.unwrap();
}

#[test]
fn visual_blf_keys() {
    render_blf("blf_keys", siple_core::config::BlfView::Keys);
}

#[test]
fn visual_blf_lamps() {
    render_blf("blf_lamps", siple_core::config::BlfView::Lamps);
}

#[test]
fn visual_blf_list() {
    render_blf("blf_list", siple_core::config::BlfView::List);
}

#[test]
fn visual_config_error() {
    render_all("config_error", config_error, |h| {
        h.state_mut().notice = Some("Dial failed: account lab-tls is not registered".into());
    });
}

#[test]
fn visual_start_error() {
    let mut results = egui_kittest::SnapshotResults::new();
    for (theme, tname) in [(egui::Theme::Light, "light"), (egui::Theme::Dark, "dark")] {
        let mut h = Harness::builder()
            .with_size(egui::vec2(480.0, 220.0))
            .with_pixels_per_point(2.0)
            .with_theme(theme)
            .wgpu()
            .build_eframe(|cc| {
                theme::install(&cc.egui_ctx);
                crate::ui::ErrorUi {
                    title: "Siple is already running".into(),
                    detail: "Another instance (pid 4211) holds the lock on \
                             /Users/engineer/Library/Application Support/Siple. \
                             Quit it first, or pass --config to use a different directory."
                        .into(),
                }
            });
        h.run();
        results.add(h.try_snapshot(format!("start_error_{tname}")));
    }
    results.unwrap();
}

// ---- settings and wizard -----------------------------------------------------------------------

/// Wide and narrow, light and dark (the settings pages are one column of rows).
fn render_wn(name: &str, make: fn() -> Snapshot, prepare: impl Fn(&mut Harness<'_, TestApp>)) {
    let mut results = egui_kittest::SnapshotResults::new();
    for (theme, tname, width, wname) in variants() {
        if matches!(width, Width::Medium) {
            continue;
        }
        let mut h = harness(make(), theme, width);
        h.run();
        prepare(&mut h);
        h.run_steps(4);
        h.remove_cursor();
        h.run_steps(2);
        results.add(h.try_snapshot(format!("{name}_{tname}_{wname}")));
    }
    results.unwrap();
}

fn open_settings(h: &mut Harness<'_, TestApp>, section: crate::ui::settings::Section) {
    h.state_mut().state.settings.open = true;
    h.state_mut().state.settings.section = section;
}

#[test]
fn visual_settings_accounts() {
    use crate::ui::rpc::{RpcReply, RpcTag};
    render_wn("settings_accounts", two_calls, |h| {
        open_settings(h, crate::ui::settings::Section::Accounts);
        let snap = h.state().snap.clone();
        // A failed test with its action, and one staged change (the Apply bar).
        h.state_mut().state.deliver(
            RpcReply {
                tag: RpcTag::TestAccount("desk-1001".into()),
                result: Ok(serde_json::json!({"diagnosis": "tls_untrusted", "ok": false,
                    "fingerprint": "ab12".repeat(16),
                    "message": "The server's TLS certificate is not trusted (self-signed certificate)"})),
            },
            &snap,
        );
        h.state_mut().state.settings.set_field(
            "desk-1001",
            siple_core::config::edit::AccountField::AutoAnswer,
            &siple_core::config::edit::EditValue::Bool(false),
            siple_core::config::edit::EditValue::Bool(true),
            "hash-0",
        );
    });
}

#[test]
fn visual_settings_audio() {
    render_wn("settings_audio", base_snapshot, |h| {
        open_settings(h, crate::ui::settings::Section::Audio);
        h.state_mut().live.mic_dbfs = Some(-24.0);
        h.state_mut().live.echo.phase = siple_core::media::EchoPhase::Playing;
    });
}

#[test]
fn visual_settings_extensions() {
    render_wn("settings_extensions", base_snapshot, |h| {
        open_settings(h, crate::ui::settings::Section::Extensions);
    });
}

#[test]
fn visual_settings_appearance() {
    render_wn("settings_appearance", base_snapshot, |h| {
        open_settings(h, crate::ui::settings::Section::Appearance);
    });
}

fn wizard_snapshot() -> Snapshot {
    let mut s = base_snapshot();
    s.accounts.clear();
    s.blf.clear();
    s.cfg = std::sync::Arc::new(
        siple_core::config::Config::parse(siple_core::config::EXAMPLE_CONFIG).unwrap(),
    );
    s
}

#[test]
fn visual_wizard_account() {
    use crate::ui::rpc::{RpcReply, RpcTag};
    render_wn("wizard_account", wizard_snapshot, |h| {
        let f = &mut h.state_mut().state.wizard.form;
        f.display_name = "Desk 1001".into();
        f.username = "1001".into();
        f.password = "secret".into();
        f.domain = "pbx.example.com".into();
        let snap = h.state().snap.clone();
        h.state_mut().state.deliver(
            RpcReply {
                tag: RpcTag::TestDraft,
                result: Ok(serde_json::json!({"diagnosis": "auth_failed", "ok": false,
                    "message": "Wrong username or password (the server answered 403)"})),
            },
            &snap,
        );
    });
}

#[test]
fn visual_wizard_audio() {
    render_wn("wizard_audio", wizard_snapshot, |h| {
        h.state_mut().state.wizard.restart();
        h.state_mut().state.wizard.step = crate::ui::wizard::Step::Audio;
        h.state_mut().live.mic_dbfs = Some(-18.0);
    });
}

#[test]
fn visual_wizard_done() {
    render_wn("wizard_done", wizard_snapshot, |h| {
        h.state_mut().state.wizard.restart();
        h.state_mut().state.wizard.step = crate::ui::wizard::Step::Done;
    });
}

// ---- store screenshots -------------------------------------------------------------------------

/// A healthy variant of a scene for the store listings: both accounts registered, no errors,
/// no failed or unknown lamps. Leaves the golden scenes alone.
fn store_clean(mut s: Snapshot) -> Snapshot {
    s.accounts.truncate(2);
    s.accounts[1].registration = RegState::Registered {
        registered_at: now() - Duration::seconds(90),
        last_renewed_at: now() - Duration::seconds(90),
        expires_at: now() + Duration::seconds(210),
        granted_secs: 300,
    };
    s.accounts[1].pending_rebuild = false;
    for b in &mut s.blf {
        match b.label.as_str() {
            "" => b.label = "Dispatch".into(),
            "Old lobby" => b.label = "Lobby".into(),
            _ => {}
        }
        if matches!(b.state, BlfState::Unknown | BlfState::Failed) {
            b.state = BlfState::Idle;
            b.subscription = blf(&b.uri, &b.label, BlfState::Idle).subscription;
        }
        b.stale = false;
    }
    s
}

/// Ten extensions in two groups, so the list ends cleanly in the 1240x760 window.
fn store_blf_list() -> Snapshot {
    let rows = [
        ("Sales", "1002", "Reception", BlfState::Idle),
        ("Sales", "1005", "Warehouse", BlfState::Held),
        ("Sales", "1011", "Dock", BlfState::Idle),
        ("Sales", "1015", "Gate", BlfState::Idle),
        ("Sales", "1016", "Front desk", BlfState::Idle),
        ("Support", "1003", "Bob Martin", BlfState::Busy),
        ("Support", "1004", "Helpdesk", BlfState::Ringing),
        ("Support", "1006", "Dispatch", BlfState::Dialing),
        ("Support", "1012", "Yard", BlfState::Busy),
        ("Support", "1007", "Conference room", BlfState::Idle),
    ];
    let mut s = store_clean(blf_scenario(siple_core::config::BlfView::List));
    s.blf = rows
        .into_iter()
        .map(|(g, uri, label, st)| {
            let mut b = blf(uri, label, st);
            b.group = g.into();
            if st == BlfState::Ringing {
                b.pickup = siple_sip::PickupMode::Prefix;
            }
            b
        })
        .collect();
    s
}

/// Renders the wide scenes at 2x for the store listings (`siple-web` composes them into frames).
/// Not a golden: writes `target/store-shots/<scene>_<theme>.png` (2480x1520). Run with
/// `cargo test -p siple --release store_screenshots -- --ignored`.
#[test]
#[ignore = "writes marketing renders to target/store-shots; run explicitly"]
fn store_screenshots() {
    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/store-shots");
    std::fs::create_dir_all(&out).unwrap();
    type Prepare = fn(&mut Harness<'_, TestApp>);
    type Scene = (&'static str, fn() -> Snapshot, Prepare);
    let scenes: [Scene; 4] = [
        ("two_lines", || store_clean(two_calls()), |_| {}),
        ("incoming", || store_clean(incoming()), |_| {}),
        (
            "transfer_consult",
            || store_clean(transfer_consult()),
            |_| {},
        ),
        ("blf_list", store_blf_list, |h| {
            h.state_mut().state.ext_open = Some(true);
            h.state_mut().state.ext_tab = true;
        }),
    ];
    for (theme, tname) in [(egui::Theme::Light, "light"), (egui::Theme::Dark, "dark")] {
        for (name, make, prepare) in &scenes {
            let mut h = Harness::builder()
                .with_size(egui::vec2(1240.0, 760.0))
                .with_pixels_per_point(2.0)
                .with_step_dt(1.0 / 240.0)
                .with_theme(theme)
                .wgpu()
                .build_eframe(move |cc| {
                    theme::install(&cc.egui_ctx);
                    TestApp {
                        snap: make(),
                        state: UiState::default(),
                        config_path: PathBuf::from(
                            "/Users/engineer/Library/Application Support/Siple/config.toml",
                        ),
                        notice: None,
                        live: Default::default(),
                    }
                });
            h.run();
            prepare(&mut h);
            h.run();
            h.remove_cursor();
            h.run();
            let img = h.render().expect("render");
            assert_eq!((img.width(), img.height()), (2480, 1520));
            img.save(out.join(format!("{name}_{tname}.png"))).unwrap();
        }
    }
}
