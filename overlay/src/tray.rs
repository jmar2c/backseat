// System tray icon shown while hosting.
//
// Linux  → ksni (D-Bus StatusNotifierItem, no GTK required)
// Windows → tray-icon (Win32 Shell_NotifyIcon on a background thread)
// Other   → no-op stub

// ── Linux ──────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
pub struct HostTray {
    copy_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    exit_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(target_os = "linux")]
impl HostTray {
    pub fn new(room_code: String) -> Self {
        use std::sync::{Arc, atomic::AtomicBool};
        let copy_flag = Arc::new(AtomicBool::new(false));
        let exit_flag = Arc::new(AtomicBool::new(false));
        let tray = LinuxTray {
            room_code,
            copy_flag: Arc::clone(&copy_flag),
            exit_flag: Arc::clone(&exit_flag),
        };
        ksni::TrayService::new(tray).spawn();
        Self { copy_flag, exit_flag }
    }

    pub fn pop_copy_request(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.copy_flag.swap(false, Ordering::Relaxed)
    }

    pub fn pop_exit_request(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.exit_flag.swap(false, Ordering::Relaxed)
    }
}

#[cfg(target_os = "linux")]
struct LinuxTray {
    room_code: String,
    copy_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    exit_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(target_os = "linux")]
impl ksni::Tray for LinuxTray {
    fn icon_name(&self) -> String { "video-display".into() }
    fn title(&self) -> String { "backseat".into() }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        use std::sync::atomic::Ordering;
        vec![
            StandardItem {
                label: self.room_code.clone(),
                enabled: false,
                ..Default::default()
            }.into(),
            StandardItem {
                label: "Copy room code".into(),
                activate: Box::new(|this: &mut Self| {
                    this.copy_flag.store(true, Ordering::Relaxed);
                }),
                ..Default::default()
            }.into(),
            MenuItem::Separator,
            StandardItem {
                label: "Exit".into(),
                activate: Box::new(|this: &mut Self| {
                    this.exit_flag.store(true, Ordering::Relaxed);
                }),
                ..Default::default()
            }.into(),
        ]
    }
}

// ── Windows ────────────────────────────────────────────────────────────────

#[cfg(windows)]
pub struct HostTray {
    copy_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    exit_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _tray: tray_icon::TrayIcon,
}

#[cfg(windows)]
impl HostTray {
    pub fn new(room_code: String) -> Self {
        use std::sync::{Arc, atomic::AtomicBool};
        use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};

        let copy_flag = Arc::new(AtomicBool::new(false));
        let exit_flag = Arc::new(AtomicBool::new(false));

        let menu = Menu::new();
        let label = MenuItem::new(&room_code, false, None);
        let sep   = PredefinedMenuItem::separator();
        let copy  = MenuItem::new("Copy room code", true, None);
        let copy_id = copy.id().clone();
        let sep2  = PredefinedMenuItem::separator();
        let quit  = MenuItem::new("Exit", true, None);
        let quit_id = quit.id().clone();
        menu.append_items(&[&label, &sep, &copy, &sep2, &quit]).ok();

        let icon = make_win_icon();
        let _tray = tray_icon::TrayIconBuilder::new()
            .with_tooltip("backseat — HOSTING")
            .with_icon(icon)
            .with_menu(Box::new(menu))
            .build()
            .expect("tray icon");

        // Pump MenuEvents on a background thread; set flags on match.
        let copy_flag2 = Arc::clone(&copy_flag);
        let exit_flag2 = Arc::clone(&exit_flag);
        std::thread::spawn(move || {
            let rx = tray_icon::menu::MenuEvent::receiver();
            loop {
                if let Ok(ev) = rx.recv() {
                    if ev.id == copy_id {
                        copy_flag2.store(true, std::sync::atomic::Ordering::Relaxed);
                    } else if ev.id == quit_id {
                        exit_flag2.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
        });

        Self { copy_flag, exit_flag, _tray }
    }

    pub fn pop_copy_request(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.copy_flag.swap(false, Ordering::Relaxed)
    }

    pub fn pop_exit_request(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.exit_flag.swap(false, Ordering::Relaxed)
    }
}

#[cfg(windows)]
fn make_win_icon() -> tray_icon::Icon {
    let size = 16u32;
    let mut data = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - size as f32 / 2.0;
            let dy = y as f32 - size as f32 / 2.0;
            if (dx * dx + dy * dy).sqrt() < size as f32 / 2.0 - 0.5 {
                let i = ((y * size + x) * 4) as usize;
                data[i]   = 76;
                data[i+1] = 175;
                data[i+2] = 80;
                data[i+3] = 255;
            }
        }
    }
    tray_icon::Icon::from_rgba(data, size, size).expect("tray icon data")
}

// ── Stub (macOS and everything else) ──────────────────────────────────────

#[cfg(not(any(target_os = "linux", windows)))]
pub struct HostTray;

#[cfg(not(any(target_os = "linux", windows)))]
impl HostTray {
    pub fn new(_room_code: String) -> Self { Self }
    pub fn pop_copy_request(&self) -> bool { false }
    pub fn pop_exit_request(&self) -> bool { false }
}
