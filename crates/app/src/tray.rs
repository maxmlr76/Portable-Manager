//! System-tray icon (Windows): keep the app alive when the main window is closed,
//! offer a menu (Mini-Ansicht / Hauptfenster / Beenden) and a left-click quick view.

#[cfg(windows)]
pub use imp::{setup, TrayGuard};

#[cfg(windows)]
mod imp {
    use std::time::Duration;

    use slint::{Timer, TimerMode};
    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    /// Keeps the tray icon + poll timer alive for the lifetime of the app.
    pub struct TrayGuard {
        _tray: TrayIcon,
        _timer: Timer,
    }

    /// Decode the embedded 32×32 tray PNG into RGBA.
    fn icon_rgba() -> Option<(Vec<u8>, u32, u32)> {
        let bytes: &[u8] = include_bytes!("../assets/tray-32.png");
        let decoder = png::Decoder::new(bytes);
        let mut reader = decoder.read_info().ok()?;
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).ok()?;
        let data = &buf[..info.buffer_size()];
        let rgba = match info.color_type {
            png::ColorType::Rgba => data.to_vec(),
            png::ColorType::Rgb => {
                let mut out = Vec::with_capacity((info.width * info.height * 4) as usize);
                for px in data.chunks_exact(3) {
                    out.extend_from_slice(px);
                    out.push(255);
                }
                out
            }
            _ => return None,
        };
        Some((rgba, info.width, info.height))
    }

    /// Create the tray icon and start polling menu/tray events on the UI thread.
    pub fn setup() -> Option<TrayGuard> {
        let (rgba, w, h) = icon_rgba()?;
        let icon = Icon::from_rgba(rgba, w, h).ok()?;

        let menu = Menu::new();
        let mi_mini = MenuItem::new("Schnellstart (Mini-Ansicht)", true, None);
        let mi_open = MenuItem::new("Hauptfenster öffnen", true, None);
        let mi_about = MenuItem::new("Über Maxtrix Portable Manager", true, None);
        let mi_quit = MenuItem::new("Beenden", true, None);
        // What the window's X does (asked once at the first close; changeable here).
        let keep = crate::close_action().as_deref() != Some("quit");
        let mi_keep = CheckMenuItem::new("Beim Schließen im Infobereich bleiben", true, keep, None);
        // Rescue when the window stays blank/white (GPU driver): software renderer.
        let mi_soft = CheckMenuItem::new(
            "Einfache Grafik (bei Anzeigefehlern, ab Neustart)",
            true,
            crate::soft_render_setting(),
            None,
        );
        menu.append(&mi_mini).ok()?;
        menu.append(&mi_open).ok()?;
        menu.append(&PredefinedMenuItem::separator()).ok()?;
        menu.append(&mi_keep).ok()?;
        menu.append(&mi_soft).ok()?;
        menu.append(&PredefinedMenuItem::separator()).ok()?;
        menu.append(&mi_about).ok()?;
        menu.append(&mi_quit).ok()?;
        let id_mini = mi_mini.id().clone();
        let id_open = mi_open.id().clone();
        let id_about = mi_about.id().clone();
        let id_quit = mi_quit.id().clone();
        let id_keep = mi_keep.id().clone();
        let id_soft = mi_soft.id().clone();

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            // Left click = quick view, right click = menu. By default the library opens
            // the menu on left click too and still reports the click: picking
            // "Schnellstart" then opened the window and the click closed it again.
            .with_menu_on_left_click(false)
            .with_tooltip("Maxtrix Portable Manager")
            .with_icon(icon)
            .build()
            .ok()?;

        let timer = Timer::default();
        timer.start(TimerMode::Repeated, Duration::from_millis(150), move || {
            while let Ok(ev) = MenuEvent::receiver().try_recv() {
                if ev.id == id_open {
                    crate::show_main_window();
                } else if ev.id == id_mini {
                    crate::show_mini_window();
                } else if ev.id == id_about {
                    crate::show_about();
                } else if ev.id == id_quit {
                    crate::quit_app();
                } else if ev.id == id_keep {
                    // The menu toggles the check mark itself; store the new state.
                    crate::set_close_to_tray(mi_keep.is_checked());
                } else if ev.id == id_soft {
                    crate::set_soft_render(mi_soft.is_checked());
                }
            }
            while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = ev
                {
                    crate::toggle_mini_window();
                }
            }
        });

        Some(TrayGuard { _tray: tray, _timer: timer })
    }
}
