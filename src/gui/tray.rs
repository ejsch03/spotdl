//! The tray icon. Closing the window hides it there while downloads carry on; clicking
//! the icon brings the window back, and its menu has Quit.
//!
//! On Linux it's a StatusNotifierItem, which KDE shows, and GNOME with the AppIndicator
//! extension. On Windows it's in the notification area. Elsewhere (macOS for now), or
//! when there's no tray, closing the window quits as usual.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "linux")]
use slint::ComponentHandle;

use crate::AppWindow;

pub struct Tray {
    /// Whether the icon is in the tray right now, so the window can hide there.
    showing: Arc<AtomicBool>,
    /// Status text for the icon's tooltip (the icon lives on its own thread on Linux).
    #[cfg(target_os = "linux")]
    status: std::sync::mpsc::Sender<String>,
    /// The icon itself. It belongs to the UI thread, whose event loop runs its window.
    #[cfg(windows)]
    icon: RefCell<Option<tray_icon::TrayIcon>>,
    last_status: RefCell<String>,
}

impl Tray {
    /// Puts the icon in the tray, in the background: the window can't hide there until
    /// it's showing.
    #[cfg(target_os = "linux")]
    pub fn start(window: slint::Weak<AppWindow>) -> Self {
        use ksni::blocking::TrayMethods;

        let showing = Arc::new(AtomicBool::new(false));
        let (status, updates) = std::sync::mpsc::channel::<String>();
        let icon = Icon {
            window,
            showing: showing.clone(),
            status: String::new(),
        };
        let started = std::thread::Builder::new()
            .name("tray".into())
            .spawn(move || {
                let showing = icon.showing.clone();
                let handle = match icon.spawn() {
                    Ok(handle) => handle,
                    Err(e) => return log::info!("no system tray ({e}); closing the window quits"),
                };
                showing.store(true, Ordering::Release);
                // ends when the app does
                for status in updates {
                    if handle.update(|icon| icon.status = status).is_none() {
                        break;
                    }
                }
            });
        if let Err(e) = started {
            log::warn!("couldn't start the tray icon: {e}");
        }
        Self {
            showing,
            status,
            last_status: RefCell::default(),
        }
    }

    /// Puts the icon in the notification area. Call on the UI thread: Windows sends the
    /// icon's clicks to a window of that thread, which Slint's event loop serves.
    #[cfg(windows)]
    pub fn start(window: slint::Weak<AppWindow>) -> Self {
        let icon = windows::start(window)
            .inspect_err(|e| {
                log::info!("no notification area icon ({e:#}); closing the window quits")
            })
            .ok();
        Self {
            showing: Arc::new(AtomicBool::new(icon.is_some())),
            icon: RefCell::new(icon),
            last_status: RefCell::default(),
        }
    }

    #[cfg(not(any(target_os = "linux", windows)))]
    pub fn start(_window: slint::Weak<AppWindow>) -> Self {
        Self {
            showing: Arc::default(),
            last_status: RefCell::default(),
        }
    }

    /// Takes the icon away for good, as the app quits. (Windows leaves an icon that isn't
    /// removed behind until the mouse passes over it.)
    pub fn remove(&self) {
        self.showing.store(false, Ordering::Release);
        #[cfg(windows)]
        drop(self.icon.borrow_mut().take());
    }

    /// Whether closing the window should hide it in the tray (rather than quit).
    pub fn is_showing(&self) -> bool {
        self.showing.load(Ordering::Acquire)
    }

    /// Shows what's going on in the icon's tooltip.
    pub fn set_status(&self, status: String) {
        let mut last = self.last_status.borrow_mut();
        if *last == status {
            return;
        }
        #[cfg(target_os = "linux")]
        {
            _ = self.status.send(status.clone());
        }
        #[cfg(windows)]
        if let Some(icon) = &*self.icon.borrow()
            && let Err(e) = icon.set_tooltip(Some(tooltip(&status)))
        {
            log::debug!("couldn't update the icon's tooltip: {e}");
        }
        *last = status;
    }
}

#[cfg(windows)]
mod windows {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{
        Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    };

    use crate::AppWindow;

