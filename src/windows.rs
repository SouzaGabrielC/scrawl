//! Window geometry and cursor log for Screen-mode captures.
//!
//! While a full-screen capture records, a low-rate worker writes down which windows were on
//! screen (front to back) and where the cursor was, so `scrawl autocrop` can work out which
//! window the narration and ink were about. Snapshots are deduped (only stored when the window
//! list changes), cursor samples are deduped by movement, and window titles stay in
//! `scrawl.json`: they never go into `summary.md` or the clipboard.

use std::collections::HashMap;

#[cfg(target_os = "linux")]
use x11rb::errors::ReplyError;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::model::TargetFrame;

/// How often the worker wakes to sample the cursor and check its triggers.
const TICK: Duration = Duration::from_millis(66);
/// Snapshots while the cursor moves are at most this often (about 2 Hz).
const MOVING_INTERVAL: Duration = Duration::from_millis(500);
/// A snapshot even when nothing seems to happen, to catch windows opening without input.
const IDLE_INTERVAL: Duration = Duration::from_millis(2_000);
/// Cursor samples closer than this to the previous one (in capture points) are dropped.
const CURSOR_MIN_MOVE_PT: f64 = 2.0;

/// The on-screen windows at one moment, front to back.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowSnapshot {
    pub t: u64,
    pub windows: Vec<WindowEntry>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowEntry {
    pub id: u32,
    pub app: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    /// `false` for agent apps (menu-bar utilities, overlays such as Cua Driver's full-screen
    /// window) whose windows are rarely what a capture is about.
    #[serde(default = "yes", skip_serializing_if = "Clone::clone")]
    pub regular: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// `[x, y, width, height]` in points from the capture's top-left corner. Windows that hang
    /// off the captured display keep their full size, so `x`/`y` can be negative.
    pub bounds: [i32; 4],
    /// `0` for normal windows, `20` for the Dock, `24` for the menu bar, up to `101` for a
    /// regular app's floating panels and pop-up menus.
    pub layer: i32,
    /// The frontmost app's frontmost normal window.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub frontmost: bool,
}

/// `[t_ms, x, y]` with `x, y` normalized to the capture (`0.0..=1.0`), like clicks.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CursorSample(pub u64, pub f32, pub f32);

/// What the worker collected over one capture.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ActivityLog {
    pub windows: Vec<WindowSnapshot>,
    pub cursor: Vec<CursorSample>,
    /// Calls to the window list and the time they took, for the log line at the end.
    pub snapshot_calls: u32,
    pub snapshot_time: Duration,
    /// When the finish sequence began and how long stopping capture streams took.
    pub stop_started: Option<Instant>,
    pub stop_ms: Option<u64>,
}

impl ActivityLog {
    /// Stores a snapshot unless the window list is identical to the last stored one.
    pub fn push_windows(&mut self, t: u64, windows: Vec<WindowEntry>) -> bool {
        if self
            .windows
            .last()
            .is_some_and(|last| last.windows == windows)
        {
            return false;
        }
        self.windows.push(WindowSnapshot { t, windows });
        true
    }

    /// Stores a cursor sample (capture points) unless it is within `CURSOR_MIN_MOVE_PT` of the
    /// last one or outside the capture.
    pub fn push_cursor(&mut self, t: u64, x_pt: f64, y_pt: f64, frame: TargetFrame) -> bool {
        let (w, h) = (frame.width.max(1.0), frame.height.max(1.0));
        if !(0.0..=w).contains(&x_pt) || !(0.0..=h).contains(&y_pt) {
            return false;
        }
        let nx = round4(x_pt / w);
        let ny = round4(y_pt / h);
        if let Some(CursorSample(_, px, py)) = self.cursor.last() {
            let dx = (nx - px) as f64 * w;
            let dy = (ny - py) as f64 * h;
            if dx.hypot(dy) < CURSOR_MIN_MOVE_PT {
                return false;
            }
        }
        self.cursor.push(CursorSample(t, nx, ny));
        true
    }
}

fn yes() -> bool {
    true
}

fn round4(v: f64) -> f32 {
    ((v * 10_000.0).round() / 10_000.0) as f32
}

/// One window as the window server reported it, in global points.
#[derive(Clone, Debug, PartialEq)]
pub struct RawWindow {
    pub id: u32,
    pub pid: i32,
    pub owner: String,
    pub title: String,
    pub bounds: [f64; 4],
    pub layer: i32,
    pub alpha: f64,
}

/// What the log needs to know about a window's app.
#[derive(Clone, Debug, PartialEq)]
pub struct AppMeta {
    pub bundle_id: Option<String>,
    /// A Dock app (`NSApplicationActivationPolicyRegular`), not an agent or overlay.
    pub regular: bool,
}

