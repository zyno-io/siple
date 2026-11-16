//! What the tray icon and its menu show, as plain data built from a [`Snapshot`],
//! and the commands its items send back.
//!
//! Both the native backend (`tray-icon`: macOS, Windows) and the StatusNotifierItem backend
//! (`ksni`: Linux) render a [`TrayModel`] and report [`TrayCmd`]s, so the logic lives here once.

use siple_sip::{CallDirection, CallInfo, CallState, RegState};

use crate::snapshot::{RegClass, Snapshot, reg_class, reg_label, visible_calls};

/// Icon variant, most urgent first (see [`TrayModel::from_snapshot`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayIconState {
    /// A call is ringing.
    Ringing,
    /// A call is established.
    InCall,
    /// An account failed to register.
    Problem,
    /// Do not disturb is on.
    Dnd,
    /// Registered (or nothing configured to register): quiet.
    Idle,
}

/// What a call row offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayCallKind {
    /// Ringing inbound: Answer / Decline.
    Ringing,
    /// Established (or held): Mute / Hold / Hang up.
    Live { muted: bool, held: bool },
    /// Outbound, not yet answered: Hang up.
    Dialing,
}

/// One live call in the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayCall {
    /// Call id.
    pub id: String,
    /// Who (display name or URI).
    pub who: String,
    /// What it offers.
    pub kind: TrayCallKind,
}

/// The tray's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayModel {
    /// Icon variant.
    pub icon: TrayIconState,
    /// Tooltip / title.
    pub tooltip: String,
    /// Live calls, in snapshot order.
    pub calls: Vec<TrayCall>,
    /// `(account id, status text)` for every account.
    pub accounts: Vec<(String, String)>,
    /// Do not disturb on every enabled account (false when there is none).
    pub dnd: bool,
    /// Whether any enabled account exists (DND item is only useful then).
    pub has_accounts: bool,
    /// The window is on screen (menu says Hide instead of Show).
    pub window_visible: bool,
}

fn who(c: &CallInfo) -> String {
    c.remote_display
        .as_deref()
        .filter(|d| !d.is_empty())
        .unwrap_or(&c.remote_uri)
        .to_string()
}

impl TrayModel {
    /// Builds the model. `now` is only used to pick the calls the main window would list.
    pub fn from_snapshot(
        snap: &Snapshot,
        window_visible: bool,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        let calls: Vec<TrayCall> = visible_calls(&snap.calls, now)
            .into_iter()
            .filter(|c| c.state != CallState::Ended)
            .map(|c| TrayCall {
                id: c.id.clone(),
                who: who(c),
                kind: match (c.direction, c.state) {
                    (CallDirection::Inbound, CallState::Ringing | CallState::Early) => {
                        TrayCallKind::Ringing
                    }
                    (_, CallState::Active | CallState::Held | CallState::RemoteHeld) => {
                        TrayCallKind::Live {
                            muted: c.muted,
                            held: c.hold.local,
                        }
                    }
                    _ => TrayCallKind::Dialing,
                },
            })
            .collect();
        let enabled: Vec<_> = snap.accounts.iter().filter(|a| a.enabled).collect();
        let dnd = !enabled.is_empty() && enabled.iter().all(|a| a.dnd);
        let problem = enabled
            .iter()
            .any(|a| matches!(a.registration, RegState::Failed { .. }));
        let icon = if calls.iter().any(|c| c.kind == TrayCallKind::Ringing) {
            TrayIconState::Ringing
        } else if calls
            .iter()
            .any(|c| matches!(c.kind, TrayCallKind::Live { .. }))
        {
            TrayIconState::InCall
        } else if problem {
            TrayIconState::Problem
        } else if dnd {
            TrayIconState::Dnd
        } else {
            TrayIconState::Idle
        };
        let accounts: Vec<(String, String)> = snap
            .accounts
            .iter()
            .map(|a| (a.id.clone(), reg_label(a)))
            .collect();
        let tooltip = tooltip(icon, &calls, &accounts, snap);
        Self {
            icon,
            tooltip,
            calls,
            accounts,
            dnd,
            has_accounts: !enabled.is_empty(),
            window_visible,
        }
    }
}

fn tooltip(
    icon: TrayIconState,
    calls: &[TrayCall],
    accounts: &[(String, String)],
    snap: &Snapshot,
) -> String {
    match icon {
        TrayIconState::Ringing => {
            let c = calls.iter().find(|c| c.kind == TrayCallKind::Ringing);
            format!(
                "Siple: incoming call from {}",
                c.map_or("?", |c| c.who.as_str())
            )
        }
        TrayIconState::InCall => format!("Siple: {} call(s)", calls.len()),
        TrayIconState::Problem => "Siple: registration problem".to_string(),
        TrayIconState::Dnd => "Siple: do not disturb".to_string(),
        TrayIconState::Idle => {
            let ok = snap
                .accounts
                .iter()
                .filter(|a| reg_class(a) == RegClass::Registered)
                .count();
            if accounts.is_empty() {
                "Siple: no accounts".to_string()
            } else {
                format!("Siple: {ok} of {} registered", accounts.len())
            }
        }
    }
}

