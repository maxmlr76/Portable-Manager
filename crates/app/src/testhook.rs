//! Debug-only UI test driver (not compiled into release builds).
//!
//! `MAXTRIX_SCRIPT` runs a sequence of steps inside the app, separated by `;`:
//! `size:W,H` · `wait:MS` · `move:X,Y` · `click:X,Y` · `down:X,Y` · `up:X,Y` ·
//! `drag:X1,Y1,X2,Y2` · `grab:X1,Y1,X2,Y2` (no release) · `key:esc` / `key:ctrl+f` / `key:enter` · `text:abc` ·
//! `shot:NAME.png` · `mini` (tray click) · `close` (window X) · `quit`. Coordinates are logical pixels of the window content.
//! Screenshots come from `Window::take_snapshot()` — they work without a visible
//! desktop (locked screen, remote session), unlike grabbing the screen.
//! PNGs land in `MAXTRIX_SHOTS` (default: the working directory).

use std::path::PathBuf;
use std::time::Duration;

use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, SharedString};

use crate::MainWindow;

pub fn run(ui: slint::Weak<MainWindow>, script: String) {
    let out = std::env::var("MAXTRIX_SHOTS").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."));
    let steps: Vec<String> = script.split(';').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    next(ui, steps, 0, out);
}

fn xy(arg: &str) -> Option<(f32, f32)> {
    let (x, y) = arg.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

fn send(ui: &MainWindow, ev: WindowEvent) {
    ui.window().dispatch_event(ev);
}

fn pointer(ui: &MainWindow, x: f32, y: f32) -> LogicalPosition {
    let p = LogicalPosition::new(x, y);
    send(ui, WindowEvent::PointerMoved { position: p });
    p
}

fn key(ui: &MainWindow, text: SharedString) {
    send(ui, WindowEvent::KeyPressed { text: text.clone() });
    send(ui, WindowEvent::KeyReleased { text });
}

/// Save what a window currently shows as PNG (also used for the mini window).
pub fn save_snapshot(window: &slint::Window, path: &std::path::Path) {
    match window.take_snapshot() {
        Ok(buf) => {
            let saved = std::fs::File::create(path).ok().and_then(|f| {
                let mut enc = png::Encoder::new(std::io::BufWriter::new(f), buf.width(), buf.height());
                enc.set_color(png::ColorType::Rgba);
                enc.set_depth(png::BitDepth::Eight);
                let mut w = enc.write_header().ok()?;
                w.write_image_data(buf.as_bytes()).ok()
            });
            eprintln!("[testhook] {} {}", if saved.is_some() { "shot" } else { "shot FAILED" }, path.display());
        }
        Err(e) => eprintln!("[testhook] snapshot failed: {e}"),
    }
}

fn next(ui: slint::Weak<MainWindow>, steps: Vec<String>, i: usize, out: PathBuf) {
    let Some(step) = steps.get(i).cloned() else { return };
    let Some(win) = ui.upgrade() else { return };
    let (kind, arg) = step.split_once(':').unwrap_or((step.as_str(), ""));
    let mut delay = 120u64;
    match kind {
        "size" => {
            if let Some((w, h)) = xy(arg) {
                win.window().set_size(slint::LogicalSize::new(w, h));
            }
        }
        "wait" => delay = arg.parse().unwrap_or(500),
        "move" => {
            if let Some((x, y)) = xy(arg) {
                pointer(&win, x, y);
            }
        }
        "click" | "down" | "up" => {
            if let Some((x, y)) = xy(arg) {
                let position = pointer(&win, x, y);
                if kind != "up" {
                    send(&win, WindowEvent::PointerPressed { position, button: PointerEventButton::Left });
                }
                if kind != "down" {
                    send(&win, WindowEvent::PointerReleased { position, button: PointerEventButton::Left });
                }
            }
        }
        "grab" => {
            // Press + move, but keep holding (screenshot mid-drag, then `up:X,Y`).
            let v: Vec<f32> = arg.split(',').filter_map(|s| s.trim().parse().ok()).collect();
            if let [x1, y1, x2, y2] = v[..] {
                let position = pointer(&win, x1, y1);
                send(&win, WindowEvent::PointerPressed { position, button: PointerEventButton::Left });
                for k in 1..=24 {
                    let t = k as f32 / 24.0;
                    pointer(&win, x1 + (x2 - x1) * t, y1 + (y2 - y1) * t);
                }
            }
        }
        "drag" => {
            // Press, move in small steps (so "moved" handlers fire), release.
            let v: Vec<f32> = arg.split(',').filter_map(|s| s.trim().parse().ok()).collect();
            if let [x1, y1, x2, y2] = v[..] {
                let position = pointer(&win, x1, y1);
                send(&win, WindowEvent::PointerPressed { position, button: PointerEventButton::Left });
                for k in 1..=24 {
                    let t = k as f32 / 24.0;
                    pointer(&win, x1 + (x2 - x1) * t, y1 + (y2 - y1) * t);
                }
                // Hold over the target for a moment before releasing: see `hold:`.
                if steps.get(i + 1).is_some_and(|s| s.starts_with("hold")) {
                    delay = 60;
                } else {
                    let position = LogicalPosition::new(x2, y2);
                    send(&win, WindowEvent::PointerReleased { position, button: PointerEventButton::Left });
                }
            }
        }
        "hold" => {
            // After `drag`: screenshot-able mid-drag state, released at the given point.
            if let Some((x, y)) = xy(arg) {
                let position = LogicalPosition::new(x, y);
                send(&win, WindowEvent::PointerReleased { position, button: PointerEventButton::Left });
            }
        }
        "key" => {
            let ctrl = arg.starts_with("ctrl+");
            let name = arg.trim_start_matches("ctrl+");
            let text: SharedString = match name {
                "esc" => Key::Escape.into(),
                "enter" => Key::Return.into(),
                "tab" => Key::Tab.into(),
                other => other.into(),
            };
            if ctrl {
                send(&win, WindowEvent::KeyPressed { text: Key::Control.into() });
            }
            key(&win, text);
            if ctrl {
                send(&win, WindowEvent::KeyReleased { text: Key::Control.into() });
            }
        }
        "text" => {
            for c in arg.chars() {
                key(&win, c.to_string().into());
            }
        }
        "shot" => save_snapshot(win.window(), &out.join(arg)),
        // Same call as a left click on the tray icon.
        "mini" => crate::toggle_mini_window(),
        // The window's X (goes through the app's close handling).
        "close" => send(&win, WindowEvent::CloseRequested),
        "quit" => {
            let _ = slint::quit_event_loop();
            return;
        }
        other => eprintln!("[testhook] unknown step '{other}'"),
    }
    slint::Timer::single_shot(Duration::from_millis(delay), move || next(ui, steps, i + 1, out));
}
