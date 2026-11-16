//! Window / attention / notification decisions as pure functions of config and state,
//! so they can be tested without a window system.

use siple_core::config::UiConfig;
use siple_sip::{CallDirection, CallInfo, CallState, TransferPhase};

/// What closing the window does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    /// Let the close through: the app exits.
    Quit,
    /// Hide the window into the tray / menu bar.
    HideToTray,
    /// Minimize instead: the tray is wanted but there is none to find the app by (no
    /// StatusNotifier host, or the icon could not be created), so hiding would lose the app.
    Minimize,
}

/// Decides what a close request does. `quitting` is set by every real quit path (tray Quit, menu
/// Quit, Ctrl+Q, SIGTERM); `tray_ready` is whether a tray icon is actually showing.
pub fn close_action(ui: &UiConfig, tray_ready: bool, quitting: bool) -> CloseAction {
    if quitting || !ui.tray || !ui.close_to_tray {
        CloseAction::Quit
    } else if tray_ready {
        CloseAction::HideToTray
    } else {
        CloseAction::Minimize
    }
}

/// How the window leaves the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HideStyle {
    /// `Visible(false)`.
    Hide,
    /// `Minimized(true)`: keeps the taskbar button (`hide_from_taskbar = false`, Windows / Linux).
    Minimize,
}

/// Plan for hiding to the tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HidePlan {
    /// How to take the window away.
    pub style: HideStyle,
    /// macOS: switch to the accessory activation policy (no Dock icon) while hidden.
    pub drop_dock_icon: bool,
}

/// `macos`: whether the Dock model applies (a hidden window there never has a Dock tile of its
/// own; the app does, until the activation policy changes).
pub fn hide_plan(ui: &UiConfig, macos: bool) -> HidePlan {
    if macos {
        HidePlan {
            style: HideStyle::Hide,
            drop_dock_icon: ui.hide_from_taskbar,
        }
    } else {
        HidePlan {
            style: if ui.hide_from_taskbar {
                HideStyle::Hide
            } else {
                HideStyle::Minimize
            },
            drop_dock_icon: false,
        }
    }
}

/// What to do about a newly ringing inbound call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IncomingEffects {
    /// Show and focus the window.
    pub raise: bool,
    /// Ask the OS for attention (taskbar flash / Dock bounce).
    pub attention: bool,
    /// Post a desktop notification.
    pub notify: bool,
}

/// Effects of an incoming call. `focused` is whether our window has keyboard focus (a hidden
/// window never does). A visible, focused window needs no notification: the banner is enough.
pub fn incoming_effects(ui: &UiConfig, focused: bool) -> IncomingEffects {
    IncomingEffects {
        raise: ui.show_on_incoming && !focused,
        attention: !focused,
        notify: ui.notifications && !focused,
    }
}

/// Whether a window we think is hidden has been brought back by the user (taskbar, Dock,
/// compositor): the OS reports it focused, and not right after we hid it (the focus report that
/// races the hide). Never for a window we unmapped: only our own show paths can bring that back,
/// and its focus report is stale (X11 keeps the pre-unmap focus), which would make a hidden
/// window look visible and focused, so incoming calls would neither notify nor raise it.
pub fn user_restored(
    visible: bool,
    unmapped: bool,
    os_focused: bool,
    since_hidden: Option<std::time::Duration>,
) -> bool {
    !visible
        && !unmapped
        && os_focused
        && since_hidden.is_none_or(|t| t > std::time::Duration::from_millis(500))
}

/// Whether answering a call should bring the window up (`show_on_answer`, only when hidden; an
/// answer from the visible window has nothing to raise).
pub fn raise_on_answer(ui: &UiConfig, visible: bool) -> bool {
    ui.show_on_answer && !visible
}

/// A desktop notification to post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Toast {
    /// A call is ringing: `(call id, who)`.
    Incoming(String, String),
    /// A call the caller abandoned: `who`.
    Missed(String),
    /// A transfer finished: `(headline, detail)`.
    Transfer(String, String),
}

/// Something that happened between two snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallEvent {
    /// A new inbound call is ringing.
    Ringing(String),
    /// A call became active.
    Answered(String),
    /// An inbound call ended unanswered because the caller gave up.
    Missed(String),
    /// An inbound call stopped ringing (answered, declined, cancelled or gone).
    RingStopped(String),
    /// A transfer reached a terminal phase.
    TransferDone(String, TransferPhase),
}