/// What a tray / menu item asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayCmd {
    /// Show and raise the window.
    Show,
    /// Hide the window to the tray.
    Hide,
    /// Click on the icon: show when hidden, hide when shown.
    Toggle,
    /// Answer a ringing call.
    Answer(String),
    /// Decline a ringing call.
    Decline(String),
    /// Mute (`true`) or unmute.
    Mute(String, bool),
    /// Hold (`true`) or resume.
    Hold(String, bool),
    /// Hang up.
    Hangup(String),
    /// Switch do not disturb on / off on every account.
    SetDnd(bool),
    /// Open the settings (the window comes up).
    Settings,
    /// Quit the application.
    Quit,
}

#[cfg_attr(target_os = "linux", allow(dead_code))] // menu ids: native tray backends only
impl TrayCmd {
    /// Stable menu item id.
    pub fn to_id(&self) -> String {
        match self {
            Self::Show => "show".into(),
            Self::Hide => "hide".into(),
            Self::Toggle => "toggle".into(),
            Self::Answer(c) => format!("answer:{c}"),
            Self::Decline(c) => format!("decline:{c}"),
            Self::Mute(c, on) => format!("mute:{}:{c}", u8::from(*on)),
            Self::Hold(c, on) => format!("hold:{}:{c}", u8::from(*on)),
            Self::Hangup(c) => format!("hangup:{c}"),
            Self::SetDnd(on) => format!("dnd:{}", u8::from(*on)),
            Self::Settings => "settings".into(),
            Self::Quit => "quit".into(),
        }
    }

    /// Inverse of [`Self::to_id`]; `None` for foreign ids.
    pub fn from_id(id: &str) -> Option<Self> {
        let flag = |s: &str| match s {
            "1" => Some(true),
            "0" => Some(false),
            _ => None,
        };
        Some(match id {
            "show" => Self::Show,
            "hide" => Self::Hide,
            "toggle" => Self::Toggle,
            "settings" => Self::Settings,
            "quit" => Self::Quit,
            _ => {
                let (kind, rest) = id.split_once(':')?;
                match kind {
                    "answer" => Self::Answer(rest.into()),
                    "decline" => Self::Decline(rest.into()),
                    "hangup" => Self::Hangup(rest.into()),
                    "dnd" => Self::SetDnd(flag(rest)?),
                    "mute" | "hold" => {
                        let (on, call) = rest.split_once(':')?;
                        let on = flag(on)?;
                        if kind == "mute" {
                            Self::Mute(call.into(), on)
                        } else {
                            Self::Hold(call.into(), on)
                        }
                    }
                    _ => return None,
                }
            }
        })
    }
}

/// One entry of the rendered menu; backends map it onto their menu types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    /// Non-clickable label.
    Label(String),
    /// Clickable item.
    Item(String, TrayCmd),
    /// Check item: `(label, checked, command to send when toggled)`.
    Check(String, bool, TrayCmd),
    /// Separator.
    Separator,
}

