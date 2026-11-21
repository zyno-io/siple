//! Linux tray: a StatusNotifierItem over D-Bus (`ksni`, pure Rust; no GTK, no libappindicator, so
//! the packaging NEEDED-library rule holds). Needs a StatusNotifierHost (KDE, XFCE, and GNOME
//! with the AppIndicator extension); without one [`spawn`] fails and the shell makes closing the
//! window minimize it instead, so the app never becomes unreachable.
//!
//! In a Flatpak the tray registers under its unique bus name only (no `--own-name` grant): ksni's
//! well-known name embeds a counter that changes each time the tray is recreated.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ksni::menu::{CheckmarkItem, MenuItem, StandardItem};
use ksni::{Category, Handle, Icon, OfflineReason, Status, ToolTip, Tray, TrayMethods};

use super::tray_model::{MenuEntry, TrayCmd, TrayIconState, TrayModel, render_icon};
use super::{ShellMsg, ShellTx};

const ICON_PX: usize = 48;

/// Whether the tray asks for the well-known `org.kde.StatusNotifierItem-<pid>-<n>` name. A
/// Flatpak has no grant for it, so it registers by its unique name.
fn owns_bus_name(c: &siple_core::config::channel::Channel) -> bool {
    c.kind != siple_core::config::channel::ChannelKind::Flatpak
}

pub struct SipleTray {
    model: TrayModel,
    pixmap: Vec<Icon>,
    tx: ShellTx,
    /// A StatusNotifierHost is showing us (false while none is running).
    online: Arc<AtomicBool>,
}

/// ARGB32, network byte order, from RGBA.
fn argb(rgba: &[u8]) -> Vec<u8> {
    rgba.as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[3], p[0], p[1], p[2]])
        .collect()
}

fn pixmap(base: &Option<(Vec<u8>, usize)>, state: TrayIconState) -> Vec<Icon> {
    match base {
        Some((rgba, side)) => vec![Icon {
            width: ICON_PX as i32,
            height: ICON_PX as i32,
            data: argb(&render_icon(rgba, *side, ICON_PX, state)),
        }],
        None => Vec::new(),
    }
}

impl Tray for SipleTray {
    fn id(&self) -> String {
        siple_core::config::APP_ID.into()
    }
    fn category(&self) -> Category {
        Category::Communications
    }
    fn title(&self) -> String {
        "Siple".into()
    }
    fn status(&self) -> Status {
        if self.model.icon == TrayIconState::Ringing {
            Status::NeedsAttention
        } else {
            Status::Active
        }
    }
    fn icon_name(&self) -> String {
        // Used when the host has no pixmap support; the theme usually has "siple" once installed.
        if self.pixmap.is_empty() {
            "siple".into()
        } else {
            String::new()
        }
    }
    fn icon_pixmap(&self) -> Vec<Icon> {
        self.pixmap.clone()
    }
    fn attention_icon_pixmap(&self) -> Vec<Icon> {
        self.pixmap.clone()
    }
    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: self.model.tooltip.clone(),
            ..Default::default()
        }
    }
    fn watcher_online(&self) {
        self.online.store(true, Ordering::SeqCst);
        self.tx.send(ShellMsg::Wake);
    }
    fn watcher_offline(&self, _reason: OfflineReason) -> bool {
        // Keep the service: the icon reappears when a host (re)starts.
        self.online.store(false, Ordering::SeqCst);
        self.tx.send(ShellMsg::Wake);
        true
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        self.tx.send(ShellMsg::Tray(TrayCmd::Toggle));
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        self.model
            .menu()
            .into_iter()
            .map(|e| match e {
                MenuEntry::Label(l) => StandardItem {
                    label: l,
                    enabled: false,
                    ..Default::default()
                }
                .into(),
                MenuEntry::Item(l, cmd) => StandardItem {
                    label: l,
                    activate: Box::new(move |t: &mut Self| t.tx.send(ShellMsg::Tray(cmd.clone()))),
                    ..Default::default()
                }
                .into(),
                MenuEntry::Check(l, on, cmd) => CheckmarkItem {
                    label: l,
                    checked: on,
                    activate: Box::new(move |t: &mut Self| t.tx.send(ShellMsg::Tray(cmd.clone()))),
                    ..Default::default()
                }
                .into(),
                MenuEntry::Separator => MenuItem::Separator,
            })
            .collect()
    }
}

/// Handle to the running tray service.
#[derive(Clone)]
pub struct LinuxTray {
    handle: Handle<SipleTray>,
    online: Arc<AtomicBool>,
    models: tokio::sync::watch::Sender<TrayModel>,
    rt: tokio::runtime::Handle,
}

impl LinuxTray {
    /// Starts the service on `rt`; reports the outcome as [`ShellMsg::LinuxTray`]. The service
    /// is started even when no StatusNotifierHost runs yet (login-time autostart often beats the
    /// panel): the icon appears when one does, and [`Self::is_online`] says whether it is shown.
    pub fn spawn(
        rt: &tokio::runtime::Handle,
        tx: ShellTx,
        base: Option<(Vec<u8>, usize)>,
        model: TrayModel,
    ) {
        let rt2 = rt.clone();
        rt.spawn(async move {
            let online = Arc::new(AtomicBool::new(true));
            let tray = SipleTray {
                pixmap: pixmap(&base, model.icon),
                model: model.clone(),
                tx: tx.clone(),
                online: online.clone(),
            };
            let result = match tray
                .assume_sni_available(true)
                .disable_dbus_name(!owns_bus_name(siple_core::config::channel::current()))
                .spawn()
                .await
            {
                Ok(handle) => {
                    // One task applies updates in order (separate tasks could land reordered).
                    let (models, mut rx) = tokio::sync::watch::channel(model);
                    let h = handle.clone();
                    tokio::spawn(async move {
                        while rx.changed().await.is_ok() {
                            let m = rx.borrow_and_update().clone();
                            let base = base.clone();
                            if h.update(move |t: &mut SipleTray| {
                                t.pixmap = pixmap(&base, m.icon);
                                t.model = m;
                            })
                            .await
                            .is_none()
                            {
                                break;
                            }
                        }
                    });
                    Ok(LinuxTray {
                        handle,
                        online,
                        models,
                        rt: rt2,
                    })
                }
                Err(e) => Err(e.to_string()),
            };
            tx.send(ShellMsg::LinuxTray(result));
        });
    }

    /// Whether the service died.
    pub fn is_closed(&self) -> bool {
        self.handle.is_closed()
    }

    /// Whether a StatusNotifierHost is currently showing the icon.
    pub fn is_online(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }

    pub fn update(&self, model: &TrayModel) {
        let _ = self.models.send(model.clone());
    }

    pub fn shutdown(&self) {
        let h = self.handle.clone();
        self.rt.spawn(async move {
            h.shutdown().await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::owns_bus_name;
    use siple_core::config::channel::Channel;

    #[test]
    fn only_a_flatpak_skips_the_well_known_bus_name() {
        assert!(owns_bus_name(&Channel::DIRECT));
        assert!(!owns_bus_name(&Channel::FLATPAK));
    }

    /// ksni's builder has no getters, so check that the spawn applies the decision at all.
    #[test]
    fn the_spawn_applies_the_bus_name_decision() {
        let src = include_str!("tray_linux.rs");
        let code = src.split("#[cfg(test)]").next().unwrap();
        assert!(code.contains(".disable_dbus_name(!owns_bus_name("));
    }
}