/// Remembers what the previous snapshot looked like to turn snapshots into [`CallEvent`]s.
#[derive(Debug, Default)]
pub struct EventTracker {
    calls: std::collections::HashMap<String, CallState>,
    transfers: std::collections::HashSet<String>,
    /// Inbound calls seen ringing.
    ringing: std::collections::HashSet<String>,
    primed: bool,
}

impl EventTracker {
    /// Compares `calls` and `transfers` with the previous call. The first call only records
    /// (calls that exist at startup do not ring again).
    pub fn update(
        &mut self,
        calls: &[CallInfo],
        transfers: &[siple_sip::TransferSession],
    ) -> Vec<CallEvent> {
        let mut out = Vec::new();
        for c in calls {
            let prev = self.calls.get(&c.id).copied();
            if self.primed {
                let inbound = c.direction == CallDirection::Inbound;
                match (prev, c.state) {
                    (None, CallState::Ringing | CallState::Early) if inbound => {
                        out.push(CallEvent::Ringing(c.id.clone()));
                    }
                    (Some(p), CallState::Active)
                        if p != CallState::Active
                            && p != CallState::Held
                            && p != CallState::RemoteHeld =>
                    {
                        out.push(CallEvent::Answered(c.id.clone()));
                    }
                    (None, CallState::Active) => out.push(CallEvent::Answered(c.id.clone())),
                    (Some(p), CallState::Ended)
                        if p != CallState::Ended
                            && inbound
                            && c.end.as_ref().is_some_and(|e| e.missed) =>
                    {
                        out.push(CallEvent::Missed(c.id.clone()));
                    }
                    _ => {}
                }
            }
            self.calls.insert(c.id.clone(), c.state);
        }
        self.calls.retain(|id, _| calls.iter().any(|c| &c.id == id));
        let now_ringing: std::collections::HashSet<String> = calls
            .iter()
            .filter(|c| is_ringing_inbound(c))
            .map(|c| c.id.clone())
            .collect();
        for id in self.ringing.difference(&now_ringing) {
            out.push(CallEvent::RingStopped(id.clone()));
        }
        self.ringing = now_ringing;
        for s in transfers {
            if s.phase.is_terminal() && self.transfers.insert(s.id.clone()) && self.primed {
                out.push(CallEvent::TransferDone(s.id.clone(), s.phase));
            }
        }
        if !self.primed {
            // Sessions already finished at startup are not news.
            for s in transfers.iter().filter(|s| s.phase.is_terminal()) {
                self.transfers.insert(s.id.clone());
            }
        }
        self.primed = true;
        out
    }
}

/// An inbound call that is still ringing (the only kind Answer / Decline apply to).
pub fn is_ringing_inbound(c: &CallInfo) -> bool {
    c.direction == CallDirection::Inbound
        && matches!(c.state, CallState::Ringing | CallState::Early)
}

/// Whether Answer / Decline for `call_id` still means something: a notification button or menu
/// item can outlive its call, and must then do nothing (not answer a different call, not
/// raise an error notice).
pub fn ring_is_actionable(calls: &[CallInfo], call_id: &str) -> bool {
    calls
        .iter()
        .any(|c| c.id == call_id && is_ringing_inbound(c))
}

/// Headline and detail of a finished transfer, or `None` for outcomes not worth a toast
/// (cancelled by the user).
pub fn transfer_toast(phase: TransferPhase, target: &str) -> Option<(String, String)> {
    match phase {
        TransferPhase::Transferred => Some((
            "Transfer completed".into(),
            format!("Call sent to {target}"),
        )),
        TransferPhase::Failed => Some((
            "Transfer failed".into(),
            format!("Could not transfer to {target}"),
        )),
        TransferPhase::Abandoned => Some((
            "Transfer not completed".into(),
            format!("The call ended before it reached {target}"),
        )),
        _ => None,
    }
}

/// Clicks closer together than this are one double click (Windows default 500 ms).
pub const DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(500);

/// What a left click on the tray icon does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickAction {
    /// Show and raise the window.
    Show,
    /// Hide it again.
    Hide,
    /// Part of a double click: do nothing.
    Ignore,
}

