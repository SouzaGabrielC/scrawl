//! The Settings window. It is SwiftUI in `scrawl-helper settings` and, like onboarding, only
//! draws what this module sends it (`View`) and reports what was clicked or typed (`Action`).
//! Everything is decided and written here: `~/.config/scrawl/config.json` keeps its other keys
//! and its `0600` mode (it holds the ingest token).
//!
//! Panes: General (launch at login, sending dev-site captures, the two shortcuts), Recording
//! (transcriber, video export), Storage (how long recordings are kept), Permissions (the
//! onboarding checklist, to fix things later) and About.
//!
//! Shortcuts live under `"shortcuts"` (`{"record": {"keyCode", "modifiers", "label"}}`, Carbon
//! modifier bits; absent means the default). While the window records one, Scrawl's hotkeys are
//! paused (`capture::pause_shortcuts`) so the current combo reaches the recorder.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::capture::{
    CARBON_CMD_KEY, CARBON_CONTROL_KEY, CARBON_OPTION_KEY, CARBON_SHIFT_KEY, ShortcutConfig,
    ShortcutKind, modifier_symbols,
};
use crate::onboarding::{Grants, Permission, Status};

// MARK: Shortcuts in the config

/// Scrawl's two shortcuts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shortcuts {
    pub record: ShortcutConfig,
    pub paste: ShortcutConfig,
}

impl Default for Shortcuts {
    fn default() -> Self {
        Self {
            record: ShortcutConfig::default_record(),
            paste: ShortcutConfig::default_paste(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stored {
    key_code: u32,
    modifiers: u32,
    label: String,
}

impl Shortcuts {
    pub fn get(&self, kind: ShortcutKind) -> &ShortcutConfig {
        match kind {
            ShortcutKind::Record => &self.record,
            ShortcutKind::Paste => &self.paste,
        }
    }

    fn set(&mut self, kind: ShortcutKind, cfg: ShortcutConfig) {
        match kind {
            ShortcutKind::Record => self.record = cfg,
            ShortcutKind::Paste => self.paste = cfg,
        }
    }

    /// From `~/.config/scrawl/config.json`, moving a shortcut saved by 0.3.x (its own file
    /// under Application Support) across the first time.
    pub fn load() -> Self {
        let path = crate::handoff::default_config_path();
        let legacy = legacy_path();
        let (shortcuts, migrated) = Self::load_from(&path, &legacy);
        if migrated && save_shortcut_in(&path, ShortcutKind::Record, &shortcuts.record).is_ok() {
            let _ = std::fs::remove_file(&legacy);
        }
        shortcuts
    }

    /// The shortcuts in `path`, else a custom record shortcut from the old file at `legacy`
    /// (returning `true` when that one should be moved across).
    pub fn load_from(path: &Path, legacy: &Path) -> (Self, bool) {
        let config = read_json(path);
        let mut shortcuts = Self::default();
        let stored = config.get("shortcuts");
        for kind in ShortcutKind::ALL {
            if let Some(cfg) = stored
                .and_then(|s| s.get(kind_key(kind)))
                .and_then(|v| serde_json::from_value::<Stored>(v.clone()).ok())
                .and_then(|s| from_stored(kind, s))
            {
                shortcuts.set(kind, cfg);
            }
        }
        if stored.is_some() {
            return (shortcuts, false);
        }
        let old = read_json(legacy);
        let old = (
            old.get("key_code").and_then(serde_json::Value::as_u64),
            old.get("carbon_modifiers")
                .and_then(serde_json::Value::as_u64),
            old.get("label").and_then(serde_json::Value::as_str),
        );
        if let (Some(key_code), Some(modifiers), Some(label)) = old
            && let Some(cfg) = from_stored(
                ShortcutKind::Record,
                Stored {
                    key_code: key_code as u32,
                    modifiers: modifiers as u32,
                    label: label.to_owned(),
                },
            )
            // 0.3.x saved its default too, and ⌃⌥⇧S was an older default.
            && !cfg.is_default_for(ShortcutKind::Record)
            && cfg.label != "⌃⌥⇧S"
        {
            shortcuts.record = cfg;
            return (shortcuts, true);
        }
        (shortcuts, false)
    }
}

fn kind_key(kind: ShortcutKind) -> &'static str {
    match kind {
        ShortcutKind::Record => "record",
        ShortcutKind::Paste => "paste",
    }
}

/// A stored default is read as the default, so it follows the active layout's key.
fn from_stored(kind: ShortcutKind, stored: Stored) -> Option<ShortcutConfig> {
    let cfg = ShortcutConfig {
        key_code: stored.key_code,
        carbon_modifiers: stored.modifiers,
        label: stored.label,
    };
    if cfg.label.is_empty() || cfg.key_label().is_empty() {
        return None;
    }
    Some(if cfg.is_default_for(kind) {
        kind.default_config()
    } else {
        cfg
    })
}

fn legacy_path() -> PathBuf {
    crate::handoff::default_captures_dir()
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
        .join("config.json")
}

fn read_json(path: &Path) -> serde_json::Value {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}))
}

/// Saves one shortcut; a default is removed rather than written.
pub fn save_shortcut(kind: ShortcutKind, cfg: &ShortcutConfig) {
    if let Err(error) = save_shortcut_in(&crate::handoff::default_config_path(), kind, cfg) {
        crate::capture::scrawl_log(format!("Saving the {kind:?} shortcut failed: {error:#}"));
    }
}

pub fn save_shortcut_in(
    path: &Path,
    kind: ShortcutKind,
    cfg: &ShortcutConfig,
) -> anyhow::Result<()> {
    let mut shortcuts = read_json(path)
        .get("shortcuts")
        .cloned()
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    let map = shortcuts.as_object_mut().expect("an object");
    if cfg.is_default_for(kind) {
        map.remove(kind_key(kind));
    } else {
        map.insert(
            kind_key(kind).to_owned(),
            serde_json::to_value(Stored {
                key_code: cfg.key_code,
                modifiers: cfg.carbon_modifiers,
                label: cfg.label.clone(),
            })?,
        );
    }
    crate::handoff::set_config_value_in(path, "shortcuts", shortcuts)
}

/// The label of the paste shortcut, for hints (`⌥⇧V` unless changed).
pub fn paste_label() -> String {
    PASTE_LABEL
        .lock()
        .ok()
        .and_then(|label| label.clone())
        .unwrap_or_else(|| "⌥⇧V".to_owned())
}

static PASTE_LABEL: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub fn note_paste_label(label: &str) {
    if let Ok(mut slot) = PASTE_LABEL.lock() {
        *slot = Some(label.to_owned());
    }
}

// MARK: Recording a shortcut