/// Keeps normal windows (plus regular apps' panels and menus), the menu bar and the Dock that
/// overlap the capture, converts them to capture points and marks the frontmost app's first
/// normal window.
///
/// The Dock's own window covers the whole display, so it is replaced by `dock` (the strip the
/// Dock reserves, in global points) or dropped when the Dock hides.
pub fn filter_windows(
    raw: &[RawWindow],
    frame: TargetFrame,
    own_pid: i32,
    frontmost_pid: Option<i32>,
    dock: Option<[f64; 4]>,
    app_meta: &mut dyn FnMut(i32) -> AppMeta,
) -> Vec<WindowEntry> {
    let mut front_marked = false;
    let mut out = Vec::new();
    for w in raw {
        let is_dock = w.layer == 20 && w.owner == "Dock";
        let bounds = match (is_dock, dock) {
            (true, Some(rect)) => rect,
            (true, None) => continue,
            (false, _) => w.bounds,
        };
        let [x, y, width, height] = bounds;
        let menubar = w.layer == 24 && w.title == "Menubar";
        let overlaps = x < frame.x + frame.width
            && x + width > frame.x
            && y < frame.y + frame.height
            && y + height > frame.y;
        if w.pid == own_pid || w.alpha <= 0.01 || width < 24.0 || height < 16.0 || !overlaps {
            continue;
        }
        let meta = app_meta(w.pid);
        // Floating panels, sheets and pop-up menus (layers up to 101) count when a regular
        // app owns them; other high layers are system chrome, notifications and overlays.
        let keep =
            w.layer == 0 || menubar || is_dock || (meta.regular && (1..=101).contains(&w.layer));
        if !keep {
            continue;
        }
        let frontmost = !front_marked && w.layer == 0 && Some(w.pid) == frontmost_pid;
        front_marked |= frontmost;
        out.push(WindowEntry {
            id: w.id,
            app: w.owner.clone(),
            bundle_id: meta.bundle_id,
            regular: meta.regular,
            title: w.title.clone(),
            bounds: [
                (x - frame.x).round() as i32,
                (y - frame.y).round() as i32,
                width.round() as i32,
                height.round() as i32,
            ],
            layer: w.layer,
            frontmost,
        });
    }
    out
}

/// Whether the topmost window under a global point (in `raw`, front to back) belongs to `own_pid`,
/// looking through `passthrough` windows, which let clicks fall to whatever is below them.
pub fn owned_window_at(
    raw: &[RawWindow],
    x: f64,
    y: f64,
    own_pid: i32,
    passthrough: &[u32],
) -> bool {
    raw.iter()
        .filter(|w| {
            let [wx, wy, ww, wh] = w.bounds;
            w.alpha > 0.01 && x >= wx && x <= wx + ww && y >= wy && y <= wy + wh
        })
        .find(|w| !(w.pid == own_pid && passthrough.contains(&w.id)))
        .is_some_and(|w| w.pid == own_pid)
}

/// The active application in front of the desktop.
#[derive(Clone, Debug, PartialEq)]
pub struct FrontmostApp {
    pub name: String,
    pub bundle_id: Option<String>,
    pub pid: i32,
}

/// Insets `geom` by GTK client-side decoration shadows (`_GTK_FRAME_EXTENTS`),
/// or expands `geom` by server-side window manager decorations (`_NET_WM_EXTENTS`).
pub fn apply_frame_extents(
    geom: [f64; 4],
    net_extents: Option<[i32; 4]>,
    gtk_extents: Option<[i32; 4]>,
) -> [f64; 4] {
    if let Some([l, r, t, b]) = gtk_extents {
        let (l, r, t, b) = (l as f64, r as f64, t as f64, b as f64);
        let w = (geom[2] - (l + r)).max(1.0);
        let h = (geom[3] - (t + b)).max(1.0);
        [geom[0] + l, geom[1] + t, w, h]
    } else if let Some([l, r, t, b]) = net_extents {
        let (l, r, t, b) = (l as f64, r as f64, t as f64, b as f64);
        let w = (geom[2] + (l + r)).max(1.0);
        let h = (geom[3] + (t + b)).max(1.0);
        [geom[0] - l, geom[1] - t, w, h]
    } else {
        geom
    }
}

#[cfg(target_os = "macos")]
fn frontmost_external_pid() -> Option<i32> {
    crate::capture::frontmost_external_pid()
}