/// Windows delivers a click per button-up, so a double click arrives twice; the second must not
/// undo the first. A click shows and raises the window unless it is already shown *and*
/// focused, in which case it hides it. `since_last` is the time since the previous accepted
/// click.
pub fn tray_click(
    visible: bool,
    focused: bool,
    since_last: Option<std::time::Duration>,
) -> ClickAction {
    if since_last.is_some_and(|d| d < DOUBLE_CLICK) {
        ClickAction::Ignore
    } else if visible && focused {
        ClickAction::Hide
    } else {
        ClickAction::Show
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ui(f: impl FnOnce(&mut UiConfig)) -> UiConfig {
        let mut u = UiConfig::default();
        f(&mut u);
        u
    }

    #[test]
    fn a_focus_report_restores_only_a_minimized_window() {
        use std::time::Duration as D;
        let old = Some(D::from_secs(2));
        // Minimized (taskbar restore): focus after the grace period means the user is back.
        assert!(user_restored(false, false, true, old));
        assert!(user_restored(false, false, true, None));
        // The focus report racing our own hide is ignored.
        assert!(!user_restored(
            false,
            false,
            true,
            Some(D::from_millis(100))
        ));
        assert!(!user_restored(false, false, false, old));
        assert!(!user_restored(true, false, true, old));
        // Unmapped by us (hidden to the tray): a stale focus report (X11) must not revive it.
        assert!(!user_restored(false, true, true, old));
        assert!(!user_restored(false, true, true, None));
    }

    #[test]
    fn close_decision_table() {
        let d = UiConfig::default();
        assert_eq!(close_action(&d, true, false), CloseAction::HideToTray);
        assert_eq!(close_action(&d, false, false), CloseAction::Minimize);
        // A real quit always goes through, whatever the config says.
        assert_eq!(close_action(&d, true, true), CloseAction::Quit);
        assert_eq!(close_action(&d, false, true), CloseAction::Quit);
        // close_to_tray = false: close quits.
        assert_eq!(
            close_action(&ui(|u| u.close_to_tray = false), true, false),
            CloseAction::Quit
        );
        // tray = false forces close to quit even with close_to_tray = true.
        assert_eq!(
            close_action(&ui(|u| u.tray = false), true, false),
            CloseAction::Quit
        );
        assert_eq!(
            close_action(&ui(|u| u.tray = false), false, false),
            CloseAction::Quit
        );
    }

    #[test]
    fn hide_plan_per_platform() {
        let d = UiConfig::default();
        assert_eq!(
            hide_plan(&d, true),
            HidePlan {
                style: HideStyle::Hide,
                drop_dock_icon: true
            }
        );
        assert_eq!(
            hide_plan(&d, false),
            HidePlan {
                style: HideStyle::Hide,
                drop_dock_icon: false
            }
        );
        let keep = ui(|u| u.hide_from_taskbar = false);
        assert_eq!(
            hide_plan(&keep, true),
            HidePlan {
                style: HideStyle::Hide,
                drop_dock_icon: false
            }
        );
        assert_eq!(
            hide_plan(&keep, false),
            HidePlan {
                style: HideStyle::Minimize,
                drop_dock_icon: false
            }
        );
    }

    #[test]
    fn incoming_effects_follow_config_and_focus() {
        let d = UiConfig::default();
        assert_eq!(
            incoming_effects(&d, false),
            IncomingEffects {
                raise: true,
                attention: true,
                notify: true
            }
        );
        // A focused window shows the banner itself.
        assert_eq!(incoming_effects(&d, true), IncomingEffects::default());
        let quiet = ui(|u| {
            u.show_on_incoming = false;
            u.notifications = false;
        });
        assert_eq!(
            incoming_effects(&quiet, false),
            IncomingEffects {
                raise: false,
                attention: true,
                notify: false
            }
        );
    }

    #[test]
    fn answer_raises_only_when_hidden_and_enabled() {
        let on = ui(|u| u.show_on_answer = true);
        assert!(raise_on_answer(&on, false));
        assert!(!raise_on_answer(&on, true));
        assert!(!raise_on_answer(&UiConfig::default(), false));
    }

    #[test]
    fn transfer_toasts() {
        assert!(
            transfer_toast(TransferPhase::Transferred, "1002")
                .unwrap()
                .0
                .contains("completed")
        );
        assert!(
            transfer_toast(TransferPhase::Failed, "1002")
                .unwrap()
                .0
                .contains("failed")
        );
        assert!(transfer_toast(TransferPhase::Cancelled, "1002").is_none());
    }

    use crate::visual_tests::{call, session};
    use siple_sip::CallEnd;

    fn ring(id: &str) -> CallInfo {
        call(
            id,
            CallDirection::Inbound,
            CallState::Ringing,
            ("Bob", "sip:1002@pbx.example.test"),
        )
    }

    #[test]
    fn tracker_reports_new_ringing_answered_and_missed() {
        let mut t = EventTracker::default();
        // RingStopped has its own test; here only the other events matter.
        let upd = |t: &mut EventTracker, c: &[CallInfo], x: &[siple_sip::TransferSession]| {
            t.update(c, x)
                .into_iter()
                .filter(|e| !matches!(e, CallEvent::RingStopped(_)))
                .collect::<Vec<_>>()
        };
        // Startup: an already ringing call is not news.
        assert!(upd(&mut t, &[ring("c1")], &[]).is_empty());
        assert_eq!(
            upd(&mut t, &[ring("c1"), ring("c2")], &[]),
            vec![CallEvent::Ringing("c2".into())]
        );
        let mut c1 = ring("c1");
        c1.state = CallState::Active;
        assert_eq!(
            upd(&mut t, &[c1.clone(), ring("c2")], &[]),
            vec![CallEvent::Answered("c1".into())]
        );
        // Hold and resume is not an answer.
        let mut held = c1.clone();
        held.state = CallState::Held;
        assert!(upd(&mut t, &[held.clone(), ring("c2")], &[]).is_empty());
        assert!(upd(&mut t, &[c1.clone(), ring("c2")], &[]).is_empty());
        // The caller gives up on c2.
        let mut gone = ring("c2");
        gone.state = CallState::Ended;
        gone.end = Some(CallEnd {
            code: 487,
            reason: "Request Terminated".into(),
            by: siple_sip::EndedBy::Remote,
            missed: true,
            q850_cause: None,
            cause: siple_sip::EndCause::Missed,
        });
        assert_eq!(
            upd(&mut t, &[c1.clone(), gone.clone()], &[]),
            vec![CallEvent::Missed("c2".into())]
        );
        // Not reported twice.
        assert!(upd(&mut t, &[c1, gone], &[]).is_empty());
    }

    #[test]
    fn tracker_reports_a_transfer_result_once() {
        let mut t = EventTracker::default();
        assert!(
            t.update(&[], &[session(TransferPhase::Referring)])
                .is_empty()
        );
        let done = session(TransferPhase::Transferred);
        assert_eq!(
            t.update(&[], std::slice::from_ref(&done)),
            vec![CallEvent::TransferDone(
                "t1".into(),
                TransferPhase::Transferred
            )]
        );
        assert!(t.update(&[], &[done]).is_empty());
        // A session that was already finished at startup stays quiet.
        let mut t = EventTracker::default();
        assert!(t.update(&[], &[session(TransferPhase::Failed)]).is_empty());
    }

    #[test]
    fn tray_click_raises_unless_shown_and_focused_and_ignores_double_clicks() {
        use std::time::Duration as D;
        // Hidden: show.
        assert_eq!(tray_click(false, false, None), ClickAction::Show);
        // Shown but behind other windows: raise, do not hide.
        assert_eq!(tray_click(true, false, None), ClickAction::Show);
        // Shown and focused: hide.
        assert_eq!(tray_click(true, true, None), ClickAction::Hide);
        // The second click of a double click is ignored, whatever the state became.
        assert_eq!(
            tray_click(true, true, Some(D::from_millis(200))),
            ClickAction::Ignore
        );
        assert_eq!(
            tray_click(false, false, Some(D::from_millis(100))),
            ClickAction::Ignore
        );
        // A later click counts again.
        assert_eq!(
            tray_click(true, true, Some(D::from_millis(900))),
            ClickAction::Hide
        );
    }

    #[test]
    fn ring_stop_is_reported_once_and_stale_actions_are_inert() {
        let mut t = EventTracker::default();
        t.update(&[], &[]);
        let r = ring("c1");
        assert_eq!(
            t.update(std::slice::from_ref(&r), &[]),
            vec![CallEvent::Ringing("c1".into())]
        );
        assert!(ring_is_actionable(std::slice::from_ref(&r), "c1"));
        assert!(!ring_is_actionable(std::slice::from_ref(&r), "c2"));
        let mut answered = r.clone();
        answered.state = CallState::Active;
        assert!(!ring_is_actionable(std::slice::from_ref(&answered), "c1"));
        let ev = t.update(std::slice::from_ref(&answered), &[]);
        assert!(ev.contains(&CallEvent::RingStopped("c1".into())), "{ev:?}");
        assert!(
            !t.update(&[answered], &[])
                .contains(&CallEvent::RingStopped("c1".into()))
        );
        // A ringing call that simply vanishes also stops.
        let mut t = EventTracker::default();
        t.update(&[], &[]);
        t.update(&[ring("c9")], &[]);
        assert_eq!(
            t.update(&[], &[]),
            vec![CallEvent::RingStopped("c9".into())]
        );
    }
}