impl TrayModel {
    /// The menu, top to bottom.
    pub fn menu(&self) -> Vec<MenuEntry> {
        let mut m = vec![MenuEntry::Label("Siple".into()), MenuEntry::Separator];
        if !self.calls.is_empty() {
            for c in &self.calls {
                match &c.kind {
                    TrayCallKind::Ringing => {
                        m.push(MenuEntry::Label(format!("{} is calling", c.who)));
                        m.push(MenuEntry::Item(
                            "Answer".into(),
                            TrayCmd::Answer(c.id.clone()),
                        ));
                        m.push(MenuEntry::Item(
                            "Decline".into(),
                            TrayCmd::Decline(c.id.clone()),
                        ));
                    }
                    TrayCallKind::Live { muted, held } => {
                        m.push(MenuEntry::Label(format!(
                            "{}{}",
                            c.who,
                            if *held { " (on hold)" } else { "" }
                        )));
                        m.push(MenuEntry::Item(
                            if *muted { "Unmute" } else { "Mute" }.into(),
                            TrayCmd::Mute(c.id.clone(), !muted),
                        ));
                        m.push(MenuEntry::Item(
                            if *held { "Resume" } else { "Hold" }.into(),
                            TrayCmd::Hold(c.id.clone(), !held),
                        ));
                        m.push(MenuEntry::Item(
                            "Hang up".into(),
                            TrayCmd::Hangup(c.id.clone()),
                        ));
                    }
                    TrayCallKind::Dialing => {
                        m.push(MenuEntry::Label(format!("Calling {}", c.who)));
                        m.push(MenuEntry::Item(
                            "Hang up".into(),
                            TrayCmd::Hangup(c.id.clone()),
                        ));
                    }
                }
            }
            m.push(MenuEntry::Separator);
        }
        m.push(if self.window_visible {
            MenuEntry::Item("Hide Siple".into(), TrayCmd::Hide)
        } else {
            MenuEntry::Item("Show Siple".into(), TrayCmd::Show)
        });
        if self.has_accounts {
            m.push(MenuEntry::Check(
                "Do not disturb".into(),
                self.dnd,
                TrayCmd::SetDnd(!self.dnd),
            ));
        }
        for (id, status) in &self.accounts {
            m.push(MenuEntry::Label(format!("{id}  {status}")));
        }
        m.push(MenuEntry::Separator);
        m.push(MenuEntry::Item("Settings…".into(), TrayCmd::Settings));
        m.push(MenuEntry::Item("Quit Siple".into(), TrayCmd::Quit));
        m
    }
}

/// Palette of the status badge drawn over the tray icon: `None` = no badge.
pub fn badge_rgb(s: TrayIconState) -> Option<[u8; 3]> {
    match s {
        TrayIconState::Idle => None,
        TrayIconState::Ringing => Some([0x2e, 0xc2, 0x5b]),
        TrayIconState::InCall => Some([0x2f, 0x7d, 0xf6]),
        TrayIconState::Problem => Some([0xe5, 0x48, 0x4d]),
        TrayIconState::Dnd => Some([0xf0, 0xa2, 0x0c]),
    }
}