#[cfg(target_os = "linux")]
fn frontmost_external_pid() -> Option<i32> {
    frontmost_application().map(|a| a.pid)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn frontmost_external_pid() -> Option<i32> {
    None
}

/// The frontmost active application.
#[cfg(target_os = "macos")]
pub fn frontmost_application() -> Option<FrontmostApp> {
    crate::capture::frontmost_app_info().map(|(pid, name, bundle_id)| FrontmostApp {
        name,
        bundle_id,
        pid,
    })
}

/// The frontmost active application from X11 `_NET_ACTIVE_WINDOW`.
#[cfg(target_os = "linux")]
pub fn frontmost_application() -> Option<FrontmostApp> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};

    let (conn, screen_num) = x11rb::connect(None).ok()?;
    let root = conn.setup().roots[screen_num].root;

    let atom_active = conn
        .intern_atom(false, b"_NET_ACTIVE_WINDOW")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let prop = conn
        .get_property(false, root, atom_active, AtomEnum::WINDOW, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    let win = prop.value32()?.next()?;
    if win == 0 {
        return None;
    }

    // Read PID
    let atom_pid = conn
        .intern_atom(false, b"_NET_WM_PID")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let pid_prop = conn
        .get_property(false, win, atom_pid, AtomEnum::CARDINAL, 0, 1)
        .ok()
        .and_then(|c| c.reply().ok());
    let pid = pid_prop
        .and_then(|p| p.value32().map(|it| it.collect::<Vec<u32>>().into_iter()))
        .and_then(|mut it| it.next())
        .unwrap_or(0) as i32;

    // Read WM_CLASS
    let class_prop = conn
        .get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 1024)
        .ok()
        .and_then(|c| c.reply().ok());
    let (res_name, res_class) = class_prop
        .map(|p| {
            let bytes = p.value;
            let parts: Vec<&str> = std::str::from_utf8(&bytes)
                .unwrap_or("")
                .split('\0')
                .filter(|s| !s.is_empty())
                .collect();
            let name = parts.first().copied().unwrap_or("").to_string();
            let class = parts.get(1).copied().unwrap_or(&name).to_string();
            (name, class)
        })
        .unwrap_or_default();

    // Read _NET_WM_NAME or WM_NAME
    let atom_utf8 = conn
        .intern_atom(false, b"UTF8_STRING")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let atom_net_wm_name = conn
        .intern_atom(false, b"_NET_WM_NAME")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let title = conn
        .get_property(false, win, atom_net_wm_name, atom_utf8, 0, 1024)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|p| String::from_utf8_lossy(&p.value).to_string())
        .filter(|t| !t.is_empty())
        .or_else(|| {
            conn.get_property(false, win, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 1024)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|p| String::from_utf8_lossy(&p.value).to_string())
                .filter(|t| !t.is_empty())
        })
        .unwrap_or_default();

    let name = if !res_class.is_empty() {
        res_class.clone()
    } else if pid > 0 {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|s| s.trim().to_string())
            .unwrap_or(title)
    } else {
        title
    };

    let bundle_id = if !res_class.is_empty() {
        Some(res_class.to_lowercase())
    } else if !res_name.is_empty() {
        Some(res_name.to_lowercase())
    } else if pid > 0 {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|s| s.trim().to_lowercase())
    } else {
        None
    };

    Some(FrontmostApp {
        name,
        bundle_id,
        pid,
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn frontmost_application() -> Option<FrontmostApp> {
    None
}

/// Activates an application window by PID or bundle ID.
#[cfg(target_os = "macos")]
pub fn activate_app(pid: i32, _bundle_id: Option<&str>) -> bool {
    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
    if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) {
        app.activateWithOptions(NSApplicationActivationOptions::empty())
    } else {
        false
    }
}

/// Activates an application window by sending `_NET_ACTIVE_WINDOW` client message to the root window.
#[cfg(target_os = "linux")]
pub fn activate_app(pid: i32, bundle_id: Option<&str>) -> bool {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{self, AtomEnum, ConnectionExt};

    let Ok((conn, screen_num)) = x11rb::connect(None) else {
        return false;
    };
    let root = conn.setup().roots[screen_num].root;

    let atom_active = conn
        .intern_atom(false, b"_NET_ACTIVE_WINDOW")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);
    let Some(atom_active) = atom_active else {
        return false;
    };

    let atom_stacking = conn
        .intern_atom(false, b"_NET_CLIENT_LIST_STACKING")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);
    let atom_clients = conn
        .intern_atom(false, b"_NET_CLIENT_LIST")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);
    let atom_pid = conn
        .intern_atom(false, b"_NET_WM_PID")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);

    let mut windows: Vec<xproto::Window> = Vec::new();
    if let Some(atom_s) = atom_stacking {
        if let Ok(prop) = conn
            .get_property(false, root, atom_s, AtomEnum::WINDOW, 0, 1024)
            .map_err(ReplyError::from).and_then(|c| c.reply())
        {
            if let Some(w) = prop.value32() {
                windows = w.collect();
            }
        }
    }
    if windows.is_empty() {
        if let Some(atom_c) = atom_clients {
            if let Ok(prop) = conn
                .get_property(false, root, atom_c, AtomEnum::WINDOW, 0, 1024)
                .map_err(ReplyError::from).and_then(|c| c.reply())
            {
                if let Some(w) = prop.value32() {
                    windows = w.collect();
                }
            }
        }
    }

    // Search top-to-bottom
    windows.reverse();

    let mut target_win: Option<xproto::Window> = None;
    for win in windows {
        if pid > 0 && atom_pid.is_some() {
            if let Ok(prop) = conn
                .get_property(false, win, atom_pid.unwrap(), AtomEnum::CARDINAL, 0, 1)
                .map_err(ReplyError::from).and_then(|c| c.reply())
            {
                if let Some(w_pid) = prop.value32().and_then(|mut it| it.next()) {
                    if w_pid as i32 == pid {
                        target_win = Some(win);
                        break;
                    }
                }
            }
        }
        if let Some(target_bundle) = bundle_id {
            if let Ok(prop) = conn
                .get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 1024)
                .map_err(ReplyError::from).and_then(|c| c.reply())
            {
                let s = String::from_utf8_lossy(&prop.value);
                if s.split('\0').any(|c| c.eq_ignore_ascii_case(target_bundle)) {
                    target_win = Some(win);
                    break;
                }
            }
        }
    }

    let Some(win) = target_win else {
        return false;
    };

    let event = xproto::ClientMessageEvent {
        response_type: xproto::CLIENT_MESSAGE_EVENT,
        format: 32,
        sequence: 0,
        window: win,
        type_: atom_active,
        data: xproto::ClientMessageData::from([2u32, 0u32, 0u32, 0u32, 0u32]),
    };

    let _ = conn.send_event(
        false,
        root,
        xproto::EventMask::SUBSTRUCTURE_REDIRECT | xproto::EventMask::SUBSTRUCTURE_NOTIFY,
        event,
    );
    let _ = conn.flush();
    true
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn activate_app(_pid: i32, _bundle_id: Option<&str>) -> bool {
    false
}