/// A key combination pressed in the recorder.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pressed {
    pub key_code: u32,
    /// Carbon modifier bits (⌘ 1<<8, ⇧ 1<<9, ⌥ 1<<11, ⌃ 1<<12).
    pub modifiers: u32,
    /// The key as the active layout types it, without modifiers ("r", "5", "/").
    #[serde(default)]
    pub key: String,
    /// macOS has this combination as one of its own shortcuts (Keyboard Shortcuts settings).
    #[serde(default)]
    pub reserved: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rejection {
    /// Only ⇧, or nothing, with a key that isn't a function key.
    NoModifier,
    /// A key Scrawl can't name or that the recorder uses (Esc).
    UnsupportedKey,
    /// macOS or every app already uses it.
    Reserved(String),
    /// Scrawl's other shortcut.
    Taken(ShortcutKind),
    /// `RegisterEventHotKey` refused it.
    Refused,
}

impl Rejection {
    pub fn message(&self) -> String {
        match self {
            Self::NoModifier => "Hold ⌘, ⌥ or ⌃ with the key.".to_owned(),
            Self::UnsupportedKey => "Scrawl can't use that key. Try a letter or number.".to_owned(),
            Self::Reserved(label) if label.starts_with('⌘') && !label.contains(['⌃', '⌥']) => {
                format!("Apps use {label}. Add ⌥ or ⌃.")
            }
            Self::Reserved(label) => format!("macOS uses {label}."),
            Self::Taken(kind) => format!("{} already uses this.", kind.name()),
            Self::Refused => "Another app has this shortcut.".to_owned(),
        }
    }
}

const FUNCTION_KEYS: [(u32, &str); 16] = [
    (122, "F1"),
    (120, "F2"),
    (99, "F3"),
    (118, "F4"),
    (96, "F5"),
    (97, "F6"),
    (98, "F7"),
    (100, "F8"),
    (101, "F9"),
    (109, "F10"),
    (103, "F11"),
    (111, "F12"),
    (105, "F13"),
    (107, "F14"),
    (113, "F15"),
    (106, "F16"),
];

const NAMED_KEYS: [(u32, &str); 8] = [
    (49, "Space"),
    (36, "↩"),
    (48, "⇥"),
    (51, "⌫"),
    (123, "←"),
    (124, "→"),
    (125, "↓"),
    (126, "↑"),
];

const KEY_ESCAPE: u32 = 53;

/// The name a key is shown with: a function key, a named key, or the typed character in capitals.
pub fn key_label(key_code: u32, typed: &str) -> Option<String> {
    if let Some((_, name)) = FUNCTION_KEYS
        .iter()
        .chain(NAMED_KEYS.iter())
        .find(|(code, _)| *code == key_code)
    {
        return Some((*name).to_owned());
    }
    if key_code == KEY_ESCAPE {
        return None;
    }
    let mut chars = typed
        .chars()
        .filter(|c| !c.is_control() && !c.is_whitespace());
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some(ch.to_uppercase().collect())
}

fn is_function_key(key_code: u32) -> bool {
    FUNCTION_KEYS.iter().any(|(code, _)| *code == key_code)
}

/// Combinations macOS or nearly every app keeps for itself, as (Carbon modifiers, key label).
/// Turned-off system shortcuts are allowed; `Pressed::reserved` covers the enabled ones,
/// including any Kit set up himself.
fn reserved(modifiers: u32, key: &str) -> bool {
    const CMD: u32 = CARBON_CMD_KEY;
    const SHIFT: u32 = CARBON_SHIFT_KEY;
    const OPT: u32 = CARBON_OPTION_KEY;
    const CTRL: u32 = CARBON_CONTROL_KEY;
    // ⌘ with one key is an app's menu shortcut somewhere (⌘Q, ⌘W, ⌘R…).
    if modifiers == CMD || modifiers == CMD | SHIFT && ["3", "4", "5", "6", "Q", "/"].contains(&key)
    {
        return true;
    }
    [
        (CTRL | CMD, "Q"),
        (CTRL | CMD, "F"),
        (CTRL | CMD, "Space"),
        (CTRL | CMD, "D"),
        (CTRL, "Space"),
        (CTRL | OPT, "Space"),
        (OPT | CMD, "Space"),
        (CMD | SHIFT, "Space"),
        (OPT | CMD, "D"),
        (OPT | CMD, "H"),
        (OPT | CMD, "M"),
        (OPT | CMD, "W"),
        (CMD | SHIFT, "⇥"),
        (CTRL, "⇥"),
    ]
    .contains(&(modifiers, key))
}

/// Checks a pressed combination for `kind` and names it. `other` is Scrawl's other shortcut.
pub fn validate(
    kind: ShortcutKind,
    pressed: &Pressed,
    other: &ShortcutConfig,
) -> Result<ShortcutConfig, Rejection> {
    let modifiers = pressed.modifiers
        & (CARBON_CMD_KEY | CARBON_CONTROL_KEY | CARBON_OPTION_KEY | CARBON_SHIFT_KEY);
    let key = key_label(pressed.key_code, &pressed.key).ok_or(Rejection::UnsupportedKey)?;
    let strong = modifiers & (CARBON_CMD_KEY | CARBON_CONTROL_KEY | CARBON_OPTION_KEY) != 0;
    if !strong && !is_function_key(pressed.key_code) {
        return Err(Rejection::NoModifier);
    }
    let label = format!("{}{key}", modifier_symbols(modifiers));
    if pressed.reserved || reserved(modifiers, &key) {
        return Err(Rejection::Reserved(label));
    }
    let cfg = ShortcutConfig {
        key_code: pressed.key_code,
        carbon_modifiers: modifiers,
        label,
    };
    let other_kind = match kind {
        ShortcutKind::Record => ShortcutKind::Paste,
        ShortcutKind::Paste => ShortcutKind::Record,
    };
    if cfg.key_code == other.key_code && cfg.carbon_modifiers == other.carbon_modifiers
        || cfg.label == other.label
    {
        return Err(Rejection::Taken(other_kind));
    }
    Ok(if cfg.is_default_for(kind) {
        kind.default_config()
    } else {
        cfg
    })
}

// MARK: Launch at login (SMAppService, macOS 13+)