    pub fn start(window: slint::Weak<AppWindow>) -> anyhow::Result<TrayIcon> {
        let show = MenuItem::new("Show spotdl", true, None);
        let quit = MenuItem::new("Quit spotdl", true, None);
        let menu = Menu::new();
        menu.append_items(&[&show, &PredefinedMenuItem::separator(), &quit])?;

        // These handlers run on the UI thread, in the middle of the event loop's dispatch;
        // what they do is queued for the loop rather than done there and then. (Menu
        // events have one handler for the whole program, settable once: that's why Slint
        // is kept from using native menus on Windows, see `main`.)
        let (show_id, quit_id) = (show.id().clone(), quit.id().clone());
        let for_menu = window.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if event.id == show_id {
                show_window(&for_menu);
            } else if event.id == quit_id {
                _ = slint::invoke_from_event_loop(|| _ = slint::quit_event_loop());
            }
        }));
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_window(&window);
            }
        }));

        // the icon built into spotdl-gui.exe, at every size Windows may want; the drawn
        // one if that's missing (say, a build made without it)
        let icon = Icon::from_resource(1, None).or_else(|_| {
            let (width, height, argb) = super::draw(32);
            Icon::from_rgba(super::rgba(&argb), width as u32, height as u32)
        })?;
        Ok(TrayIconBuilder::new()
            .with_icon(icon)
            .with_tooltip("spotdl")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()?)
    }

    fn show_window(window: &slint::Weak<AppWindow>) {
        _ = window.upgrade_in_event_loop(|window| {
            use slint::ComponentHandle;
            if let Err(e) = window.show() {
                log::warn!("couldn't show the window: {e}");
            }
        });
    }
}

#[cfg(target_os = "linux")]
struct Icon {
    window: slint::Weak<AppWindow>,
    showing: Arc<AtomicBool>,
    status: String,
}

#[cfg(target_os = "linux")]
impl Icon {
    fn show_window(&self) {
        _ = self.window.upgrade_in_event_loop(|window| {
            if let Err(e) = window.show() {
                log::warn!("couldn't show the window: {e}");
            }
        });
    }
}

#[cfg(target_os = "linux")]
impl ksni::Tray for Icon {
    fn id(&self) -> String {
        "spotdl".into()
    }

    fn title(&self) -> String {
        "spotdl".into()
    }

    /// The installed icon (see install.sh); the picture below stands in without it.
    fn icon_name(&self) -> String {
        "spotdl".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        static PICTURES: std::sync::LazyLock<Vec<ksni::Icon>> =
            std::sync::LazyLock::new(|| [22, 32, 64].map(picture).into());
        PICTURES.clone()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "spotdl".into(),
            description: self.status.clone(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.show_window();
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem {
                label: "Show spotdl".into(),
                activate: Box::new(|icon: &mut Self| icon.show_window()),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|_| {
                    _ = slint::invoke_from_event_loop(|| _ = slint::quit_event_loop());
                }),
                ..Default::default()
            }
            .into(),
        ]
    }

    fn watcher_online(&self) {
        self.showing.store(true, Ordering::Release);
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        log::info!("the system tray went away ({reason:?}); closing the window quits for now");
        self.showing.store(false, Ordering::Release);
        // a hidden window would have no way back
        self.show_window();
        true // it may come back (e.g. when the panel restarts)
    }
}

/// The tooltip for `status`, within Windows' limit of 127 characters.
#[cfg(any(windows, test))]
fn tooltip(status: &str) -> String {
    const MAX: usize = 120;
    let text = format!("spotdl · {status}");
    if text.encode_utf16().count() <= MAX {
        return text;
    }
    let mut short = String::new();
    let mut units = 0;
    for c in text.chars() {
        // and one for the "…"
        if units + c.len_utf16() >= MAX {
            break;
        }
        units += c.len_utf16();
        short.push(c);
    }
    short + "…"
}

/// ARGB pixels (as [`draw`] makes them) as RGBA.
#[cfg(any(windows, test))]
fn rgba(argb: &[u8]) -> Vec<u8> {
    argb.as_chunks::<4>()
        .0
        .iter()
        .flat_map(|&[a, r, g, b]| [r, g, b, a])
        .collect()
}

/// The app icon's small version (assets/icon/spotdl-small.svg), drawn here for when it
/// isn't installed: five light sound bars pointing down over a base line, on a charcoal
/// rounded square. ARGB, 4×4 samples per pixel for smooth edges.
#[cfg(target_os = "linux")]
fn picture(size: i32) -> ksni::Icon {
    let (width, height, data) = draw(size);
    ksni::Icon {
        width,
        height,
        data,
    }
}