/// Reads the on-screen window list (front to back) from the window server.
#[cfg(target_os = "macos")]
pub fn raw_windows() -> Vec<RawWindow> {
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use core_graphics::window::{
        copy_window_info, kCGNullWindowID, kCGWindowListExcludeDesktopElements,
        kCGWindowListOptionOnScreenOnly,
    };

    let Some(list) = copy_window_info(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
        kCGNullWindowID,
    ) else {
        return Vec::new();
    };
    let key = CFString::from_static_string;
    let (k_number, k_pid, k_owner, k_name, k_layer, k_alpha, k_bounds) = (
        key("kCGWindowNumber"),
        key("kCGWindowOwnerPID"),
        key("kCGWindowOwnerName"),
        key("kCGWindowName"),
        key("kCGWindowLayer"),
        key("kCGWindowAlpha"),
        key("kCGWindowBounds"),
    );
    let (k_x, k_y, k_w, k_h) = (key("X"), key("Y"), key("Width"), key("Height"));
    let number = |dict: &CFDictionary<CFString, CFType>, k: &CFString| -> Option<f64> {
        dict.find(k)?.downcast::<CFNumber>()?.to_f64()
    };
    let string = |dict: &CFDictionary<CFString, CFType>, k: &CFString| -> String {
        dict.find(k)
            .and_then(|v| v.downcast::<CFString>())
            .map(|s| s.to_string())
            .unwrap_or_default()
    };
    let mut out = Vec::new();
    for item in list.iter() {
        let ptr = *item as CFDictionaryRef;
        if ptr.is_null() {
            continue;
        }
        let dict: CFDictionary<CFString, CFType> =
            unsafe { CFDictionary::wrap_under_get_rule(ptr) };
        let Some(bounds) = dict
            .find(&k_bounds)
            .and_then(|v| v.downcast::<CFDictionary>())
        else {
            continue;
        };
        let bounds: CFDictionary<CFString, CFType> =
            unsafe { CFDictionary::wrap_under_get_rule(bounds.as_concrete_TypeRef()) };
        out.push(RawWindow {
            id: number(&dict, &k_number).unwrap_or(0.0) as u32,
            pid: number(&dict, &k_pid).unwrap_or(0.0) as i32,
            owner: string(&dict, &k_owner),
            title: string(&dict, &k_name),
            bounds: [
                number(&bounds, &k_x).unwrap_or(0.0),
                number(&bounds, &k_y).unwrap_or(0.0),
                number(&bounds, &k_w).unwrap_or(0.0),
                number(&bounds, &k_h).unwrap_or(0.0),
            ],
            layer: number(&dict, &k_layer).unwrap_or(0.0) as i32,
            alpha: number(&dict, &k_alpha).unwrap_or(1.0),
        });
    }
    out
}