#[link(name = "ServiceManagement", kind = "framework")]
unsafe extern "C" {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginItem {
    Off,
    On,
    /// Registered, waiting for the user in System Settings › Login Items.
    NeedsApproval,
    /// Not running from Scrawl.app (a development build).
    Unavailable,
}

#[cfg(target_os = "macos")]
fn main_app_service() -> Option<objc2::rc::Retained<objc2::runtime::AnyObject>> {
    use objc2::runtime::{AnyClass, AnyObject};
    let class = AnyClass::get(c"SMAppService")?;
    let exe = std::env::current_exe().ok()?;
    // SMAppService registers the bundle this executable is in; outside one it can't.
    exe.ancestors()
        .find(|p| p.extension().is_some_and(|ext| ext == "app"))?;
    unsafe {
        let service: *mut AnyObject = objc2::msg_send![class, mainAppService];
        objc2::rc::Retained::retain(service)
    }
}

#[cfg(target_os = "macos")]
pub fn login_item() -> LoginItem {
    let Some(service) = main_app_service() else {
        return LoginItem::Unavailable;
    };
    let status: isize = unsafe { objc2::msg_send![&*service, status] };
    match status {
        1 => LoginItem::On,
        2 => LoginItem::NeedsApproval,
        3 => LoginItem::Unavailable,
        _ => LoginItem::Off,
    }
}

#[cfg(target_os = "macos")]
pub fn set_login_item(on: bool) -> anyhow::Result<()> {
    use objc2::runtime::Bool;
    use objc2_foundation::NSError;
    let service =
        main_app_service().ok_or_else(|| anyhow::anyhow!("not running from Scrawl.app"))?;
    let mut error: Option<objc2::rc::Retained<NSError>> = None;
    let ok: Bool = unsafe {
        if on {
            objc2::msg_send![&*service, registerAndReturnError: &mut error]
        } else {
            objc2::msg_send![&*service, unregisterAndReturnError: &mut error]
        }
    };
    if ok.as_bool() {
        Ok(())
    } else {
        anyhow::bail!(
            "{}",
            error.map_or_else(
                || "unknown error".to_owned(),
                |e| e.localizedDescription().to_string()
            )
        )
    }
}

#[cfg(target_os = "macos")]
fn open_login_items_settings() {
    use objc2::runtime::AnyClass;
    if let Some(class) = AnyClass::get(c"SMAppService") {
        let _: () = unsafe { objc2::msg_send![class, openSystemSettingsLoginItems] };
    }
}

#[cfg(target_os = "linux")]
fn autostart_desktop_path() -> Option<PathBuf> {
    let config_dir = dirs::config_dir().or_else(|| {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))
    })?;
    Some(config_dir.join("autostart").join("scrawl.desktop"))
}

#[cfg(target_os = "linux")]
pub fn login_item() -> LoginItem {
    let Some(path) = autostart_desktop_path() else {
        return LoginItem::Unavailable;
    };
    if path.is_file() {
        LoginItem::On
    } else {
        LoginItem::Off
    }
}