/// Downscales `rgba` (square, `src` px) to `dst` px with a box filter (premultiplied) and draws
/// the status badge (a filled disc with a white ring, bottom right). Pure, so it can be tested.
pub fn render_icon(rgba: &[u8], src: usize, dst: usize, state: TrayIconState) -> Vec<u8> {
    assert_eq!(rgba.len(), src * src * 4);
    let mut out = vec![0u8; dst * dst * 4];
    for y in 0..dst {
        for x in 0..dst {
            let (x0, x1) = (x * src / dst, ((x + 1) * src / dst).max(x * src / dst + 1));
            let (y0, y1) = (y * src / dst, ((y + 1) * src / dst).max(y * src / dst + 1));
            let (mut r, mut g, mut b, mut a, mut n) = (0u32, 0u32, 0u32, 0u32, 0u32);
            for sy in y0..y1.min(src) {
                for sx in x0..x1.min(src) {
                    let p = &rgba[(sy * src + sx) * 4..][..4];
                    let al = u32::from(p[3]);
                    r += u32::from(p[0]) * al;
                    g += u32::from(p[1]) * al;
                    b += u32::from(p[2]) * al;
                    a += al;
                    n += 1;
                }
            }
            let o = &mut out[(y * dst + x) * 4..][..4];
            if a > 0 && n > 0 {
                o[0] = (r / a) as u8;
                o[1] = (g / a) as u8;
                o[2] = (b / a) as u8;
                o[3] = (a / n) as u8;
            }
        }
    }
    if let Some(c) = badge_rgb(state) {
        let d = dst as f32;
        let (cx, cy, rad) = (d * 0.72, d * 0.72, d * 0.26);
        for y in 0..dst {
            for x in 0..dst {
                let dist = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                let o = &mut out[(y * dst + x) * 4..][..4];
                if dist <= rad {
                    o.copy_from_slice(&[c[0], c[1], c[2], 255]);
                } else if dist <= rad + (d * 0.05).max(1.0) {
                    o.copy_from_slice(&[255, 255, 255, 255]);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::visual_tests::{base_snapshot, call, now};
    use siple_sip::CallState;

    fn model(f: impl FnOnce(&mut Snapshot)) -> TrayModel {
        let mut s = base_snapshot();
        f(&mut s);
        TrayModel::from_snapshot(&s, false, now())
    }

    #[test]
    fn ids_round_trip() {
        for c in [
            TrayCmd::Show,
            TrayCmd::Hide,
            TrayCmd::Toggle,
            TrayCmd::Answer("c1".into()),
            TrayCmd::Decline("c2".into()),
            TrayCmd::Mute("c1".into(), true),
            TrayCmd::Mute("c1".into(), false),
            TrayCmd::Hold("c3".into(), true),
            TrayCmd::Hangup("c1".into()),
            TrayCmd::SetDnd(true),
            TrayCmd::SetDnd(false),
            TrayCmd::Settings,
            TrayCmd::Quit,
        ] {
            assert_eq!(TrayCmd::from_id(&c.to_id()), Some(c.clone()), "{c:?}");
        }
        assert_eq!(TrayCmd::from_id("nope"), None);
        assert_eq!(TrayCmd::from_id("mute:2:c1"), None);
        assert_eq!(TrayCmd::from_id("zzz:c1"), None);
    }

    #[test]
    fn idle_model_has_no_calls_and_a_quiet_icon() {
        let m = model(|_| {});
        assert!(m.calls.is_empty());
        assert!(matches!(
            m.icon,
            TrayIconState::Idle | TrayIconState::Problem | TrayIconState::Dnd
        ));
        let menu = m.menu();
        assert_eq!(menu.first(), Some(&MenuEntry::Label("Siple".into())));
        assert!(menu.contains(&MenuEntry::Item("Show Siple".into(), TrayCmd::Show)));
        assert_eq!(
            menu.last(),
            Some(&MenuEntry::Item("Quit Siple".into(), TrayCmd::Quit))
        );
    }

    #[test]
    fn ringing_call_offers_answer_and_decline_and_wins_the_icon() {
        let m = model(|s| {
            s.calls = vec![
                call(
                    "c1",
                    CallDirection::Inbound,
                    CallState::Ringing,
                    ("Bob", "sip:1@x"),
                ),
                call(
                    "c2",
                    CallDirection::Outbound,
                    CallState::Active,
                    ("Carol", "sip:2@x"),
                ),
            ];
        });
        assert_eq!(m.icon, TrayIconState::Ringing);
        let menu = m.menu();
        assert!(menu.contains(&MenuEntry::Item(
            "Answer".into(),
            TrayCmd::Answer("c1".into())
        )));
        assert!(menu.contains(&MenuEntry::Item(
            "Decline".into(),
            TrayCmd::Decline("c1".into())
        )));
        assert!(menu.contains(&MenuEntry::Item(
            "Mute".into(),
            TrayCmd::Mute("c2".into(), true)
        )));
        assert!(menu.contains(&MenuEntry::Item(
            "Hang up".into(),
            TrayCmd::Hangup("c2".into())
        )));
        assert!(m.tooltip.contains("Bob"));
    }

    #[test]
    fn live_call_toggles_reflect_state() {
        let m = model(|s| {
            let mut c = call(
                "c1",
                CallDirection::Outbound,
                CallState::Held,
                ("Bob", "sip:1@x"),
            );
            c.muted = true;
            c.hold.local = true;
            s.calls = vec![c];
        });
        assert_eq!(m.icon, TrayIconState::InCall);
        let menu = m.menu();
        assert!(menu.contains(&MenuEntry::Item(
            "Unmute".into(),
            TrayCmd::Mute("c1".into(), false)
        )));
        assert!(menu.contains(&MenuEntry::Item(
            "Resume".into(),
            TrayCmd::Hold("c1".into(), false)
        )));
    }

    #[test]
    fn dnd_requires_every_enabled_account() {
        let m = model(|s| {
            for a in &mut s.accounts {
                a.dnd = true;
            }
        });
        if m.has_accounts {
            assert!(m.dnd);
            assert!(
                m.menu()
                    .iter()
                    .any(|e| matches!(e, MenuEntry::Check(_, true, TrayCmd::SetDnd(false))))
            );
        }
        let m = model(|s| {
            if let Some(a) = s.accounts.first_mut() {
                a.dnd = false;
            }
        });
        assert!(!m.dnd);
    }

    #[test]
    fn visible_window_offers_hide() {
        let s = base_snapshot();
        let m = TrayModel::from_snapshot(&s, true, now());
        assert!(
            m.menu()
                .contains(&MenuEntry::Item("Hide Siple".into(), TrayCmd::Hide))
        );
    }

    #[test]
    fn icon_rendering_scales_and_badges() {
        let src = 8;
        let rgba: Vec<u8> = (0..src * src).flat_map(|_| [10u8, 20, 30, 255]).collect();
        let out = render_icon(&rgba, src, 4, TrayIconState::Idle);
        assert_eq!(out.len(), 4 * 4 * 4);
        assert_eq!(&out[..4], &[10, 20, 30, 255]);
        let badged = render_icon(&rgba, src, 16, TrayIconState::Problem);
        let px = |x: usize, y: usize| badged[(y * 16 + x) * 4..][..4].to_vec();
        assert_eq!(px(11, 11), vec![0xe5, 0x48, 0x4d, 255]);
        assert_eq!(px(1, 1), vec![10, 20, 30, 255]);
    }
}