/// Reads the on-screen window list (front to back) from X11 `_NET_CLIENT_LIST_STACKING`.
#[cfg(target_os = "linux")]
pub fn raw_windows() -> Vec<RawWindow> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{self, AtomEnum, ConnectionExt};

    let Ok((conn, screen_num)) = x11rb::connect(None) else {
        return Vec::new();
    };
    let root = conn.setup().roots[screen_num].root;

    let atom_stacking = conn
        .intern_atom(false, b"_NET_CLIENT_LIST_STACKING")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);
    let atom_clients = conn
        .intern_atom(false, b"_NET_CLIENT_LIST")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);
    let atom_state = conn
        .intern_atom(false, b"_NET_WM_STATE")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);
    let atom_hidden = conn
        .intern_atom(false, b"_NET_WM_STATE_HIDDEN")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);
    let atom_net_wm_name = conn
        .intern_atom(false, b"_NET_WM_NAME")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);
    let atom_utf8 = conn
        .intern_atom(false, b"UTF8_STRING")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);
    let atom_pid = conn
        .intern_atom(false, b"_NET_WM_PID")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);
    let atom_wtype = conn
        .intern_atom(false, b"_NET_WM_WINDOW_TYPE")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);
    let atom_dock = conn
        .intern_atom(false, b"_NET_WM_WINDOW_TYPE_DOCK")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);
    let atom_dialog = conn
        .intern_atom(false, b"_NET_WM_WINDOW_TYPE_DIALOG")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);
    let atom_utility = conn
        .intern_atom(false, b"_NET_WM_WINDOW_TYPE_UTILITY")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);
    let atom_toolbar = conn
        .intern_atom(false, b"_NET_WM_WINDOW_TYPE_TOOLBAR")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);
    let atom_popup = conn
        .intern_atom(false, b"_NET_WM_WINDOW_TYPE_POPUP_MENU")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);
    let atom_dropdown = conn
        .intern_atom(false, b"_NET_WM_WINDOW_TYPE_DROPDOWN_MENU")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);
    let atom_net_extents = conn
        .intern_atom(false, b"_NET_WM_EXTENTS")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);
    let atom_gtk_extents = conn
        .intern_atom(false, b"_GTK_FRAME_EXTENTS")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);
    let atom_opacity = conn
        .intern_atom(false, b"_NET_WM_WINDOW_OPACITY")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);

    let mut windows: Vec<xproto::Window> = Vec::new();
    if let Some(atom_s) = atom_stacking {
        if let Ok(prop) = conn
            .get_property(false, root, atom_s, AtomEnum::WINDOW, 0, 1024)
            .map_err(ReplyError::from).and_then(|c| c.reply())
        {
            if let Some(w) = prop.value32() {
                windows = w.collect();
            }
        }
    }
    if windows.is_empty() {
        if let Some(atom_c) = atom_clients {
            if let Ok(prop) = conn
                .get_property(false, root, atom_c, AtomEnum::WINDOW, 0, 1024)
                .map_err(ReplyError::from).and_then(|c| c.reply())
            {
                if let Some(w) = prop.value32() {
                    windows = w.collect();
                }
            }
        }
    }

    // Stacking list is bottom-to-top; reverse to front-to-back
    windows.reverse();

    let mut out = Vec::new();

    for win in windows {
        // Skip unviewable windows
        let Ok(attrs) = conn.get_window_attributes(win).map_err(ReplyError::from).and_then(|c| c.reply()) else {
            continue;
        };
        if attrs.map_state != xproto::MapState::VIEWABLE {
            continue;
        }

        // Check _NET_WM_STATE for _NET_WM_STATE_HIDDEN
        if let (Some(astate), Some(ahidden)) = (atom_state, atom_hidden) {
            if let Ok(prop) = conn
                .get_property(false, win, astate, AtomEnum::ATOM, 0, 64)
                .map_err(ReplyError::from).and_then(|c| c.reply())
            {
                if let Some(states) = prop.value32() {
                    if states.into_iter().any(|s| s == ahidden) {
                        continue;
                    }
                }
            }
        }

        // Geometry in root window coordinates
        let Ok(geom) = conn.get_geometry(win).map_err(ReplyError::from).and_then(|c| c.reply()) else {
            continue;
        };
        let Ok(coords) = conn.translate_coordinates(win, root, 0, 0).map_err(ReplyError::from).and_then(|c| c.reply()) else {
            continue;
        };

        let raw_bounds: [f64; 4] = [
            coords.dst_x as f64,
            coords.dst_y as f64,
            geom.width as f64,
            geom.height as f64,
        ];

        let get_extents = |atom: Option<xproto::Atom>| -> Option<[i32; 4]> {
            let a = atom?;
            let prop = conn.get_property(false, win, a, AtomEnum::CARDINAL, 0, 4).ok()?.reply().ok()?;
            let v: Vec<u32> = prop.value32()?.collect();
            if v.len() >= 4 {
                Some([v[0] as i32, v[1] as i32, v[2] as i32, v[3] as i32])
            } else {
                None
            }
        };

        let gtk_extents = get_extents(atom_gtk_extents);
        let net_extents = get_extents(atom_net_extents);
        let bounds = apply_frame_extents(raw_bounds, net_extents, gtk_extents);

        // PID
        let pid = atom_pid
            .and_then(|ap| {
                conn.get_property(false, win, ap, AtomEnum::CARDINAL, 0, 1)
                    .ok()?
                    .reply()
                    .ok()?
                    .value32()?
                    .next()
            })
            .unwrap_or(0) as i32;

        // Class & Owner
        let mut owner = String::new();
        if let Ok(prop) = conn
            .get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 1024)
            .map_err(ReplyError::from).and_then(|c| c.reply())
        {
            let bytes = prop.value;
            let parts: Vec<&str> = std::str::from_utf8(&bytes)
                .unwrap_or("")
                .split('\0')
                .filter(|s| !s.is_empty())
                .collect();
            if let Some(class) = parts.get(1).or_else(|| parts.first()) {
                owner = (*class).to_string();
            }
        }
        if owner.is_empty() && pid > 0 {
            owner = std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .ok()
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
        }

        // Title
        let mut title = String::new();
        if let (Some(aname), Some(autf8)) = (atom_net_wm_name, atom_utf8) {
            if let Ok(prop) = conn
                .get_property(false, win, aname, autf8, 0, 1024)
                .map_err(ReplyError::from).and_then(|c| c.reply())
            {
                if !prop.value.is_empty() {
                    title = String::from_utf8_lossy(&prop.value).to_string();
                }
            }
        }
        if title.is_empty() {
            if let Ok(prop) = conn
                .get_property(false, win, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 1024)
                .map_err(ReplyError::from).and_then(|c| c.reply())
            {
                if !prop.value.is_empty() {
                    title = String::from_utf8_lossy(&prop.value).to_string();
                }
            }
        }

        // Layer
        let mut layer = 0i32;
        if let Some(awtype) = atom_wtype {
            if let Ok(prop) = conn
                .get_property(false, win, awtype, AtomEnum::ATOM, 0, 16)
                .map_err(ReplyError::from).and_then(|c| c.reply())
            {
                if let Some(types) = prop.value32() {
                    let ts: Vec<u32> = types.collect();
                    if atom_dock.is_some_and(|d| ts.contains(&d)) {
                        layer = 20;
                    } else if ts.iter().any(|t| {
                        atom_dialog.is_some_and(|d| *t == d)
                            || atom_utility.is_some_and(|u| *t == u)
                            || atom_toolbar.is_some_and(|tb| *t == tb)
                            || atom_popup.is_some_and(|p| *t == p)
                            || atom_dropdown.is_some_and(|dd| *t == dd)
                    }) {
                        layer = 101;
                    }
                }
            }
        }

        // Alpha / Opacity
        let alpha = atom_opacity
            .and_then(|ao| {
                conn.get_property(false, win, ao, AtomEnum::CARDINAL, 0, 1)
                    .ok()?
                    .reply()
                    .ok()?
                    .value32()?
                    .next()
            })
            .map(|op| (op as f64) / 4294967295.0)
            .unwrap_or(1.0);

        out.push(RawWindow {
            id: win,
            pid,
            owner,
            title,
            bounds,
            layer,
            alpha,
        });
    }

    out
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn raw_windows() -> Vec<RawWindow> {
    Vec::new()
}

#[cfg(target_os = "macos")]
fn app_meta_of(pid: i32) -> AppMeta {
    use objc2_app_kit::{NSApplicationActivationPolicy, NSRunningApplication};
    match NSRunningApplication::runningApplicationWithProcessIdentifier(pid) {
        Some(app) => AppMeta {
            bundle_id: app.bundleIdentifier().map(|b| b.to_string()),
            regular: app.activationPolicy() == NSApplicationActivationPolicy::Regular,
        },
        None => AppMeta {
            bundle_id: None,
            regular: false,
        },
    }
}

