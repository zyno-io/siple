//! Tray icon for macOS (`NSStatusItem`) and Windows (`Shell_NotifyIcon`) through `tray-icon`.
//!
//! Must be created and updated on the main thread (the shell does both from
//! `eframe::App::logic`). Menu and click events arrive through `tray-icon`'s global handlers and
//! are forwarded to the shell as [`ShellMsg::Tray`].

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

use super::tray_model::{MenuEntry, TrayCmd, TrayIconState, TrayModel, render_icon};
use super::{ShellMsg, ShellTx};

/// Size the icon is rendered at (2x of the 22 pt menu-bar / 16 px tray slot; the OS scales).
const ICON_PX: usize = 44;

/// Installs the global menu / click handlers once. Safe to call again (replaces the handlers).
pub fn install_handlers(tx: &ShellTx) {
    let t = tx.clone();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if let Some(cmd) = TrayCmd::from_id(e.id().as_ref()) {
            // A shortcut chosen from the keyboard goes straight to the window's view, in order
            // with the keys after it; see `macos::forward_shortcut`.
            #[cfg(target_os = "macos")]
            if let TrayCmd::Edit(c) = &cmd
                && super::macos::forward_shortcut(*c)
            {
                return;
            }
            t.send(ShellMsg::Tray(cmd));
        }
    }));
    #[cfg(target_os = "windows")]
    {
        use tray_icon::{MouseButton, MouseButtonState};
        let t = tx.clone();
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = e
            {
                t.send(ShellMsg::Tray(TrayCmd::Toggle));
            }
        }));
    }
    #[cfg(not(target_os = "windows"))]
    TrayIconEvent::set_event_handler(None::<fn(TrayIconEvent)>);
}

pub struct NativeTray {
    icon: TrayIcon,
    base: Option<(Vec<u8>, usize)>,
    shown: Option<(TrayIconState, String)>,
    menu_for: Option<Vec<MenuEntry>>,
}

fn icon_for(base: &(Vec<u8>, usize), state: TrayIconState) -> Result<Icon, String> {
    let rgba = render_icon(&base.0, base.1, ICON_PX, state);
    Icon::from_rgba(rgba, ICON_PX as u32, ICON_PX as u32).map_err(|e| e.to_string())
}

fn build_menu(entries: &[MenuEntry]) -> Result<Menu, String> {
    let menu = Menu::new();
    for e in entries {
        match e {
            MenuEntry::Label(l) => menu
                .append(&MenuItem::new(l, false, None))
                .map_err(|e| e.to_string())?,
            MenuEntry::Item(l, cmd) => menu
                .append(&MenuItem::with_id(MenuId::new(cmd.to_id()), l, true, None))
                .map_err(|e| e.to_string())?,
            MenuEntry::Check(l, on, cmd) => menu
                .append(&CheckMenuItem::with_id(
                    MenuId::new(cmd.to_id()),
                    l,
                    true,
                    *on,
                    None,
                ))
                .map_err(|e| e.to_string())?,
            MenuEntry::Separator => menu
                .append(&PredefinedMenuItem::separator())
                .map_err(|e| e.to_string())?,
        }
    }
    Ok(menu)
}

impl NativeTray {
    /// Creates the icon with `model`'s state. `base` is the app icon as `(rgba, side)`.
    pub fn new(base: Option<(Vec<u8>, usize)>, model: &TrayModel) -> Result<Self, String> {
        let base_icon = base
            .as_ref()
            .ok_or_else(|| "no tray icon image".to_string())?;
        let menu = build_menu(&model.menu())?;
        let builder = TrayIconBuilder::new()
            .with_id(siple_core::config::APP_ID)
            .with_icon(icon_for(base_icon, model.icon)?)
            .with_tooltip(&model.tooltip)
            .with_menu(Box::new(menu))
            // Windows: left click shows the window (handled in `install_handlers`), right click
            // opens the menu. macOS: a click opens the menu.
            .with_menu_on_left_click(cfg!(target_os = "macos"));
        let icon = builder.build().map_err(|e| e.to_string())?;
        Ok(Self {
            icon,
            base,
            shown: Some((model.icon, model.tooltip.clone())),
            menu_for: Some(model.menu()),
        })
    }

    /// Brings the icon, tooltip and menu in line with `model`; only touches what changed (a
    /// rebuilt menu closes an open one).
    pub fn update(&mut self, model: &TrayModel) {
        let want = (model.icon, model.tooltip.clone());
        if self.shown.as_ref() != Some(&want) {
            if let Some(base) = &self.base {
                match icon_for(base, model.icon) {
                    Ok(i) => {
                        let _ = self.icon.set_icon(Some(i));
                    }
                    Err(e) => tracing::debug!("tray icon: {e}"),
                }
            }
            let _ = self.icon.set_tooltip(Some(&model.tooltip));
            self.shown = Some(want);
        }
        let menu = model.menu();
        if self.menu_for.as_ref() != Some(&menu) {
            match build_menu(&menu) {
                Ok(m) => {
                    self.icon.set_menu(Some(Box::new(m)));
                    self.menu_for = Some(menu);
                }
                Err(e) => tracing::debug!("tray menu: {e}"),
            }
        }
    }
}