#[cfg(target_os = "linux")]
pub fn set_login_item(on: bool) -> anyhow::Result<()> {
    let Some(path) = autostart_desktop_path() else {
        anyhow::bail!("Cannot determine autostart directory");
    };
    if on {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "scrawl".into());
        let content = format!(
            "[Desktop Entry]\nType=Application\nName=Scrawl\nComment=Shortcut-triggered screen, voice, and drawing capture for agents\nExec={exe}\nIcon=scrawl\nTerminal=false\nCategories=Utility;\n"
        );
        std::fs::write(&path, content)?;
    } else if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn open_login_items_settings() {}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn login_item() -> LoginItem {
    LoginItem::Unavailable
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn set_login_item(_on: bool) -> anyhow::Result<()> {
    Ok(())
}

// MARK: What the window shows

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Pane {
    General,
    Recording,
    Storage,
    Permissions,
    About,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub version: String,
    pub login: LoginView,
    pub auto_send: bool,
    pub shortcuts: Vec<ShortcutView>,
    pub transcriber: TranscriberView,
    pub video_export: bool,
    pub retention: RetentionView,
    /// How much the captures folder takes, once measured.
    pub captures_size: Option<String>,
    pub permissions: Vec<PermissionView>,
    pub updates: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginView {
    pub on: bool,
    pub available: bool,
    pub needs_approval: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShortcutView {
    pub kind: ShortcutKind,
    pub title: &'static str,
    pub label: String,
    pub key: String,
    pub shift: bool,
    pub control: bool,
    pub option: bool,
    pub command: bool,
    pub is_default: bool,
    /// The recorder is waiting for a new combination.
    pub recording: bool,
    /// Why the last combination was turned down.
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriberView {
    /// What recordings use now: "apple" or "hex".
    pub engine: &'static str,
    pub hex_title: &'static str,
    pub hex_enabled: bool,
    pub models: Vec<ModelView>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ModelView {
    pub id: String,
    pub title: String,
    pub chosen: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RetentionView {
    /// The chosen preset, or `None` for a policy written by hand.
    pub preset: Option<&'static str>,
    pub options: Vec<OptionView>,
    pub note: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OptionView {
    pub id: &'static str,
    pub label: &'static str,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionView {
    pub permission: Permission,
    pub status: Status,
    pub required: bool,
    pub requested: bool,
    /// Granted, but on only after Scrawl reopens.
    pub relaunch: bool,
}

fn preset_id(preset: crate::retention::Preset) -> &'static str {
    match preset {
        crate::retention::Preset::Week => "week",
        crate::retention::Preset::Month => "month",
        crate::retention::Preset::Forever => "forever",
    }
}

fn retention_note(policy: &crate::retention::Policy) -> String {
    let days = |n: u32| {
        if n == 1 {
            "1 day".to_owned()
        } else {
            format!("{n} days")
        }
    };
    match (policy.media_days, policy.bundle_days) {
        (None, None) => String::new(),
        (Some(media), Some(bundle)) => format!(
            "Video and audio go to the Trash after {}, the rest of each capture after {}.",
            days(media),
            days(bundle)
        ),
        (Some(media), None) => format!("Video and audio go to the Trash after {}.", days(media)),
        (None, Some(bundle)) => format!("Captures go to the Trash after {}.", days(bundle)),
    }
}

fn shortcut_view(
    kind: ShortcutKind,
    cfg: &ShortcutConfig,
    recording: bool,
    error: Option<&String>,
) -> ShortcutView {
    let mods = cfg.carbon_modifiers;
    ShortcutView {
        kind,
        title: kind.name(),
        label: cfg.label.clone(),
        key: cfg.key_label().to_owned(),
        shift: mods & CARBON_SHIFT_KEY != 0,
        control: mods & CARBON_CONTROL_KEY != 0,
        option: mods & CARBON_OPTION_KEY != 0,
        command: mods & CARBON_CMD_KEY != 0,
        is_default: cfg.is_default_for(kind),
        recording,
        error: error.cloned(),
    }
}

// MARK: The window's logic

/// What the window can ask for.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum Action {
    /// A toggle or popup: `launchAtLogin`, `autoSend`, `videoExport` (bool), `transcriber`
    /// ("apple"/"hex"), `hexModel` (an id), `retention` (a preset id).
    Set {
        key: String,
        value: serde_json::Value,
    },
    /// The recorder for `kind` was clicked.
    RecordShortcut {
        kind: ShortcutKind,
    },
    /// A combination was pressed while recording.
    Shortcut {
        kind: ShortcutKind,
        #[serde(flatten)]
        pressed: Pressed,
    },
    /// Esc, a click elsewhere, or the window losing focus.
    CancelShortcut,
    ResetShortcut {
        kind: ShortcutKind,
    },
    /// A permission row's button.
    Grant {
        permission: Permission,
    },
    /// Reopen Scrawl so a Screen Recording grant takes.
    Reopen,
    ShowCaptures,
    CheckForUpdates,
    /// "Show Welcome Tour" on About.
    Tour,
    Close,
}

/// What the app does with an action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    None,
    /// Ask macOS (the system prompt): the first press on a row that was never asked.
    Request(Permission),
    /// Already asked or denied: its System Settings pane.
    OpenSettings(Permission),
    OpenLoginItems,
    ShowCaptures,
    CheckForUpdates,
    /// The captures folder may have changed size.
    Measure,
    Relaunch,
    Tour,
    Close,
}

/// The outside world the window changes: Scrawl's hotkeys and the config file.
pub trait Host {
    fn shortcuts(&self) -> Shortcuts;
    /// Registers both, keeping the old ones if macOS refuses one.
    fn set_shortcuts(&mut self, shortcuts: &Shortcuts) -> Result<(), ShortcutKind>;
    fn pause_shortcuts(&mut self, paused: bool);
    fn config_path(&self) -> PathBuf;
    fn set_login_item(&mut self, on: bool) -> anyhow::Result<()>;
}

/// The window's own state: what it's recording, what failed, what was asked for.
#[derive(Debug, Default)]
pub struct Panel {
    recording: Option<ShortcutKind>,
    error: Option<(ShortcutKind, String)>,
    requested: BTreeSet<Permission>,
    grants: Option<Grants>,
    captures_size: Option<u64>,
}

impl Panel {
    #[cfg(test)]
    pub fn recording(&self) -> Option<ShortcutKind> {
        self.recording
    }

    pub fn set_grants(&mut self, grants: Grants) {
        self.grants = Some(grants);
    }

    pub fn set_captures_size(&mut self, bytes: u64) {
        self.captures_size = Some(bytes);
    }

    pub fn on(&mut self, action: Action, host: &mut dyn Host) -> Effect {
        match action {
            Action::Set { key, value } => self.set(&key, &value, host),
            Action::RecordShortcut { kind } => {
                self.error = None;
                if self.recording.replace(kind).is_none() {
                    host.pause_shortcuts(true);
                }
                Effect::None
            }
            Action::CancelShortcut => {
                self.stop_recording(host);
                Effect::None
            }
            Action::Shortcut { kind, pressed } => {
                if self.recording != Some(kind) {
                    return Effect::None;
                }
                self.stop_recording(host);
                let mut shortcuts = host.shortcuts();
                let other = match kind {
                    ShortcutKind::Record => &shortcuts.paste,
                    ShortcutKind::Paste => &shortcuts.record,
                };
                match validate(kind, &pressed, other) {
                    Ok(cfg) => self.apply(kind, cfg, &mut shortcuts, host),
                    Err(rejection) => self.error = Some((kind, rejection.message())),
                }
                Effect::None
            }
            Action::ResetShortcut { kind } => {
                self.stop_recording(host);
                let mut shortcuts = host.shortcuts();
                self.apply(kind, kind.default_config(), &mut shortcuts, host);
                Effect::None
            }
            Action::Grant { permission } => self.grant(permission),
            Action::Reopen => {
                if self.screen_relaunch() {
                    Effect::Relaunch
                } else {
                    Effect::None
                }
            }
            Action::ShowCaptures => Effect::ShowCaptures,
            Action::CheckForUpdates => Effect::CheckForUpdates,
            Action::Tour => Effect::Tour,
            Action::Close => {
                self.stop_recording(host);
                Effect::Close
            }
        }
    }

    fn stop_recording(&mut self, host: &mut dyn Host) {
        if self.recording.take().is_some() {
            host.pause_shortcuts(false);
        }
    }

    fn apply(
        &mut self,
        kind: ShortcutKind,
        cfg: ShortcutConfig,
        shortcuts: &mut Shortcuts,
        host: &mut dyn Host,
    ) {
        if shortcuts.get(kind) == &cfg {
            self.error = None;
            return;
        }
        shortcuts.set(kind, cfg.clone());
        match host.set_shortcuts(shortcuts) {
            Ok(()) => {
                self.error = None;
                if let Err(error) = save_shortcut_in(&host.config_path(), kind, &cfg) {
                    crate::capture::scrawl_log(format!("Saving the shortcut failed: {error:#}"));
                }
            }
            Err(refused) => self.error = Some((refused, Rejection::Refused.message())),
        }
    }

    fn set(&mut self, key: &str, value: &serde_json::Value, host: &mut dyn Host) -> Effect {
        let path = host.config_path();
        let write = |key: &str, value: serde_json::Value| {
            if let Err(error) = crate::handoff::set_config_value_in(&path, key, value) {
                crate::capture::scrawl_log(format!("Saving {key} failed: {error:#}"));
            }
        };
        match (key, value) {
            ("launchAtLogin", serde_json::Value::Bool(on)) => {
                if let Err(error) = host.set_login_item(*on) {
                    crate::capture::scrawl_log(format!("Launch at login: {error:#}"));
                }
                if *on && login_item() == LoginItem::NeedsApproval {
                    return Effect::OpenLoginItems;
                }
            }
            ("autoSend" | "videoExport", serde_json::Value::Bool(on)) => write(key, (*on).into()),
            ("transcriber", serde_json::Value::String(name)) => {
                if let Some(engine) = crate::transcribe::Engine::parse(name) {
                    write("transcriber", engine.name().into());
                }
            }
            ("hexModel", serde_json::Value::String(model)) if !model.trim().is_empty() => {
                // Picking a Hex model is choosing Hex.
                write("hexModel", model.trim().into());
                write("transcriber", "hex".into());
            }
            ("retention", serde_json::Value::String(id)) => {
                if let Some(preset) = crate::retention::Preset::ALL
                    .into_iter()
                    .find(|p| preset_id(*p) == id)
                {
                    if let Err(error) = preset.policy().save_to(&path) {
                        crate::capture::scrawl_log(format!("Saving retention failed: {error:#}"));
                    }
                    return Effect::Measure;
                }
            }
            _ => crate::capture::scrawl_log(format!("Settings: ignoring {key}={value}")),
        }
        Effect::None
    }

    fn status(&self, permission: Permission) -> Status {
        self.grants
            .as_ref()
            .map_or(Status::NotDetermined, |g| g.status(permission))
    }

    fn screen_relaunch(&self) -> bool {
        self.grants
            .as_ref()
            .is_some_and(|g| g.granted(Permission::ScreenRecording) && g.screen_needs_relaunch)
    }

    /// Like onboarding: the first press asks macOS, later ones (or a denied one) open the
    /// System Settings pane.
    fn grant(&mut self, permission: Permission) -> Effect {
        let status = self.status(permission);
        if status == Status::Granted {
            return Effect::None;
        }
        if self.requested.insert(permission) && status != Status::Denied {
            Effect::Request(permission)
        } else {
            Effect::OpenSettings(permission)
        }
    }

    pub fn view(&self, host: &dyn Host) -> View {
        let config = crate::handoff::load_config_from(&host.config_path());
        let shortcuts = host.shortcuts();
        let hex = crate::hex::cached_status();
        let engine = config
            .transcriber
            .as_deref()
            .and_then(crate::transcribe::Engine::parse)
            .unwrap_or_default();
        let menu = crate::transcribe::menu_state(engine, hex.as_ref(), config.hex_model.as_deref());
        let policy = crate::retention::Policy::load_from(&host.config_path());
        let login = login_item();
        let order = [
            Permission::ScreenRecording,
            Permission::Microphone,
            Permission::Accessibility,
            Permission::Camera,
        ];
        View {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            login: LoginView {
                on: matches!(login, LoginItem::On | LoginItem::NeedsApproval),
                available: login != LoginItem::Unavailable,
                needs_approval: login == LoginItem::NeedsApproval,
            },
            auto_send: config.auto_send.unwrap_or(true),
            shortcuts: ShortcutKind::ALL
                .into_iter()
                .map(|kind| {
                    let error = self
                        .error
                        .as_ref()
                        .filter(|(k, _)| *k == kind)
                        .map(|(_, e)| e);
                    shortcut_view(
                        kind,
                        shortcuts.get(kind),
                        self.recording == Some(kind),
                        error,
                    )
                })
                .collect(),
            transcriber: TranscriberView {
                engine: if menu.hex_checked { "hex" } else { "apple" },
                hex_title: menu.hex_title,
                hex_enabled: menu.hex_enabled,
                models: menu
                    .models
                    .into_iter()
                    .map(|(id, title, chosen)| ModelView { id, title, chosen })
                    .collect(),
            },
            video_export: config.video_export.unwrap_or(false),
            retention: RetentionView {
                preset: policy.preset().map(preset_id),
                options: crate::retention::Preset::ALL
                    .into_iter()
                    .map(|p| OptionView {
                        id: preset_id(p),
                        label: p.label(),
                    })
                    .collect(),
                note: retention_note(&policy),
            },
            captures_size: self.captures_size.map(crate::retention::format_bytes),
            permissions: order
                .into_iter()
                .map(|permission| PermissionView {
                    permission,
                    status: self.status(permission),
                    required: permission.required(),
                    requested: self.requested.contains(&permission),
                    relaunch: permission == Permission::ScreenRecording && self.screen_relaunch(),
                })
                .collect(),
            updates: crate::sparkle::available(),
        }
    }
}

// MARK: The live window

/// The real hotkeys and config file.
pub struct AppHost<'a> {
    pub telemetry: &'a crate::capture::SharedCaptureTelemetry,
}

impl Host for AppHost<'_> {
    fn shortcuts(&self) -> Shortcuts {
        let (record, paste) = self.telemetry.shortcuts();
        Shortcuts { record, paste }
    }

    fn set_shortcuts(&mut self, shortcuts: &Shortcuts) -> Result<(), ShortcutKind> {
        crate::capture::set_shortcuts(
            self.telemetry,
            shortcuts.record.clone(),
            shortcuts.paste.clone(),
        )?;
        note_paste_label(&shortcuts.paste.label);
        Ok(())
    }

    fn pause_shortcuts(&mut self, paused: bool) {
        crate::capture::pause_shortcuts(self.telemetry, paused);
    }

    fn config_path(&self) -> PathBuf {
        crate::handoff::default_config_path()
    }

    fn set_login_item(&mut self, on: bool) -> anyhow::Result<()> {
        set_login_item(on)
    }
}

/// From the window's helper process and its pollers.
#[derive(Clone, Debug)]
pub enum Event {
    Action(Action),
    Grants(Grants),
    CapturesSize(u64),
    /// Something outside the window may have changed (Hex, the login item): redraw.
    Tick,
    Exited,
}

/// The open Settings window.
pub struct Session {
    panel: Panel,
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    polling: std::sync::Arc<std::sync::atomic::AtomicBool>,
    events: std::sync::Arc<dyn Fn(Event) + Send + Sync>,
    last_view: String,
}

impl Session {
    pub fn open(
        pane: Option<Pane>,
        grants: Option<Grants>,
        events: impl Fn(Event) + Send + Sync + 'static,
    ) -> anyhow::Result<Self> {
        use std::io::BufRead;
        use std::process::{Command, Stdio};
        use std::sync::atomic::{AtomicBool, Ordering};

        let events: std::sync::Arc<dyn Fn(Event) + Send + Sync> = std::sync::Arc::new(events);
        let mut command = Command::new(crate::helper::path()?);
        command.arg("settings");
        if let Some(serde_json::Value::String(pane)) = pane.map(serde_json::to_value).transpose()? {
            command.args(["--pane", &pane]);
        }
        let dev_icon = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/app-icon.png");
        if Path::new(dev_icon).is_file() {
            command.args(["--icon", dev_icon]);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();

        let reader = std::sync::Arc::clone(&events);
        std::thread::spawn(move || {
            if let Some(stdout) = stdout {
                for line in std::io::BufReader::new(stdout)
                    .lines()
                    .map_while(Result::ok)
                {
                    match serde_json::from_str::<Action>(&line) {
                        Ok(action) => reader(Event::Action(action)),
                        Err(error) => {
                            crate::capture::scrawl_log(format!("settings: {error}: {line}"))
                        }
                    }
                }
            }
            reader(Event::Exited);
        });

        let polling = std::sync::Arc::new(AtomicBool::new(true));
        let alive = std::sync::Arc::clone(&polling);
        let poll = std::sync::Arc::clone(&events);
        std::thread::spawn(move || {
            let mut n = 0_u32;
            while alive.load(Ordering::Acquire) {
                // Hex and the size on open (`hex::watch` keeps Hex fresh) and every 30 s; the
                // grants and the login item every second.
                if n % 30 == 0 {
                    crate::hex::refresh_in_background();
                    let size =
                        crate::retention::captures_size(&crate::handoff::default_captures_dir());
                    poll(Event::CapturesSize(size));
                }
                if let Some(grants) = crate::onboarding::read_grants()
                    && alive.load(Ordering::Acquire)
                {
                    poll(Event::Grants(grants));
                } else {
                    poll(Event::Tick);
                }
                n += 1;
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        });

        let mut panel = Panel::default();
        if let Some(grants) = grants {
            panel.set_grants(grants);
        }
        Ok(Self {
            panel,
            child,
            stdin,
            polling,
            events,
            last_view: String::new(),
        })
    }

    /// Feeds an event in, redraws, and returns what the app must do (`Relaunch`, `Tour`,
    /// `Close`); the rest is done here.
    pub fn event(&mut self, event: Event, host: &mut dyn Host) -> Effect {
        let effect = match event {
            Event::Action(action) => {
                crate::capture::scrawl_log(format!("settings: {action:?}"));
                self.panel.on(action, host)
            }
            Event::Grants(grants) => {
                self.panel.set_grants(grants);
                Effect::None
            }
            Event::CapturesSize(bytes) => {
                self.panel.set_captures_size(bytes);
                Effect::None
            }
            Event::Tick => Effect::None,
            Event::Exited => {
                self.panel.on(Action::Close, host);
                return Effect::Close;
            }
        };
        let result = match effect {
            // Attributed to whichever process asks, so Scrawl itself asks.
            Effect::Request(Permission::ScreenRecording) => {
                crate::capture::request_screen_capture();
                Effect::None
            }
            Effect::Request(permission) => {
                #[cfg(target_os = "linux")]
                if permission == Permission::Microphone {
                    std::thread::spawn(|| {
                        let _ = crate::parakeet::ensure_parakeet_model(&mut |_, _| {});
                    });
                }
                self.send(&serde_json::json!({"type": "request", "permission": permission}));
                Effect::None
            }
            Effect::OpenSettings(permission) => {
                #[cfg(target_os = "linux")]
                if permission == Permission::Microphone {
                    std::thread::spawn(|| {
                        let _ = crate::parakeet::ensure_parakeet_model(&mut |_, _| {});
                    });
                }
                self.send(&serde_json::json!({"type": "open-settings", "permission": permission}));
                Effect::None
            }
            Effect::OpenLoginItems => {
                open_login_items_settings();
                Effect::None
            }
            Effect::ShowCaptures => {
                let dir = crate::handoff::default_captures_dir();
                let _ = std::fs::create_dir_all(&dir);
                #[cfg(target_os = "macos")]
                let _ = std::process::Command::new("/usr/bin/open").arg(dir).spawn();
                #[cfg(target_os = "linux")]
                let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
                #[cfg(not(any(target_os = "macos", target_os = "linux")))]
                let _ = std::process::Command::new("open").arg(dir).spawn();
                Effect::None
            }
            Effect::CheckForUpdates => {
                crate::sparkle::check_for_updates();
                Effect::None
            }
            Effect::Measure => {
                crate::retention::nudge();
                let events = std::sync::Arc::clone(&self.events);
                std::thread::spawn(move || {
                    let size =
                        crate::retention::captures_size(&crate::handoff::default_captures_dir());
                    events(Event::CapturesSize(size));
                });
                Effect::None
            }
            other => other,
        };
        self.sync(host);
        result
    }

    pub fn show(&mut self, pane: Option<Pane>) {
        self.send(&serde_json::json!({"type": "show", "pane": pane}));
    }

    pub fn sync(&mut self, host: &dyn Host) {
        let mut view = serde_json::to_value(self.panel.view(host)).unwrap_or_default();
        view["type"] = "state".into();
        let text = view.to_string();
        if text != self.last_view {
            self.send(&view);
            self.last_view = text;
        }
    }

    fn send(&mut self, message: &serde_json::Value) {
        use std::io::Write;
        if let Some(stdin) = self.stdin.as_mut()
            && writeln!(stdin, "{message}")
                .and_then(|()| stdin.flush())
                .is_err()
        {
            self.stdin = None;
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.polling
            .store(false, std::sync::atomic::Ordering::Release);
        self.send(&serde_json::json!({"type": "close"}));
        self.stdin = None;
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: u32 = 15; // ANSI R
    const OPT_SHIFT: u32 = CARBON_OPTION_KEY | CARBON_SHIFT_KEY;

    struct Fake {
        shortcuts: Shortcuts,
        path: PathBuf,
        paused: Vec<bool>,
        refuse: Option<String>,
    }

    impl Host for Fake {
        fn shortcuts(&self) -> Shortcuts {
            self.shortcuts.clone()
        }
        fn set_shortcuts(&mut self, shortcuts: &Shortcuts) -> Result<(), ShortcutKind> {
            for kind in ShortcutKind::ALL {
                if self.refuse.as_deref() == Some(shortcuts.get(kind).label.as_str()) {
                    return Err(kind);
                }
            }
            self.shortcuts = shortcuts.clone();
            Ok(())
        }
        fn pause_shortcuts(&mut self, paused: bool) {
            self.paused.push(paused);
        }
        fn config_path(&self) -> PathBuf {
            self.path.clone()
        }
        fn set_login_item(&mut self, _on: bool) -> anyhow::Result<()> {
            Ok(())
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "scrawl-settings-{name}-{}-{}",
                std::process::id(),
                crate::now_epoch_ms()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn pressed(key_code: u32, modifiers: u32, key: &str) -> Pressed {
        Pressed {
            key_code,
            modifiers,
            key: key.to_owned(),
            reserved: false,
        }
    }

    fn fake(dir: &TempDir) -> Fake {
        Fake {
            shortcuts: Shortcuts::default(),
            path: dir.0.join("config.json"),
            paused: Vec::new(),
            refuse: None,
        }
    }

    #[test]
    fn a_shortcut_needs_a_real_modifier_and_a_free_key() {
        let paste = ShortcutConfig::default_paste();
        let ok = validate(
            ShortcutKind::Record,
            &pressed(R, CARBON_CONTROL_KEY | CARBON_OPTION_KEY, "r"),
            &paste,
        )
        .unwrap();
        assert_eq!(ok.label, "⌃⌥R");
        assert_eq!(ok.key_label(), "R");
        // Labels follow macOS's order whatever order the bits come in.
        let ok = validate(
            ShortcutKind::Record,
            &pressed(
                R,
                CARBON_CMD_KEY | CARBON_SHIFT_KEY | CARBON_CONTROL_KEY | CARBON_OPTION_KEY,
                "r",
            ),
            &paste,
        )
        .unwrap();
        assert_eq!(ok.label, "⌃⌥⇧⌘R");
        let no = |p: Pressed| validate(ShortcutKind::Record, &p, &paste).unwrap_err();
        assert_eq!(no(pressed(R, 0, "r")), Rejection::NoModifier);
        assert_eq!(no(pressed(R, CARBON_SHIFT_KEY, "r")), Rejection::NoModifier);
        // A function key may stand alone.
        assert_eq!(
            validate(ShortcutKind::Record, &pressed(96, 0, ""), &paste)
                .unwrap()
                .label,
            "F5"
        );
        assert_eq!(
            no(pressed(53, CARBON_OPTION_KEY, "\u{1b}")),
            Rejection::UnsupportedKey
        );
        // ⌘ and one key belongs to apps; macOS's own combinations are refused.
        assert_eq!(
            no(pressed(R, CARBON_CMD_KEY, "r")),
            Rejection::Reserved("⌘R".into())
        );
        assert_eq!(
            no(pressed(R, CARBON_CMD_KEY, "r")).message(),
            "Apps use ⌘R. Add ⌥ or ⌃."
        );
        assert_eq!(
            no(pressed(21, CARBON_CMD_KEY | CARBON_SHIFT_KEY, "4")),
            Rejection::Reserved("⇧⌘4".into())
        );
        assert_eq!(
            no(pressed(49, CARBON_CONTROL_KEY, " ")).message(),
            "macOS uses ⌃Space."
        );
        let mut system = pressed(R, CARBON_CONTROL_KEY | CARBON_OPTION_KEY, "r");
        system.reserved = true;
        assert_eq!(no(system), Rejection::Reserved("⌃⌥R".into()));
        // Scrawl's other shortcut.
        assert_eq!(
            no(pressed(9, OPT_SHIFT, "v")),
            Rejection::Taken(ShortcutKind::Paste)
        );
        assert_eq!(
            Rejection::Taken(ShortcutKind::Paste).message(),
            "Paste last capture already uses this."
        );
    }

    #[test]
    fn keys_are_named_for_the_layout_in_use() {
        assert_eq!(
            key_label(41, "s").as_deref(),
            Some("S"),
            "Dvorak S sits on ANSI ;"
        );
        assert_eq!(key_label(49, " ").as_deref(), Some("Space"));
        assert_eq!(key_label(122, "").as_deref(), Some("F1"));
        assert_eq!(key_label(44, "/").as_deref(), Some("/"));
        assert_eq!(key_label(53, "\u{1b}"), None);
        assert_eq!(key_label(0, ""), None);
    }

    #[test]
    fn shortcuts_round_trip_through_the_config_keeping_other_keys_and_0600() {
        let dir = TempDir::new("round-trip");
        let path = dir.0.join("config.json");
        let legacy = dir.0.join("legacy.json");
        std::fs::write(&path, r#"{"ingestToken":"secret","autoSend":false}"#).unwrap();
        assert_eq!(
            Shortcuts::load_from(&path, &legacy),
            (Shortcuts::default(), false)
        );
        let custom = ShortcutConfig {
            key_code: R,
            carbon_modifiers: CARBON_CONTROL_KEY | CARBON_OPTION_KEY,
            label: "⌃⌥R".into(),
        };
        save_shortcut_in(&path, ShortcutKind::Record, &custom).unwrap();
        let (loaded, _) = Shortcuts::load_from(&path, &legacy);
        assert_eq!(loaded.record, custom);
        assert_eq!(loaded.paste, ShortcutConfig::default_paste());
        let config = read_json(&path);
        assert_eq!(config["ingestToken"], "secret");
        assert_eq!(config["autoSend"], false);
        assert_eq!(config["shortcuts"]["record"]["label"], "⌃⌥R");
        assert!(
            config["shortcuts"].get("paste").is_none(),
            "defaults aren't written"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Back to the default removes it.
        save_shortcut_in(
            &path,
            ShortcutKind::Record,
            &ShortcutConfig::default_record(),
        )
        .unwrap();
        assert!(read_json(&path)["shortcuts"].get("record").is_none());
        assert_eq!(read_json(&path)["ingestToken"], "secret");
    }

    #[test]
    fn a_custom_shortcut_from_0_3_moves_across_once() {
        let dir = TempDir::new("legacy");
        let path = dir.0.join("config.json");
        let legacy = dir.0.join("legacy.json");
        // 0.3.x wrote its default too: that stays the default (on the active layout's S).
        std::fs::write(
            &legacy,
            r#"{"key_code":41,"carbon_modifiers":2560,"label":"⌥⇧S"}"#,
        )
        .unwrap();
        assert_eq!(
            Shortcuts::load_from(&path, &legacy),
            (Shortcuts::default(), false)
        );
        std::fs::write(
            &legacy,
            r#"{"key_code":15,"carbon_modifiers":6144,"label":"⌃⌥R"}"#,
        )
        .unwrap();
        let (loaded, migrate) = Shortcuts::load_from(&path, &legacy);
        assert!(migrate);
        assert_eq!(loaded.record.label, "⌃⌥R");
        // Once the new config has shortcuts, the old file is ignored.
        save_shortcut_in(
            &path,
            ShortcutKind::Record,
            &ShortcutConfig::default_record(),
        )
        .unwrap();
        assert_eq!(
            Shortcuts::load_from(&path, &legacy),
            (Shortcuts::default(), false)
        );
    }

    #[test]
    fn recording_pauses_the_hotkeys_and_a_good_combo_is_saved() {
        let dir = TempDir::new("record");
        let mut host = fake(&dir);
        let mut panel = Panel::default();
        panel.on(
            Action::RecordShortcut {
                kind: ShortcutKind::Record,
            },
            &mut host,
        );
        assert_eq!(panel.recording(), Some(ShortcutKind::Record));
        assert_eq!(host.paused, [true]);
        // A key for the other recorder does nothing.
        panel.on(
            Action::Shortcut {
                kind: ShortcutKind::Paste,
                pressed: pressed(R, CARBON_CONTROL_KEY | CARBON_OPTION_KEY, "r"),
            },
            &mut host,
        );
        assert_eq!(host.shortcuts, Shortcuts::default());
        panel.on(
            Action::Shortcut {
                kind: ShortcutKind::Record,
                pressed: pressed(R, CARBON_CONTROL_KEY | CARBON_OPTION_KEY, "r"),
            },
            &mut host,
        );
        assert_eq!(host.paused, [true, false]);
        assert_eq!(panel.recording(), None);
        assert_eq!(host.shortcuts.record.label, "⌃⌥R");
        assert_eq!(read_json(&host.path)["shortcuts"]["record"]["label"], "⌃⌥R");
        let view = panel.view(&host);
        let record = &view.shortcuts[0];
        assert_eq!(
            (
                record.key.as_str(),
                record.control,
                record.option,
                record.shift
            ),
            ("R", true, true, false)
        );
        assert!(!record.is_default && record.error.is_none());
        // Reset puts the default back and removes it from the config.
        panel.on(
            Action::ResetShortcut {
                kind: ShortcutKind::Record,
            },
            &mut host,
        );
        assert!(host.shortcuts.record.is_default_for(ShortcutKind::Record));
        assert!(read_json(&host.path)["shortcuts"].get("record").is_none());
    }

    #[test]
    fn a_bad_or_refused_combo_keeps_the_old_one_and_says_why() {
        let dir = TempDir::new("refused");
        let mut host = fake(&dir);
        let mut panel = Panel::default();
        let press = |panel: &mut Panel, host: &mut Fake, p: Pressed| {
            panel.on(
                Action::RecordShortcut {
                    kind: ShortcutKind::Paste,
                },
                host,
            );
            panel.on(
                Action::Shortcut {
                    kind: ShortcutKind::Paste,
                    pressed: p,
                },
                host,
            );
        };
        press(&mut panel, &mut host, pressed(R, CARBON_SHIFT_KEY, "r"));
        assert_eq!(host.shortcuts, Shortcuts::default());
        assert_eq!(
            panel.view(&host).shortcuts[1].error.as_deref(),
            Some("Hold ⌘, ⌥ or ⌃ with the key.")
        );
        host.refuse = Some("⌃⌥R".into());
        press(
            &mut panel,
            &mut host,
            pressed(R, CARBON_CONTROL_KEY | CARBON_OPTION_KEY, "r"),
        );
        assert_eq!(host.shortcuts, Shortcuts::default());
        assert_eq!(
            panel.view(&host).shortcuts[1].error.as_deref(),
            Some("Another app has this shortcut.")
        );
        assert!(!dir.0.join("config.json").exists(), "nothing saved");
        // Esc cancels and resumes the hotkeys; the old message goes when recording starts again.
        panel.on(
            Action::RecordShortcut {
                kind: ShortcutKind::Paste,
            },
            &mut host,
        );
        assert!(panel.view(&host).shortcuts[1].error.is_none());
        panel.on(Action::CancelShortcut, &mut host);
        assert_eq!(host.paused.last(), Some(&false));
        assert_eq!(panel.recording(), None);
        // Closing the window while recording resumes them too.
        panel.on(
            Action::RecordShortcut {
                kind: ShortcutKind::Record,
            },
            &mut host,
        );
        assert_eq!(panel.on(Action::Close, &mut host), Effect::Close);
        assert_eq!(host.paused.last(), Some(&false));
    }

    #[test]
    fn toggles_and_popups_write_their_keys() {
        let dir = TempDir::new("set");
        let mut host = fake(&dir);
        std::fs::write(&host.path, r#"{"ingestToken":"secret"}"#).unwrap();
        let mut panel = Panel::default();
        let set = |panel: &mut Panel, host: &mut Fake, key: &str, value: serde_json::Value| {
            panel.on(
                Action::Set {
                    key: key.into(),
                    value,
                },
                host,
            )
        };
        assert!(panel.view(&host).auto_send);
        set(&mut panel, &mut host, "autoSend", false.into());
        set(&mut panel, &mut host, "videoExport", true.into());
        set(&mut panel, &mut host, "transcriber", "hex".into());
        assert_eq!(
            set(&mut panel, &mut host, "retention", "month".into()),
            Effect::Measure
        );
        set(&mut panel, &mut host, "transcriber", "nonsense".into());
        let view = panel.view(&host);
        assert!(!view.auto_send && view.video_export);
        assert_eq!(view.retention.preset, Some("month"));
        assert_eq!(
            view.retention.note,
            "Video and audio go to the Trash after 30 days, the rest of each capture after 90 days."
        );
        let config = read_json(&host.path);
        assert_eq!(config["transcriber"], "hex");
        assert_eq!(config["ingestToken"], "secret");
        set(&mut panel, &mut host, "hexModel", "parakeet".into());
        assert_eq!(read_json(&host.path)["hexModel"], "parakeet");
        set(&mut panel, &mut host, "retention", "forever".into());
        // "Forever" says it all; no caption.
        assert_eq!(panel.view(&host).retention.note, "");
    }

    #[test]
    fn permission_rows_ask_once_then_open_settings() {
        let dir = TempDir::new("grant");
        let mut host = fake(&dir);
        let mut panel = Panel::default();
        let mut grants = Grants::default();
        grants
            .statuses
            .insert(Permission::Microphone, Status::Granted);
        grants.statuses.insert(Permission::Camera, Status::Denied);
        panel.set_grants(grants.clone());
        let grant = |panel: &mut Panel, host: &mut Fake, permission| {
            panel.on(Action::Grant { permission }, host)
        };
        assert_eq!(
            grant(&mut panel, &mut host, Permission::Microphone),
            Effect::None
        );
        assert_eq!(
            grant(&mut panel, &mut host, Permission::Accessibility),
            Effect::Request(Permission::Accessibility)
        );
        assert_eq!(
            grant(&mut panel, &mut host, Permission::Accessibility),
            Effect::OpenSettings(Permission::Accessibility)
        );
        assert_eq!(
            grant(&mut panel, &mut host, Permission::Camera),
            Effect::OpenSettings(Permission::Camera)
        );
        let view = panel.view(&host);
        let order: Vec<_> = view.permissions.iter().map(|p| p.permission).collect();
        assert_eq!(
            order,
            [
                Permission::ScreenRecording,
                Permission::Microphone,
                Permission::Accessibility,
                Permission::Camera
            ],
            "no Input Monitoring"
        );
        assert!(view.permissions[..3].iter().all(|p| p.required));
        assert!(!view.permissions[3].required);
        // Screen Recording granted but not yet usable: Reopen relaunches.
        assert_eq!(panel.on(Action::Reopen, &mut host), Effect::None);
        grants
            .statuses
            .insert(Permission::ScreenRecording, Status::Granted);
        grants.screen_needs_relaunch = true;
        panel.set_grants(grants);
        assert!(panel.view(&host).permissions[0].relaunch);
        assert_eq!(panel.on(Action::Reopen, &mut host), Effect::Relaunch);
    }

    #[test]
    fn helper_lines_parse_into_actions() {
        let parse = |line: &str| serde_json::from_str::<Action>(line).unwrap();
        assert_eq!(
            parse(
                r#"{"action":"shortcut","kind":"paste","keyCode":15,"modifiers":6144,"key":"r","reserved":false}"#
            ),
            Action::Shortcut {
                kind: ShortcutKind::Paste,
                pressed: pressed(R, CARBON_CONTROL_KEY | CARBON_OPTION_KEY, "r")
            }
        );
        assert_eq!(
            parse(r#"{"action":"set","key":"autoSend","value":true}"#),
            Action::Set {
                key: "autoSend".into(),
                value: true.into()
            }
        );
        assert_eq!(
            parse(r#"{"action":"record-shortcut","kind":"record"}"#),
            Action::RecordShortcut {
                kind: ShortcutKind::Record
            }
        );
        assert_eq!(
            parse(r#"{"action":"grant","permission":"camera"}"#),
            Action::Grant {
                permission: Permission::Camera
            }
        );
        assert_eq!(
            parse(r#"{"action":"check-for-updates"}"#),
            Action::CheckForUpdates
        );
    }
}