#[cfg(target_os = "linux")]
fn app_meta_of(pid: i32) -> AppMeta {
    if pid <= 0 {
        return AppMeta {
            bundle_id: None,
            regular: false,
        };
    }
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string());
    AppMeta {
        bundle_id: comm.as_ref().map(|s| s.to_lowercase()),
        regular: true,
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn app_meta_of(_pid: i32) -> AppMeta {
    AppMeta {
        bundle_id: None,
        regular: false,
    }
}

/// The strip the Dock reserves on the display at `frame` (global top-left points), from the
/// difference between the screen's frame and visible frame. `None` when the Dock hides or off
/// the main thread (`NSScreen` is main-thread only).
#[cfg(target_os = "macos")]
pub fn dock_rect(frame: TargetFrame) -> Option<[f64; 4]> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSScreen;
    let mtm = MainThreadMarker::new()?;
    let screens = NSScreen::screens(mtm);
    // AppKit's origin is the primary display's bottom-left corner, y up.
    let primary_h = screens.firstObject()?.frame().size.height;
    let flip = |r: objc2_foundation::NSRect| {
        [
            r.origin.x,
            primary_h - r.origin.y - r.size.height,
            r.size.width,
            r.size.height,
        ]
    };
    let screen = screens.iter().find(|s| {
        let [x, y, w, h] = flip(s.frame());
        (x - frame.x).abs() < 1.0
            && (y - frame.y).abs() < 1.0
            && (w - frame.width).abs() < 1.0
            && (h - frame.height).abs() < 1.0
    })?;
    let [fx, fy, fw, fh] = flip(screen.frame());
    let [vx, vy, vw, vh] = flip(screen.visibleFrame());
    let (left, right, bottom) = (vx - fx, (fx + fw) - (vx + vw), (fy + fh) - (vy + vh));
    // A hidden Dock still leaves a few points at the edge.
    let min = 8.0;
    if bottom >= min {
        Some([fx, vy + vh, fw, bottom])
    } else if left >= min {
        Some([fx, vy, left, vh])
    } else if right >= min {
        Some([vx + vw, vy, right, vh])
    } else {
        None
    }
}