#[cfg(any(target_os = "linux", windows, test))]
fn draw(size: i32) -> (i32, i32, Vec<u8>) {
    const SAMPLES: i32 = 4;
    // the shapes of spotdl-small.svg, in its 512 units
    const BARS: [(f32, f32, f32); 5] = [
        (104.0, 118.0, 214.0),
        (180.0, 92.0, 290.0),
        (256.0, 72.0, 366.0),
        (332.0, 92.0, 290.0),
        (408.0, 118.0, 214.0),
    ];
    let square = |x: f32, y: f32| {
        let (lo, hi, r) = (16.0, 496.0, 104.0);
        let dx = (lo + r - x).max(x - (hi - r)).max(0.0);
        let dy = (lo + r - y).max(y - (hi - r)).max(0.0);
        (lo..=hi).contains(&x) && (lo..=hi).contains(&y) && dx * dx + dy * dy <= r * r
    };
    // a vertical bar from `top` to `bottom` with round ends
    let bar = |x: f32, y: f32, center: f32, top: f32, bottom: f32, radius: f32| {
        let cy = y.clamp(top + radius, bottom - radius);
        (x - center).powi(2) + (y - cy).powi(2) <= radius * radius
    };
    let light = |x: f32, y: f32| {
        BARS.iter().any(|&(c, t, b)| bar(x, y, c, t, b, 30.0))
            // the base line, a bar on its side
            || ((x.clamp(136.0, 376.0) - x).powi(2) + (y - 426.0).powi(2) <= 24.0 * 24.0)
    };
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (mut inside, mut lit) = (0, 0);
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let x = (px as f32 + (sx as f32 + 0.5) / SAMPLES as f32) / size as f32 * 512.0;
                    let y = (py as f32 + (sy as f32 + 0.5) / SAMPLES as f32) / size as f32 * 512.0;
                    if square(x, y) {
                        inside += 1;
                        lit += i32::from(light(x, y));
                    }
                }
            }
            // pure grays; both run top to bottom: tile #4a4a4a → #1c1c1c, bars white → #c8c8c8
            let t = (py as f32 + 0.5) / size as f32;
            let (tile, bars) = (74.0 - 46.0 * t, 255.0 - 55.0 * t);
            let gray = if inside == 0 {
                0
            } else {
                let w = lit as f32 / inside as f32;
                (tile * (1.0 - w) + bars * w).round() as u8
            };
            let alpha = (inside * 255 / (SAMPLES * SAMPLES)) as u8;
            data.extend([alpha, gray, gray, gray]);
        }
    }
    (size, size, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_tooltips_within_windows_limit() {
        assert_eq!(tooltip("Paused"), "spotdl · Paused");
        let long = tooltip(&"Daft Punk — Random Access Memories ".repeat(10));
        assert_eq!(long.encode_utf16().count(), 120);
        assert!(long.ends_with('…'));
        // characters outside the BMP count twice and aren't split
        let emoji = tooltip(&"🎵".repeat(100));
        assert!(emoji.encode_utf16().count() <= 120 && emoji.ends_with('…'));
    }

    #[test]
    fn turns_argb_into_rgba() {
        assert_eq!(
            rgba(&[255, 1, 2, 3, 0, 9, 8, 7]),
            [1, 2, 3, 255, 9, 8, 7, 0]
        );
    }

    #[test]
    fn draws_the_tray_picture() {
        let (w, h, data) = draw(32);
        assert_eq!((w, h, data.len()), (32, 32, 32 * 32 * 4));
        let px = |x: usize, y: usize| <[u8; 4]>::try_from(&data[(y * 32 + x) * 4..][..4]).unwrap();
        assert_eq!(px(0, 0)[0], 0, "corners are transparent");
        let [a, r, g, b] = px(16, 10);
        assert!(
            a == 255 && r > 200 && r == g && b >= g,
            "the middle bar is light"
        );
        let [a, r, g, b] = px(16, 30);
        assert!(
            a == 255 && r < 60 && r == g && b.abs_diff(g) <= 3,
            "the tile is charcoal"
        );
        assert!(px(16, 26)[1] > 150, "the base line is light");
    }
}