/// The strip the panel / dock reserves on the display at `frame` on Linux.
#[cfg(target_os = "linux")]
pub fn dock_rect(frame: TargetFrame) -> Option<[f64; 4]> {
    let screens = crate::screens::current();
    let screen = screens.iter().find(|s| {
        (s.frame[0] - frame.x).abs() < 1.0
            && (s.frame[1] - frame.y).abs() < 1.0
            && (s.frame[2] - frame.width).abs() < 1.0
            && (s.frame[3] - frame.height).abs() < 1.0
    })?;
    let [fx, fy, fw, fh] = screen.frame;
    let [vx, vy, vw, vh] = screen.visible;
    let (left, right, top, bottom) = (vx - fx, (fx + fw) - (vx + vw), vy - fy, (fy + fh) - (vy + vh));
    let min = 8.0;
    if bottom >= min {
        Some([fx, vy + vh, fw, bottom])
    } else if top >= min {
        Some([fx, fy, fw, top])
    } else if left >= min {
        Some([fx, vy, left, vh])
    } else if right >= min {
        Some([vx + vw, vy, right, vh])
    } else {
        None
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn dock_rect(_frame: TargetFrame) -> Option<[f64; 4]> {
    None
}

/// One timed snapshot of the windows overlapping `frame`.
#[cfg(target_os = "macos")]
pub fn snapshot(
    frame: TargetFrame,
    dock: Option<[f64; 4]>,
    cache: &mut HashMap<i32, AppMeta>,
) -> Vec<WindowEntry> {
    objc2::rc::autoreleasepool(|_| {
        let raw = raw_windows();
        let front = frontmost_external_pid();
        filter_windows(
            &raw,
            frame,
            std::process::id() as i32,
            front,
            dock,
            &mut |pid| cache.entry(pid).or_insert_with(|| app_meta_of(pid)).clone(),
        )
    })
}

/// One timed snapshot of the windows overlapping `frame` on Linux.
#[cfg(target_os = "linux")]
pub fn snapshot(
    frame: TargetFrame,
    dock: Option<[f64; 4]>,
    cache: &mut HashMap<i32, AppMeta>,
) -> Vec<WindowEntry> {
    let raw = raw_windows();
    let front = frontmost_external_pid();
    filter_windows(
        &raw,
        frame,
        std::process::id() as i32,
        front,
        dock,
        &mut |pid| cache.entry(pid).or_insert_with(|| app_meta_of(pid)).clone(),
    )
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn snapshot(
    frame: TargetFrame,
    dock: Option<[f64; 4]>,
    cache: &mut HashMap<i32, AppMeta>,
) -> Vec<WindowEntry> {
    let raw = raw_windows();
    filter_windows(
        &raw,
        frame,
        std::process::id() as i32,
        None,
        dock,
        &mut |pid| cache.entry(pid).or_insert_with(|| app_meta_of(pid)).clone(),
    )
}
/// Background worker that logs windows and the cursor during a Screen-mode capture.
///
/// Snapshots are taken at the start, right after `request` is raised (click, ink or laser
/// start), when the frontmost app changes, at most every 500 ms while the cursor moves, every
/// 2 s otherwise, and once more at the end. Cursor samples are taken at 15 Hz.
pub struct WindowLogger {
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<ActivityLog>>,
}

impl WindowLogger {
    /// Call on the main thread, so the Dock's strip can be read from `NSScreen`.
    pub fn start(frame: TargetFrame, started_at: Instant, request: Arc<AtomicBool>) -> Self {
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopping);
        let dock = dock_rect(frame);
        let worker = thread::Builder::new()
            .name("scrawl-window-log".into())
            .spawn(move || run(frame, dock, started_at, &request, &stop))
            .ok();
        Self { stopping, worker }
    }

    pub fn stop(mut self) -> ActivityLog {
        self.stopping.store(true, Ordering::Release);
        let log = self
            .worker
            .take()
            .and_then(|w| {
                w.thread().unpark();
                w.join().ok()
            })
            .unwrap_or_default();
        if log.snapshot_calls > 0 {
            crate::capture::scrawl_log(format!(
                "Window log: {} snapshots taken, {} stored, {:.2} ms each; {} cursor samples",
                log.snapshot_calls,
                log.windows.len(),
                log.snapshot_time.as_secs_f64() * 1_000.0 / f64::from(log.snapshot_calls),
                log.cursor.len()
            ));
        }
        log
    }
}

impl Drop for WindowLogger {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(ref w) = self.worker {
            w.thread().unpark();
        }
    }
}

fn run(
    frame: TargetFrame,
    dock: Option<[f64; 4]>,
    started_at: Instant,
    request: &AtomicBool,
    stopping: &AtomicBool,
) -> ActivityLog {
    let mut log = ActivityLog::default();
    let mut cache = HashMap::new();
    let mut last_snapshot: Option<Instant> = None;
    let mut last_front = None;
    let mut cursor_at_snapshot: Option<[f32; 2]> = None;
    let mut take = |log: &mut ActivityLog, t: u64| {
        let began = Instant::now();
        let windows = snapshot(frame, dock, &mut cache);
        log.snapshot_calls += 1;
        log.snapshot_time += began.elapsed();
        log.push_windows(t, windows);
    };
    loop {
        let finishing = stopping.load(Ordering::Acquire);
        let t = started_at.elapsed().as_millis() as u64;
        let cursor = crate::capture::current_mouse_screen();
        if let Some([x, y]) = cursor {
            log.push_cursor(t, f64::from(x) - frame.x, f64::from(y) - frame.y, frame);
        }
        let front = frontmost_external_pid();
        let since = last_snapshot.map(|s| s.elapsed());
        let moved = cursor != cursor_at_snapshot;
        let due = !finishing
            && (since.is_none()
                || request.swap(false, Ordering::AcqRel)
                || front != last_front
                || (moved && since.is_some_and(|s| s >= MOVING_INTERVAL))
                || since.is_some_and(|s| s >= IDLE_INTERVAL));
        if due {
            take(&mut log, t);
            last_snapshot = Some(Instant::now());
            last_front = front;
            cursor_at_snapshot = cursor;
        }
        if finishing {
            return log;
        }
        thread::park_timeout(TICK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_extents_subtract_gtk_csd_shadows_and_add_server_decorations() {
        // Review Focus #3: GTK4 window with 26px shadow on left/right/bottom and 22px on top:
        let geom = [100.0, 50.0, 852.0, 648.0];
        let gtk = Some([26, 26, 22, 26]); // left, right, top, bottom
        assert_eq!(
            apply_frame_extents(geom, None, gtk),
            [126.0, 72.0, 800.0, 600.0]
        );
        // Server-side window manager frame (_NET_WM_EXTENTS) with 32px titlebar:
        let net = Some([0, 0, 32, 0]);
        assert_eq!(
            apply_frame_extents([100.0, 82.0, 800.0, 600.0], net, None),
            [100.0, 50.0, 800.0, 632.0]
        );
    }

    fn frame() -> TargetFrame {
        TargetFrame {
            x: 0.0,
            y: 0.0,
            width: 1000.0,
            height: 500.0,
            scale: 2.0,
        }
    }

    fn entry(id: u32, x: i32) -> WindowEntry {
        WindowEntry {
            id,
            app: format!("App {id}"),
            bundle_id: None,
            regular: true,
            title: String::new(),
            bounds: [x, 0, 400, 300],
            layer: 0,
            frontmost: id == 1,
        }
    }

    #[test]
    fn clicks_on_scrawl_windows_are_found_through_the_ink_overlay() {
        let window = |id, pid, bounds| RawWindow {
            id,
            pid,
            owner: String::new(),
            title: String::new(),
            bounds,
            layer: 0,
            alpha: 1.0,
        };
        let own = 7;
        let raw = [
            window(1, own, [0.0, 0.0, 1728.0, 1117.0]), // the ink overlay, click-through
            window(2, own, [700.0, 6.0, 320.0, 44.0]),  // the recording control
            window(3, 99, [100.0, 30.0, 1200.0, 900.0]), // an app
        ];
        assert!(
            owned_window_at(&raw, 800.0, 20.0, own, &[1]),
            "the stop button"
        );
        assert!(
            !owned_window_at(&raw, 400.0, 500.0, own, &[1]),
            "the app under the overlay"
        );
        assert!(
            !owned_window_at(&raw, 1500.0, 1000.0, own, &[1]),
            "the desktop"
        );
        assert!(
            owned_window_at(&raw, 400.0, 500.0, own, &[]),
            "an overlay that takes clicks"
        );
    }

    #[test]
    fn window_snapshots_are_stored_only_when_the_list_changes() {
        let mut log = ActivityLog::default();
        assert!(log.push_windows(0, vec![entry(1, 0), entry(2, 100)]));
        assert!(!log.push_windows(500, vec![entry(1, 0), entry(2, 100)]));
        // A moved window, a reorder, and a focus change are all new snapshots.
        assert!(log.push_windows(1_000, vec![entry(1, 10), entry(2, 100)]));
        assert!(log.push_windows(1_500, vec![entry(2, 100), entry(1, 10)]));
        let mut unfocused = vec![entry(2, 100), entry(1, 10)];
        unfocused[1].frontmost = false;
        assert!(log.push_windows(2_000, unfocused.clone()));
        assert!(!log.push_windows(2_500, unfocused));
        assert_eq!(
            log.windows.iter().map(|s| s.t).collect::<Vec<_>>(),
            vec![0, 1_000, 1_500, 2_000]
        );
    }

    #[test]
    fn logger_snapshots_at_start_and_right_after_a_request() {
        let request = Arc::new(AtomicBool::new(false));
        let logger = WindowLogger::start(frame(), Instant::now(), Arc::clone(&request));
        thread::sleep(Duration::from_millis(150));
        request.store(true, Ordering::Release);
        thread::sleep(Duration::from_millis(150));
        assert!(!request.load(Ordering::Acquire), "the request was taken");
        let log = logger.stop();
        assert!(log.snapshot_calls >= 2, "{} snapshots", log.snapshot_calls);
        assert!(log.windows.len() <= log.snapshot_calls as usize);
    }

    #[test]
    fn cursor_samples_skip_tiny_moves_and_points_outside_the_capture() {
        let mut log = ActivityLog::default();
        assert!(log.push_cursor(0, 500.0, 250.0, frame()));
        assert!(!log.push_cursor(66, 501.0, 250.5, frame()));
        assert!(log.push_cursor(132, 520.0, 250.0, frame()));
        assert!(!log.push_cursor(198, -5.0, 250.0, frame()));
        assert!(!log.push_cursor(264, 500.0, 900.0, frame()));
        assert_eq!(
            log.cursor,
            vec![CursorSample(0, 0.5, 0.5), CursorSample(132, 0.52, 0.5)]
        );
        assert_eq!(
            serde_json::to_string(&log.cursor[1]).unwrap(),
            "[132,0.52,0.5]"
        );
    }

    #[test]
    fn filtering_keeps_overlapping_windows_menu_bar_and_dock_in_capture_points() {
        let capture = TargetFrame {
            x: 1000.0,
            ..frame()
        };
        let raw = |id, pid, owner: &str, title: &str, bounds, layer| RawWindow {
            id,
            pid,
            owner: owner.into(),
            title: title.into(),
            bounds,
            layer,
            alpha: 1.0,
        };
        let windows = [
            raw(1, 9, "Scrawl", "", [1000.0, 0.0, 300.0, 300.0], 0),
            raw(
                2,
                10,
                "Window Server",
                "Menubar",
                [1000.0, 0.0, 1000.0, 24.0],
                24,
            ),
            raw(3, 11, "Dock", "Dock", [1000.0, 0.0, 1000.0, 500.0], 20),
            raw(4, 12, "Control Center", "", [1800.0, 0.0, 40.0, 24.0], 25),
            raw(5, 13, "Safari", "Docs", [1100.5, 50.0, 600.0, 400.0], 0),
            raw(6, 14, "Ghostty", "zsh", [1200.0, 80.0, 600.0, 400.0], 0),
            raw(7, 13, "Safari", "Other", [1150.0, 60.0, 600.0, 400.0], 0),
            raw(8, 15, "Notes", "", [0.0, 0.0, 600.0, 400.0], 0),
            raw(9, 16, "Tiny", "", [1300.0, 300.0, 10.0, 10.0], 0),
            raw(10, 13, "Safari", "", [1200.0, 30.0, 200.0, 120.0], 101),
            raw(11, 14, "Ghostty", "", [1200.0, 30.0, 200.0, 120.0], 101),
        ];
        let mut lookups = Vec::new();
        let dock = Some([1000.0, 440.0, 1000.0, 60.0]);
        let out = filter_windows(&windows, capture, 9, Some(13), dock, &mut |pid| {
            lookups.push(pid);
            AppMeta {
                bundle_id: Some(format!("pid.{pid}")),
                regular: pid != 12 && pid != 14,
            }
        });
        assert_eq!(
            out.iter().map(|w| w.id).collect::<Vec<_>>(),
            vec![2, 3, 5, 6, 7, 10],
            "Safari's pop-up menu counts, not an agent app's high-layer window"
        );
        let safari = &out[2];
        assert_eq!(safari.bounds, [101, 50, 600, 400]);
        assert_eq!(safari.bundle_id.as_deref(), Some("pid.13"));
        assert!(safari.frontmost);
        assert!(
            !out[4].frontmost,
            "only the first window of the frontmost app"
        );
        assert!(!out[0].frontmost && !out[1].frontmost);
        assert_eq!(
            out[1].bounds,
            [0, 440, 1000, 60],
            "the Dock's strip, not its window"
        );
        assert!(!out[3].regular);
        assert_eq!(lookups, vec![10, 11, 12, 13, 14, 13, 13, 14]);
        // A hidden Dock has no strip, so its full-screen window is dropped.
        let hidden = filter_windows(&windows, capture, 9, Some(13), None, &mut |_| AppMeta {
            bundle_id: None,
            regular: true,
        });
        assert!(hidden.iter().all(|w| w.id != 3));
    }
}
