# Linux (Ubuntu / Zorin OS X11) Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port Scrawl to Ubuntu/Zorin OS Linux (X11) with full feature parity—global hotkeys, `Shift`-drag GPU red ink overlay, Dynamic Island HUD pill, in-process `parakeet-tdt-0.6b-v3` ONNX speech transcription with word timings, smart clipboard/paste handoff for OpenChamber (GUI) and OpenCode TUI (terminal), `/proc` dev-server routing, and FFmpeg video export.

**Architecture:** Retain Scrawl's portable Rust core (`model`, `sketch`, `keyframes`, `autocrop`, `render`, `audio`, `web`, `inbox`, `delivery`) unchanged while gating Apple frameworks under `#[cfg(target_os = "macos")]` and adding native Linux X11 backends (`x11rb` RandR/EWMH/XInput2/XFixes/Shape/XTest/XShm, `wgpu` WGSL overlays, `fontdb`+`ab_glyph`+`tiny-skia` text/ink rendering, `arboard` clipboard, `/proc` socket inspection, and `ffmpeg`/`ffprobe` media helpers). Speech-to-text on Linux runs completely in-process via `ort` (ONNX Runtime) using `parakeet-tdt-0.6b-v3` weights downloaded on demand to `~/.local/share/scrawl/models/parakeet-tdt-0.6b-v3/`.

**Tech Stack:** Rust 2024, `gpui = "0.2.2"`, `x11rb`, `wgpu`, `bytemuck`, `ab_glyph`, `fontdb`, `tiny-skia`, `arboard`, `ort`, `ndarray`, `rustfft`, `ureq`, `tray-icon`, `muda`, `ffmpeg` / `ffprobe`.

**Spec:** `docs/superpowers/specs/2026-10-19-linux-ubuntu-support-design.md`

## Global Constraints

- Target environment: Ubuntu-based Linux desktops (specifically Zorin OS / GNOME Shell) running on **X11** (`$XDG_SESSION_TYPE=x11`).
- X11 uses a **top-left origin (`Y-down`)** globally across RandR monitors, EWMH window geometry, and `XInput2` root pointer coordinates; `screens::flip` on Linux is an identity transform.
- Scrawl MUST NOT require the user to install Python, `sherpa-onnx` CLI binaries, Ollama, or a local HTTP transcription server.
- Model files must be stored under the XDG data directory (`~/.local/share/scrawl/models/parakeet-tdt-0.6b-v3/`).
- On Linux, `Engine::Parakeet` (`"parakeet"`) is the default transcription engine (`Engine::default()`), producing native word-level timestamps (`WordTiming` with `estimated: false`), with `Engine::Whisper` (`"whisper"`) retained as a secondary fallback option.
- Modules that are already portable (`model.rs`, `sketch.rs`, `keyframes.rs`, `render.rs`, `zoom.rs`, `autocrop.rs`, `audio.rs`, `web.rs`, `inbox.rs`, `delivery.rs`) must remain shared across macOS and Linux rather than duplicated.
- On Linux X11, GUI apps (including **OpenChamber**, Electron/WebKit/GTK apps) paste on `Ctrl+V`, whereas terminal emulators running **OpenCode TUI** (`gnome-terminal`, `ptyxis`, `kitty`, `alacritty`, `wezterm`, `foot`, `ghostty`, `konsole`, `tilix`, `xfce4-terminal`, `xterm`) require `Ctrl+Shift+V`.
- Because X11 clipboard ownership is asynchronous, Scrawl's clipboard manager (`arboard::Clipboard`) must remain alive in a persistent singleton inside the running process.
- On Linux, `default_captures_dir()` resolves to `dirs::data_local_dir().unwrap_or("~/.local/share").join("scrawl/captures")` (overridable via `SCRAWL_CAPTURES_DIR`).
- USB cabled iPhone mirroring (`CoreMediaIO`), Apple `SpeechAnalyzer`, and Sparkle (`sparkle.rs`) remain `#[cfg(target_os = "macos")]`.

## Review Focus

1. **Active CapsLock (`LockMask`) or NumLock (`Mod2Mask`) during global hotkey press, or early capture cancel while `Shift`-drawing**: Shortcut matching must strip X11 lock bits (`Mod2Mask | LockMask`) so `Alt+Shift+S` and `Alt+Shift+V` fire regardless of lock state, and cursor hiding (`XFixesHideCursor`) must be guarded by an RAII struct that calls `XFixesShowCursor` on drop so an aborted capture never leaves the desktop cursor invisible (Task 3).
2. **Parakeet TDT greedy decoder emitting consecutive `duration == 0` tokens on noisy/silent audio, or encountering an incomplete `.part` model download**: The TDT decode loop must cap zero-duration emissions per encoder frame (max 8 tokens before advancing `t += 1`) so it can never hang in an infinite loop, and `ensure_parakeet_model` must ignore `.part` files and verify all three model files are non-empty before marking the model ready (Task 6).
3. **GTK client-side decorations (`_GTK_FRAME_EXTENTS`) and single-rect `_NET_WORKAREA` on multi-monitor X11 desktops**: Window bounds must subtract `_GTK_FRAME_EXTENTS` (left, right, top, bottom shadow margins) while adding `_NET_WM_EXTENTS` server frames so window-mode captures do not include transparent shadow borders, and `visible` screen frame calculation must fall back to the monitor's own `frame` if intersecting `_NET_WORKAREA` would collapse width or height below 50% of the monitor (Task 2).
4. **Case-insensitive and reverse-DNS `WM_CLASS` / process names (`"OpenChamber"`, `"org.gnome.Ptyxis"`, `"com.mitchellh.ghostty"`) during `Alt+Shift+V` paste**: `is_terminal_class_or_name` must match both plain binary names and reverse-DNS `.desktop`/flatpak IDs case-insensitively while treating `"openchamber"` as a GUI target (`Ctrl+V` two-step image + text), and `post_synthetic_paste` must release held physical modifiers (`Alt`, `Shift`, `Control`, `Super`) before injecting `Ctrl+V` / `Ctrl+Shift+V` (Task 7).
5. **Dual-stack IPv6 (`::1` or IPv4-mapped `::ffff:127.0.0.1`) listening sockets and `TCP_ESTABLISHED` (`01`) browser client sockets in `/proc/net/tcp6`**: `/proc/net/tcp*` parsing must require socket state `0A` (`TCP_LISTEN`) so browser client sockets on the same port are ignored, and must decode both 32-bit hex IPv4 addresses and 128-bit hex IPv6 addresses (Task 8).

---

## File Structure

- **`Cargo.toml`, `build.rs`, `scripts/install-deps-ubuntu.sh`**: Cross-platform dependency partitioning, Linux system package installer script, and gating macOS `xcrun` build steps.
- **`src/screens.rs`**: Linux X11 RandR monitor enumeration, EWMH workarea/strut inset calculation, HiDPI scale detection, and identity `flip(r, primary_h)`.
- **`src/windows.rs`**: Linux EWMH `_NET_CLIENT_LIST_STACKING` window enumeration, frame extent adjustment, `_NET_ACTIVE_WINDOW` frontmost detection, and window activation.
- **`src/capture.rs`**: Linux X11 passive key grabs (`XGrabKey`), `XInput2` raw motion/button/key stream + 120 Hz pointer/modifier poller, `XFixes` cursor hide/show guard during `Shift` ink drawing, and `XShmGetImage` screen capture + `ffmpeg` MP4 recording.
- **`src/laser_overlay.wgsl`, `src/laser_overlay.rs`**: WGSL port of `laser_overlay.metal` and `wgpu` click-through X11 ARGB fullscreen overlay (`MetalLaserOverlay` platform alias/implementation).
- **`src/native/pill/atlas.rs`, `src/native/pill/pill.wgsl`, `src/native/pill/mod.rs`**: Pure-Rust `fontdb` + `ab_glyph` + `tiny-skia` glyph/icon atlas, WGSL port of `pill.metal`, and `wgpu` top-center X11 HUD pill with dynamic `ShapeInput` hit region.
- **`src/parakeet.rs`, `src/transcribe.rs`**: In-process `parakeet-tdt-0.6b-v3` HuggingFace downloader, 80-bin Slaney log-mel filterbank, ONNX Runtime (`ort`) TDT greedy decoder with word timestamps, and `Engine::Parakeet` / `Engine::Whisper` dispatch.
- **`src/handoff.rs`**: Linux XDG `default_captures_dir()`, persistent `arboard::Clipboard` singleton, terminal vs OpenChamber GUI detection, and `XTest` synthetic paste (`Ctrl+V` vs `Ctrl+Shift+V`).
- **`src/routing.rs`**: Linux browser `WM_CLASS` recognition, window-title/SDK URL lookup, and `/proc/net/tcp*` -> `/proc/<pid>/fd/*` -> `/proc/<pid>/cwd` port-to-repository resolver.
- **`src/helper.rs`, `src/export/mod.rs`, `src/export/ink.rs`, `src/retention.rs`**: Linux `ffprobe`/`ffmpeg` media operations, `tiny-skia` export ink/ripple/pin compositor, `gio trash` / XDG trash support, and `nice`/`setpriority` background priority.
- **`src/ui.rs`, `src/onboarding.rs`, `src/settings.rs`, `src/sparkle.rs`, `src/phone.rs`**: Linux system tray (`tray-icon` + `muda`), GPUI Linux windowing, Linux 4-step Onboarding flow (System Readiness, Parakeet Model Setup, Interactive Test Capture, Agent Paste Test), and no-op stubs for macOS-only `sparkle`/`phone` USB mirror.

---

### Task 1: Build System & Platform Dependency Gates

**Files:**
- Modify: `Cargo.toml:1-43`
- Modify: `build.rs:1-24`
- Modify: `src/sparkle.rs:1-160`
- Modify: `src/phone.rs:1-120`
- Create: `scripts/install-deps-ubuntu.sh`

**Interfaces:**
- Consumes: none
- Produces:
  - Shared dependencies: `gpui = "0.2.2"`, `ort = "2.0.0-rc.9"`, `ndarray = "0.16"`, `rustfft = "6.2"`, `ureq = "2.12"`
  - Linux target dependencies (`[target.'cfg(target_os = "linux")'.dependencies]`): `x11rb = { version = "0.13.1", features = ["randr", "shm", "xfixes", "shape", "xinput", "xtest", "render", "composite"] }`, `wgpu = "24.0"`, `raw-window-handle = "0.6"`, `bytemuck = { version = "1.21", features = ["derive"] }`, `ab_glyph = "0.2.29"`, `fontdb = "0.22"`, `tiny-skia = "0.11.4"`, `arboard = "3.4"`, `image = { version = "0.25", default-features = false, features = ["png"] }`, `tray-icon = "0.19"`, `muda = "0.15"`
  - `src/sparkle.rs` non-macOS functions: `pub fn start()`, `pub fn available() -> bool` (`false`), `pub fn check_for_updates()`

- [ ] **Step 1: Write a build-config verification test in `src/sparkle.rs`**

```rust
#[cfg(test)]
mod linux_gate_tests {
    use super::*;

    #[test]
    fn sparkle_reports_unavailable_when_not_bundled_macos_app() {
        #[cfg(not(target_os = "macos"))]
        assert!(!available());
    }
}
```

- [ ] **Step 2: Run test to verify current baseline**

Run: `taskpolicy -c utility cargo test -j 3 --offline sparkle::linux_gate_tests -- --test-threads=2`
Expected: FAIL with `sparkle` failing to compile on non-macOS or missing test module.

- [ ] **Step 3: Partition `Cargo.toml`, gate `src/sparkle.rs` and `src/phone.rs` macOS FFI, and create `scripts/install-deps-ubuntu.sh`**

Move `gpui` into shared `[dependencies]`, add the Linux and ONNX dependencies in `Cargo.toml`, gate the `objc2` link blocks in `src/sparkle.rs` under `#[cfg(target_os = "macos")]` with no-op `#[cfg(not(target_os = "macos"))]` implementations (`available() -> false`, `start() {}`, `check_for_updates() {}`), and create `scripts/install-deps-ubuntu.sh` installing:
`libx11-dev libxcb1-dev libxrandr-dev libxi-dev libxtst-dev libxfixes-dev libxkbcommon-dev libxkbcommon-x11-dev libasound2-dev libgtk-3-dev libayatana-appindicator3-dev libvulkan-dev vulkan-tools ffmpeg pkg-config build-essential`.

- [ ] **Step 4: Run test to verify it passes**

Run: `taskpolicy -c utility cargo test -j 3 sparkle::linux_gate_tests -- --test-threads=2`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock build.rs src/sparkle.rs src/phone.rs scripts/install-deps-ubuntu.sh
git commit -m "build: partition macOS/Linux dependencies and add Ubuntu deps script"
```

---

### Task 2: X11 Display & Window Introspection

**Files:**
- Modify: `src/screens.rs:32-234`
- Modify: `src/windows.rs:219-365`

**Interfaces:**
- Consumes: `x11rb::protocol::{randr, xproto}`
- Produces:
  - `src/screens.rs`:
    - `pub fn flip(r: Rect, primary_h: f64) -> Rect` (returns `r` unchanged on `#[cfg(target_os = "linux")]`)
    - `pub fn compute_workarea_visible(frame: Rect, workarea: Option<Rect>, struts: &[[u32; 12]]) -> Rect`
    - `pub fn parse_xft_dpi(resource_manager: &str) -> Option<f64>`
    - `pub fn displays() -> Vec<Screen>` (X11 RandR 1.5 `get_monitors` / CRTC fallback)
    - `pub fn current() -> Vec<Screen>` (X11 `displays()` with `_NET_WORKAREA` / `_NET_WM_STRUT_PARTIAL` applied to `visible` and `safe_top: 0.0`)
  - `src/windows.rs`:
    - `pub struct FrontmostApp { pub name: String, pub bundle_id: Option<String>, pub pid: i32 }`
    - `pub fn apply_frame_extents(geom: [f64; 4], net_extents: Option<[i32; 4]>, gtk_extents: Option<[i32; 4]>) -> [f64; 4]`
    - `pub fn raw_windows() -> Vec<RawWindow>` (X11 EWMH `_NET_CLIENT_LIST_STACKING` reversed to front-to-back)
    - `pub fn frontmost_application() -> Option<FrontmostApp>` (reads `_NET_ACTIVE_WINDOW`, `WM_CLASS`, `_NET_WM_NAME`, `_NET_WM_PID`)
    - `pub fn activate_app(pid: i32, bundle_id: Option<&str>) -> bool` (sends `_NET_ACTIVE_WINDOW` client message to root window)
    - `pub fn dock_rect(frame: TargetFrame) -> Option<[f64; 4]>`

- [ ] **Step 1: Write the failing tests in `src/screens.rs` and `src/windows.rs` (including Review Focus #3)**

In `src/screens.rs`:
```rust
#[test]
fn xft_dpi_and_workarea_visibility_handle_hidpi_and_multi_monitor_panels() {
    assert_eq!(parse_xft_dpi("Xft.dpi:\t192\nXft.antialias:\t1\n"), Some(2.0));
    assert_eq!(parse_xft_dpi("Xft.dpi: 96"), Some(1.0));
    assert_eq!(parse_xft_dpi("Xft.antialias: 1"), None);

    let primary = [0.0, 0.0, 1920.0, 1080.0];
    let secondary = [1920.0, 0.0, 1920.0, 1080.0];
    // Top bar of 32px and bottom Zorin panel of 48px on primary:
    let workarea = Some([0.0, 32.0, 3840.0, 1000.0]);
    assert_eq!(
        compute_workarea_visible(primary, workarea, &[]),
        [0.0, 32.0, 1920.0, 1000.0]
    );
    // Review Focus #3: broken/single-monitor _NET_WORKAREA that would clip secondary monitor to 0 width falls back to monitor frame.
    let single_workarea = Some([0.0, 32.0, 1920.0, 1000.0]);
    assert_eq!(
        compute_workarea_visible(secondary, single_workarea, &[]),
        secondary
    );
}
```

In `src/windows.rs`:
```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `taskpolicy -c utility cargo test -j 3 --offline xft_dpi_and_workarea_visibility frame_extents_subtract_gtk -- --test-threads=2`
Expected: FAIL with `cannot find function parse_xft_dpi` and `cannot find function apply_frame_extents`.

- [ ] **Step 3: Implement Linux X11 screen and window introspection in `src/screens.rs` and `src/windows.rs`**

In `src/screens.rs`:
- Gate macOS `CGDisplay`/`NSScreen` `displays()` and `current()` with `#[cfg(target_os = "macos")]` and add `#[cfg(target_os = "linux")]` implementations using `x11rb::connect(None)` and RandR (`randr::get_monitors` with CRTC fallback).
- Implement `parse_xft_dpi(resource_manager: &str) -> Option<f64>` (dividing `Xft.dpi` by `96.0`, clamped to `1.0..=4.0`) and `compute_workarea_visible(frame: Rect, workarea: Option<Rect>, struts: &[[u32; 12]]) -> Rect` (intersecting `workarea` and subtracting `_NET_WM_STRUT_PARTIAL` insets only when the resulting visible rect retains at least 50% of `frame` width and height).
- On `#[cfg(target_os = "linux")]`, `pub fn flip(r: Rect, _primary_h: f64) -> Rect` returns `r`.

In `src/windows.rs`:
- Implement `apply_frame_extents(geom: [f64; 4], net_extents: Option<[i32; 4]>, gtk_extents: Option<[i32; 4]>) -> [f64; 4]`: if `gtk_extents` is `Some([l, r, t, b])`, inset `geom` by `[+l, +t, -(l+r), -(t+b)]`; else if `net_extents` is `Some([l, r, t, b])`, expand `geom` by `[-l, -t, +(l+r), +(t+b)]`.
- Gate macOS `raw_windows()`, `app_meta_of()`, `dock_rect()`, and `snapshot()` with `#[cfg(target_os = "macos")]`, and implement `#[cfg(target_os = "linux")]` equivalents querying `_NET_CLIENT_LIST_STACKING` (iterated in reverse for front-to-back order), skipping unmapped or `_NET_WM_STATE_HIDDEN` windows, reading `_NET_WM_NAME` / `WM_NAME`, `WM_CLASS` (lowercased class as `bundle_id`, original class or `/proc/<pid>/comm` as `owner`), `_NET_WM_PID`, and implementing `frontmost_application()` and `activate_app()`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `taskpolicy -c utility cargo test -j 3 --offline screens:: windows:: -- --test-threads=2`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/screens.rs src/windows.rs
git commit -m "feat(x11): implement RandR screen discovery and EWMH window introspection"
```

---

### Task 3: X11 Global Hotkeys, Input Stream & `XShmGetImage` Screen Capture

**Files:**
- Modify: `src/capture.rs:130-1055,1750-3030`

**Interfaces:**
- Consumes: `crate::screens::{Screen, current, displays}`, `crate::windows::{frontmost_application, activate_app, raw_windows}`
- Produces:
  - `pub fn parse_x11_shortcut(label: &str) -> Option<(u32, u32)>` (maps `"Alt+Shift+S"`, `"⌥⇧S"`, `"Super+Shift+S"` to `(keysym, carbon_modifiers)`)
  - `pub fn x11_lock_permutations(base_mod_mask: u16) -> [u16; 4]` (returns `[base, base | MOD2, base | LOCK, base | MOD2 | LOCK]`)
  - `pub fn strip_x11_lock_mods(state: u16) -> u16`
  - `pub struct CursorHideGuard<F: FnOnce()> { on_drop: Option<F> }` (calls `XFixesShowCursor` on drop)
  - Linux `#[cfg(target_os = "linux")]` implementations of all public `src/capture.rs` entry points: `install_global_hotkeys`, `set_shortcuts`, `pause_shortcuts`, `screen_capture_granted` (`true`), `screen_capture_usable` (`true`), `accessibility_granted` (`true`), `modifier_flags`, `discover_target_at`, `list_selectable_windows`, `record_external_frontmost_app`, `ensure_external_app_active`, `frontmost_external_pid`, `frontmost_app_info`, `ScreenCaptureSession::{start, stop}`, `start_global_input_tap`, `current_mouse_screen`, `wait_for_modifiers_released`, `post_synthetic_click`.

- [ ] **Step 1: Write the failing tests in `src/capture.rs` (including Review Focus #1)**

```rust
#[test]
fn x11_shortcut_parsing_and_lock_mask_stripping_ignore_capslock_and_numlock() {
    // Review Focus #1: CapsLock (LockMask = 1<<1) and NumLock (Mod2Mask = 1<<4) permutations
    let base: u16 = (1 << 0) | (1 << 3); // ShiftMask | Mod1Mask (Alt)
    let perms = x11_lock_permutations(base);
    assert_eq!(perms, [base, base | (1 << 4), base | (1 << 1), base | (1 << 4) | (1 << 1)]);
    for mask in perms {
        assert_eq!(strip_x11_lock_mods(mask), base);
    }

    assert_eq!(
        parse_x11_shortcut("Alt+Shift+S"),
        Some((b's' as u32, CARBON_OPTION_KEY | CARBON_SHIFT_KEY))
    );
    assert_eq!(
        parse_x11_shortcut("⌥⇧V"),
        Some((b'v' as u32, CARBON_OPTION_KEY | CARBON_SHIFT_KEY))
    );
}

#[test]
fn cursor_hide_guard_restores_cursor_when_dropped_early() {
    // Review Focus #1: RAII cursor restore on cancel or panic
    let restored = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let flag = std::sync::Arc::clone(&restored);
        let _guard = CursorHideGuard::new(move || {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        assert!(!restored.load(std::sync::atomic::Ordering::SeqCst));
    }
    assert!(restored.load(std::sync::atomic::Ordering::SeqCst));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `taskpolicy -c utility cargo test -j 3 --offline x11_shortcut_parsing cursor_hide_guard -- --test-threads=2`
Expected: FAIL with `cannot find function x11_lock_permutations` and `cannot find struct CursorHideGuard`.

- [ ] **Step 3: Implement `CursorHideGuard`, X11 hotkey grabbing, `XInput2` input tap, and `XShmGetImage` capture in `src/capture.rs`**

- Implement portable helpers `x11_lock_permutations`, `strip_x11_lock_mods`, `parse_x11_shortcut`, and `CursorHideGuard::new(f: F)`.
- Under `#[cfg(target_os = "linux")]`:
  - `install_global_hotkeys` & `set_shortcuts`: resolve keysyms to keycodes via `GetKeyboardMapping`, call `xproto::grab_key` on the root window for all 4 `x11_lock_permutations`, and emit `LiveOverlayEvent::CaptureShortcut` / `PasteLatestShortcut` using the existing 90ms deduplication and `hold::Switcher` polling (`modifier_flags()` via `xproto::query_pointer`).
  - `start_global_input_tap`: subscribe to `xinput::EventMask::RAW_MOTION | RAW_BUTTON_PRESS | RAW_BUTTON_RELEASE | RAW_KEY_PRESS` on `XIAllMasterDevices` plus a 120 Hz pointer/modifier poller. When `ShiftMask` is held and pointer movement exceeds 6 px without typing a shifted character, acquire `CursorHideGuard` calling `xfixes::hide_cursor(conn, root)` and stream points into `telemetry.live_ink`; on `Shift` release or capture stop, drop `CursorHideGuard` (which calls `xfixes::show_cursor(conn, root)` and flushes the X11 connection).
  - `ScreenCaptureSession::start`: spawn a sampler thread using `shm::get_image` (falling back to `xproto::get_image`), converting BGRA to `RgbaFrame` into `CandidateBuffer` at `CANDIDATE_EDGE` (480 px) and piping raw frames into `ffmpeg -f rawvideo -pix_fmt bgra -s <W>x<H> -r 30 -i - -c:v libx264 -preset ultrafast -crf 23 -pix_fmt yuv420p recording.mp4`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `taskpolicy -c utility cargo test -j 3 --offline capture:: -- --test-threads=2`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/capture.rs
git commit -m "feat(x11): add global hotkeys, XInput2 telemetry, cursor guard, and XShm screen capture"
```

---

### Task 4: Click-Through Red Ink Overlay & WGSL Shader

**Files:**
- Create: `src/laser_overlay.wgsl`
- Modify: `src/laser_overlay.rs:1-759`

**Interfaces:**
- Consumes: `crate::capture::SharedCaptureTelemetry`, `crate::screens::{Screen, current, index_for_rect, index_at, pointer}`
- Produces:
  - `src/laser_overlay.wgsl`: `@vertex fn laser_vertex(@builtin(vertex_index) vid: u32) -> VertexOut` and `@fragment fn laser_fragment(in: VertexOut) -> @location(0) vec4<f32>` matching `LaserUniforms`, `array<PathSegment>`, and `array<PinMarker>` bindings at `@group(0) @binding(0..=2)`.
  - `src/laser_overlay.rs`: `pub struct MetalLaserOverlay` (implemented with `wgpu` + X11 override-redirect ARGB window on `#[cfg(target_os = "linux")]`, keeping the exact public API: `open(telemetry, on) -> Result<Self, String>`, `window_number(&self) -> u32`, `screen(&self) -> [f64; 4]`, `move_to(&mut self, rect: [f64; 4]) -> bool`).

- [ ] **Step 1: Write the failing shader validation and layout test in `src/laser_overlay.rs`**

```rust
#[test]
fn wgsl_laser_shader_parses_and_matches_uniform_struct_alignments() {
    let wgsl = include_str!("laser_overlay.wgsl");
    assert!(wgsl.contains("fn laser_vertex"));
    assert!(wgsl.contains("fn laser_fragment"));
    // WGSL uniform buffer alignment requires 16-byte multiple for LaserUniforms:
    assert_eq!(size_of::<LaserUniforms>() % 16, 0);
    assert_eq!(size_of::<PathSegment>(), 32);
    assert_eq!(size_of::<PinMarker>(), 16);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `taskpolicy -c utility cargo test -j 3 --offline wgsl_laser_shader_parses -- --test-threads=2`
Expected: FAIL with `couldn't read src/laser_overlay.wgsl`.

- [ ] **Step 3: Create `src/laser_overlay.wgsl` and implement Linux `wgpu` + X11 click-through window in `src/laser_overlay.rs`**

- Port `src/laser_overlay.metal` 1:1 into `src/laser_overlay.wgsl` (`LaserUniforms` at `@group(0) @binding(0)`, `var<storage, read> segments: array<PathSegment>` at `@group(0) @binding(1)`, `var<storage, read> pins: array<PinMarker>` at `@group(0) @binding(2)`), preserving capsule SDF, laser glow, ink outline fade, picker cutout dimming, red edge start glow, and the live red pen nib cursor tip when `tip_active > 0.5`.
- Derive `bytemuck::Pod` and `bytemuck::Zeroable` on `LaserUniforms`, `PathSegment`, and `PinMarker`.
- On `#[cfg(target_os = "linux")]`, implement `MetalLaserOverlay` creating a 32-bit ARGB visual X11 window (`override_redirect = true`, `_NET_WM_STATE_ABOVE`, `_NET_WM_STATE_STICKY`, `_NET_WM_WINDOW_TYPE_UTILITY`), calling `shape::rectangles(conn, shape::So::SET, shape::Sk::INPUT, ClipOrdering::UNSORTED, window, 0, 0, &[])` so all mouse input passes through, and driving a 60 Hz render thread with `wgpu` (`CompositeAlphaMode::PreMultiplied` / `PostMultiplied` fallback).

- [ ] **Step 4: Run test to verify it passes**

Run: `taskpolicy -c utility cargo test -j 3 --offline laser_overlay:: -- --test-threads=2`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/laser_overlay.wgsl src/laser_overlay.rs
git commit -m "feat(overlay): port laser_overlay shader to WGSL and add Linux X11 click-through wgpu overlay"
```

---

### Task 5: Pure-Rust Glyph Atlas & Dynamic Island HUD Pill

**Files:**
- Modify: `src/native/pill/atlas.rs:1-520`
- Create: `src/native/pill/pill.wgsl`
- Modify: `src/native/pill/mod.rs:1-1445`

**Interfaces:**
- Consumes: `crate::native::pill::model::{PillModel, FrameState, PANEL_W, PANEL_H, Metrics, HUD_TEXT}`, `crate::screens::{Screen, pill_origin}`
- Produces:
  - `src/native/pill/atlas.rs`:
    - `pub fn measure_text_pt(text: &str, size_pt: f32, weight: f64, monospaced_digits: bool) -> f32`
    - `pub fn measure_metrics() -> Metrics`
    - `pub fn rasterize_text_rgba(text: &str, size_pt: f32, weight: f64, monospaced_digits: bool, scale: f32) -> (u32, u32, [f32; 2], Vec<u8>)`
    - `pub fn rasterize_icon_rgba(kind: IconKind, size_pt: f32, scale: f32) -> (u32, u32, [f32; 2], Vec<u8>)`
  - `src/native/pill/pill.wgsl`: `pill_tile_vertex`, `pill_tile_fragment`, `pill_blur_h_vertex`, `pill_blur_h_fragment`, `pill_screen_vertex`, `pill_composite_fragment`
  - `src/native/pill/mod.rs`: `pub struct NativePill` with identical methods on macOS and Linux (`open`, `sync`, `place_on`, `try_recv_action`, `window_number`).

- [ ] **Step 1: Write the failing test for pure-Rust text and icon rasterization in `src/native/pill/atlas.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_rust_rasterizer_produces_non_empty_premultiplied_glyphs_and_icons() {
        let metrics = measure_metrics();
        assert!(metrics.record > 20.0 && metrics.clock_000 > 15.0);

        let (w, h, size_pt, rgba) = rasterize_text_rgba("0:05", HUD_TEXT, WEIGHT_SEMIBOLD, true, 2.0);
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        assert!(size_pt[0] > 10.0 && size_pt[1] > 8.0);
        assert!(rgba.chunks_exact(4).any(|px| px[3] > 128), "glyph alpha pixels rendered");

        for kind in [
            IconKind::Area,
            IconKind::Window,
            IconKind::Screen,
            IconKind::Close,
            IconKind::Check,
        ] {
            let (iw, ih, _, bytes) = rasterize_icon_rgba(kind, 14.0, 2.0);
            assert_eq!(bytes.len(), (iw * ih * 4) as usize);
            assert!(bytes.chunks_exact(4).any(|px| px[3] > 180), "{kind:?} icon has opaque strokes");
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `taskpolicy -c utility cargo test -j 3 --offline pure_rust_rasterizer_produces -- --test-threads=2`
Expected: FAIL with `cannot find function rasterize_text_rgba` and `cannot find function rasterize_icon_rgba`.

- [ ] **Step 3: Implement pure-Rust `ab_glyph`/`fontdb` + `tiny-skia` rasterization in `atlas.rs`, port `pill.wgsl`, and add Linux `NativePill` in `src/native/pill/mod.rs`**

- In `src/native/pill/atlas.rs`: implement `rasterize_text_rgba` (loading system sans-serif/monospace font via `fontdb::Database::new().load_system_fonts()` and rasterizing with `ab_glyph` into premultiplied white RGBA8 pixels) and `rasterize_icon_rgba` (drawing the exact SVG stroke coordinates for `Area`, `Window`, `Screen`, `Iphone`, `Close`, `Check` with `tiny_skia::PathBuilder` and `Stroke { width: 2.5 * s, line_cap: LineCap::Round, line_join: LineJoin::Round }`). Use these on Linux (and expose them for tests on all platforms).
- Port `src/native/pill/pill.metal` 1:1 to `src/native/pill/pill.wgsl`.
- In `src/native/pill/mod.rs`: add `#[cfg(target_os = "linux")]` `NativePill` backed by an override-redirect 32-bit ARGB X11 window (`PANEL_W × PANEL_H` at `screens::pill_origin`) rendered via `wgpu`. On each frame where `FrameState.shape` changes, update the X11 input shape (`shape::rectangles` with `shape::Sk::INPUT`) to the single rectangle `[shape.x, shape.y, shape.w, shape.h]` (or empty when `ignores_mouse` is true) so clicks inside the capsule hit its buttons while clicks outside pass through to the desktop.

- [ ] **Step 4: Run test to verify it passes**

Run: `taskpolicy -c utility cargo test -j 3 --offline native::pill:: -- --test-threads=2`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/native/pill/atlas.rs src/native/pill/pill.wgsl src/native/pill/mod.rs
git commit -m "feat(pill): add pure-Rust glyph/icon atlas, pill.wgsl, and Linux X11 wgpu HUD pill"
```

---

### Task 6: Self-Contained In-Process Parakeet Transcription

**Files:**
- Create: `src/parakeet.rs`
- Modify: `src/transcribe.rs:1-285`
- Modify: `src/main.rs:15-63`

**Interfaces:**
- Consumes: `crate::audio::{RecordedAudio, speech_islands}`, `crate::model::{TranscriptData, TranscriptSegment, WordTiming}`
- Produces:
  - `src/transcribe.rs`:
    - `pub enum Engine { Apple, Hex, Parakeet, Whisper }` (`Engine::default()` returns `Engine::Apple` on macOS, `Engine::Parakeet` on Linux; `Engine::parse` accepts `"apple"`, `"hex"`, `"parakeet"`, `"whisper"`)
  - `src/parakeet.rs`:
    - `pub const MODEL_ID: &str = "parakeet-tdt-0.6b-v3"`
    - `pub const HF_BASE_URL: &str = "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main"`
    - `pub const MODEL_FILES: [&str; 3] = ["encoder-model.int8.onnx", "decoder_joint-model.int8.onnx", "vocab.txt"]`
    - `pub fn model_dir() -> PathBuf` (`dirs::data_local_dir().unwrap_or("~/.local/share").join("scrawl/models/parakeet-tdt-0.6b-v3")`, overridable via `SCRAWL_PARAKEET_DIR`)
    - `pub fn is_model_ready_in(dir: &Path) -> bool`
    - `pub fn ensure_parakeet_model(progress: &mut dyn FnMut(u64, u64)) -> Result<PathBuf>`
    - `pub fn compute_log_mel_80(samples_16k: &[f32]) -> Vec<f32>` (80-bin Slaney log-mel filterbank, 25ms = 400-sample Hann window, 10ms = 160-sample hop, pre-emphasis `0.97`, per-feature mean/std normalization)
    - `pub struct TdtStepOut { pub token_id: usize, pub duration: usize, pub confidence: f32 }`
    - `pub fn decode_tdt_words<F>(num_frames: usize, blank_id: usize, vocab: &[String], step_fn: F, island_offset_ms: u64) -> Vec<WordTiming> where F: FnMut(usize, i64) -> Result<TdtStepOut>`
    - `pub fn transcribe_audio(audio: &RecordedAudio) -> Result<TranscriptData>`

- [ ] **Step 1: Write the failing tests in `src/parakeet.rs` and `src/transcribe.rs` (including Review Focus #2)**

In `src/parakeet.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_mel_80_produces_expected_frame_count_and_finite_values() {
        // 1.0 second of 16 kHz 440 Hz sine wave -> ~100 frames of 80 bins
        let samples: Vec<f32> = (0..16_000)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 16_000.0).sin() * 0.5)
            .collect();
        let mel = compute_log_mel_80(&samples);
        let frames = mel.len() / 80;
        assert!((98..=101).contains(&frames), "expected ~100 frames, got {frames}");
        assert!(mel.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn tdt_decoder_accumulates_subwords_into_measured_word_timings_and_caps_zero_duration_loops() {
        // Vocab: 0="▁now", 1="▁look", 2="ing", 3="<blank>"
        let vocab = vec!["▁now".to_string(), "▁look".to_string(), "ing".to_string(), "<blk>".to_string()];
        let blank_id = 3;
        let mut calls = 0;
        // Frame 0: emit "▁now" with duration 2 (160ms).
        // Frame 2: emit "▁look" with duration 0, then "ing" with duration 3 (240ms).
        // Frame 5: Review Focus #2 — emit "ing" with duration 0 forever; decoder must cap at 8 zero-duration steps and advance frame!
        let words = decode_tdt_words(6, blank_id, &vocab, |frame, _prev_tok| {
            calls += 1;
            Ok(match (frame, calls) {
                (0, 1) => TdtStepOut { token_id: 0, duration: 2, confidence: 0.95 },
                (2, 2) => TdtStepOut { token_id: 1, duration: 0, confidence: 0.90 },
                (2, 3) => TdtStepOut { token_id: 2, duration: 3, confidence: 0.92 },
                (5, _) => TdtStepOut { token_id: 2, duration: 0, confidence: 0.50 },
                _ => TdtStepOut { token_id: blank_id, duration: 1, confidence: 1.0 },
            })
        }, 1_000);

        assert!(calls <= 20, "zero-duration guard prevented infinite loop (calls={calls})");
        assert_eq!(words[0].word, "now");
        assert_eq!((words[0].start_ms, words[0].end_ms, words[0].estimated), (1_000, 1_160, false));
        assert_eq!(words[1].word, "looking");
        assert_eq!((words[1].start_ms, words[1].end_ms, words[1].estimated), (1_160, 1_400, false));
    }

    #[test]
    fn model_readiness_rejects_missing_or_partial_files() {
        // Review Focus #2: .part files or zero-byte files are not treated as ready
        let dir = std::env::temp_dir().join(format!("scrawl-parakeet-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in MODEL_FILES {
            std::fs::write(dir.join(format!("{name}.part")), b"partial").unwrap();
        }
        assert!(!is_model_ready_in(&dir));
        for name in MODEL_FILES {
            std::fs::write(dir.join(name), b"valid-onnx-bytes").unwrap();
        }
        assert!(is_model_ready_in(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
```

In `src/transcribe.rs`:
```rust
#[test]
fn parakeet_and_whisper_engine_names_parse() {
    assert_eq!(Engine::parse("parakeet"), Some(Engine::Parakeet));
    assert_eq!(Engine::parse("whisper"), Some(Engine::Whisper));
    #[cfg(target_os = "linux")]
    assert_eq!(Engine::default(), Engine::Parakeet);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `taskpolicy -c utility cargo test -j 3 --offline parakeet:: transcribe::tests::parakeet -- --test-threads=2`
Expected: FAIL with `unresolved module parakeet` and `no variant named Parakeet`.

- [ ] **Step 3: Implement `src/parakeet.rs` and wire `Engine::Parakeet` in `src/transcribe.rs` and `src/main.rs`**

- Create `src/parakeet.rs`:
  - `is_model_ready_in(dir: &Path) -> bool`: verifies all `MODEL_FILES` exist in `dir`, are regular files with `len() > 0`, and no `.part` file is left over without its completed target.
  - `ensure_parakeet_model`: downloads each missing file in `MODEL_FILES` from `HF_BASE_URL` via `ureq` into `<name>.part`, reporting `(downloaded_bytes, total_bytes)` to `progress`, then atomically renames `<name>.part` to `<name>`.
  - `compute_log_mel_80`: resamples/reads 16 kHz mono audio, applies `0.97` pre-emphasis, 400-sample Hann window, 160-sample hop, 512-point FFT (`rustfft`), 80 Slaney mel triangular filters (`0..8000 Hz`), natural log (`(energy + 1e-10).ln()`), and per-bin mean/variance normalization.
  - `decode_tdt_words`: runs the TDT greedy decode loop across encoder frames (`80 ms` per subsampled encoder frame = `8 * 10 ms`), capping `duration == 0` emissions at `MAX_ZERO_DURATION_STEPS = 8` per frame before forcing `frame += 1`, joining SentencePiece `"▁"` word-boundary tokens into `WordTiming { word, start_ms, end_ms, confidence: Some(conf), estimated: false }`.
  - Cache the loaded `ort::Session` pair (`encoder`, `decoder_joint`) and `vocab` in `static SESSION: OnceLock<Mutex<Option<ParakeetModel>>>`.
- Update `src/transcribe.rs` to add `Engine::Parakeet` and `Engine::Whisper`, set `#[cfg_attr(target_os = "linux", default)] Parakeet` and `#[cfg_attr(not(target_os = "linux"), default)] Apple`, and call `crate::parakeet::transcribe_audio(audio)` when `Engine::Parakeet` is active (with ` TranscriptData.source = "parakeet-tdt-0.6b-v3"` and `estimated = false`).

- [ ] **Step 4: Run tests to verify they pass**

Run: `taskpolicy -c utility cargo test -j 3 --offline parakeet:: transcribe:: -- --test-threads=2`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/parakeet.rs src/transcribe.rs src/main.rs
git commit -m "feat(transcribe): add self-contained in-process Parakeet TDT ONNX engine"
```

---

### Task 7: Linux Clipboard & Smart Agent Paste Handoff (OpenChamber GUI & OpenCode TUI)

**Files:**
- Modify: `src/handoff.rs:10-200`
- Modify: `src/capture.rs:2960-3030`

**Interfaces:**
- Consumes: `arboard::Clipboard`, `x11rb::protocol::xtest::fake_input`, `crate::windows::frontmost_application`
- Produces:
  - `src/handoff.rs`:
    - `pub fn default_captures_dir() -> PathBuf` (on Linux: `dirs::data_local_dir().unwrap_or("~/.local/share").join("scrawl/captures")`)
    - `pub fn copy_string_to_clipboard(content: &str) -> Result<()>` (on Linux: persistent `Mutex<Option<arboard::Clipboard>>`)
    - `pub fn copy_png_to_clipboard(png_bytes: &[u8]) -> Result<()>` (on Linux: decodes PNG with `image::load_from_memory` and calls `clipboard.set_image`)
    - `pub fn copy_bundle_to_clipboard(bundle_dir: &Path) -> Result<()>`
    - `pub fn paste_bundle_image_and_text(bundle_dir: &Path) -> Result<()>`
    - `pub fn paste_bundle_at_target(bundle_dir: &Path, click_target: Option<[f32; 2]>) -> Result<()>`
  - `src/capture.rs`:
    - `pub fn is_terminal_identifier(app_or_class: &str) -> bool`
    - `pub fn is_frontmost_terminal() -> bool`
    - `pub fn post_synthetic_paste(is_terminal: bool)` (on Linux: releases held modifiers via `XTest`, then sends `Ctrl+Shift+V` when `is_terminal == true`, or `Ctrl+V` when `is_terminal == false`)

- [ ] **Step 1: Write the failing tests in `src/capture.rs` and `src/handoff.rs` (including Review Focus #4)**

In `src/capture.rs`:
```rust
#[test]
fn terminal_identifier_distinguishes_linux_terminals_from_openchamber_and_gui_ides() {
    // Review Focus #4: case variations, reverse-DNS desktop IDs, OpenChamber GUI vs OpenCode TUI terminals
    for term in [
        "gnome-terminal",
        "Gnome-terminal-server",
        "org.gnome.Ptyxis",
        "ptyxis",
        "kitty",
        "Alacritty",
        "org.wezfurlong.wezterm",
        "foot",
        "com.mitchellh.ghostty",
        "konsole",
        "tilix",
        "xfce4-terminal",
        "xterm",
        "urxvt",
        "terminator",
        " warp ",
        "rio",
        "hyper",
        "tabby",
    ] {
        assert!(is_terminal_identifier(term), "{term} should be recognized as terminal");
    }
    for gui in [
        "openchamber",
        "OpenChamber",
        "dev.openchamber.app",
        "code",
        "Cursor",
        "dev.zed.Zed",
        "windsurf",
        "firefox",
        "google-chrome",
    ] {
        assert!(!is_terminal_identifier(gui), "{gui} should be recognized as GUI");
    }
}
```

In `src/handoff.rs`:
```rust
#[test]
fn linux_default_captures_dir_respects_env_and_xdg_data_local() {
    let custom = std::path::PathBuf::from("/tmp/scrawl-custom-captures");
    unsafe { std::env::set_var("SCRAWL_CAPTURES_DIR", &custom) };
    assert_eq!(default_captures_dir(), custom);
    unsafe { std::env::remove_var("SCRAWL_CAPTURES_DIR") };
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `taskpolicy -c utility cargo test -j 3 --offline terminal_identifier_distinguishes linux_default_captures_dir -- --test-threads=2`
Expected: FAIL with `cannot find function is_terminal_identifier`.

- [ ] **Step 3: Implement `is_terminal_identifier`, persistent `arboard` clipboard, and `XTest` synthetic paste**

- In `src/capture.rs`:
  - Implement `pub fn is_terminal_identifier(app_or_class: &str) -> bool`: normalize to lowercase, strip reverse-DNS prefixes (or check `.split('.').next_back()`), return `false` if it contains `"openchamber"`, and match the spec's terminal list (`gnome-terminal`, `gnome-terminal-server`, `ptyxis`, `kitty`, `alacritty`, `wezterm`, `foot`, `ghostty`, `konsole`, `tilix`, `xfce4-terminal`, `xterm`, `urxvt`, `terminator`, `warp`, `rio`, `hyper`, `tabby`, `iterm2`, `terminal`).
  - Implement `pub fn post_synthetic_paste(is_terminal: bool)`: on macOS call `post_synthetic_cmd_v()`; on Linux connect via `x11rb`, wait up to 150ms for physical modifiers (`Alt_L`, `Alt_R`, `Shift_L`, `Shift_R`, `Control_L`, `Control_R`, `Super_L`) to release and send `xtest::fake_input` `KEY_RELEASE_EVENT` for any still-depressed modifier keycodes, then synthesize `Control_L` down + (`Shift_L` down if `is_terminal`) + `v` down + `v` up + (`Shift_L` up if `is_terminal`) + `Control_L` up.
- In `src/handoff.rs`:
  - Update `default_captures_dir()` on `#[cfg(target_os = "linux")]` to use `dirs::data_local_dir().unwrap_or_else(|| home.join(".local/share")).join("scrawl/captures")`.
  - Implement `copy_string_to_clipboard` and `copy_png_to_clipboard` on Linux using a process-global `static CLIPBOARD: Mutex<Option<arboard::Clipboard>> = Mutex::new(None)` so X11 selection requests continue to be served.
  - Enable `copy_bundle_to_clipboard`, `paste_bundle_at_target`, and `paste_bundle_image_and_text` on both macOS and Linux:
    - On Linux, when `is_frontmost_terminal()` is `true` (OpenCode TUI), copy the formatted `summary` (which already contains absolute paths to `contact-sheet.png`, `frames/*.png`, and `summary.md`) and call `post_synthetic_paste(true)` (`Ctrl+Shift+V`).
    - When `is_frontmost_terminal()` is `false` (OpenChamber GUI), copy `contact-sheet.png` via `copy_png_to_clipboard`, call `post_synthetic_paste(false)` (`Ctrl+V`), sleep 120ms, copy `summary` via `copy_string_to_clipboard`, and call `post_synthetic_paste(false)` (`Ctrl+V`).

- [ ] **Step 4: Run tests to verify they pass**

Run: `taskpolicy -c utility cargo test -j 3 --offline terminal_identifier_distinguishes handoff:: -- --test-threads=2`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/handoff.rs src/capture.rs
git commit -m "feat(handoff): add X11 arboard clipboard and Ctrl+V / Ctrl+Shift+V paste for OpenChamber & OpenCode TUI"
```

---

### Task 8: Localhost Dev-Server `/proc` Routing

**Files:**
- Modify: `src/routing.rs:109-260`

**Interfaces:**
- Consumes: `crate::web`, `crate::windows`
- Produces:
  - `src/routing.rs`:
    - `pub fn parse_proc_net_tcp_inodes(content: &str, port: u16) -> Vec<u64>` (parses `/proc/net/tcp` and `/proc/net/tcp6` lines in state `0A` `TCP_LISTEN` matching `port`)
    - `pub fn extract_localhost_url_from_title(title: &str) -> Option<String>`
    - `pub fn browser_kind(bundle_id: &str) -> Option<Browser>` (recognizes Linux `WM_CLASS` values `google-chrome`, `chromium`, `brave-browser`, `firefox`, `zen`, `vivaldi-stable`, `microsoft-edge`)
    - `pub fn listener_directory(port: u16) -> Option<PathBuf>` (uses `/proc/net/tcp*` -> `/proc/<pid>/fd/*` -> `/proc/<pid>/cwd` on Linux with `lsof` fallback)

- [ ] **Step 1: Write the failing tests in `src/routing.rs` (including Review Focus #5)**

```rust
#[test]
fn proc_net_tcp_parser_finds_listening_ipv4_and_ipv6_inodes_and_skips_established_clients() {
    // Review Focus #5: state 0A (TCP_LISTEN) on IPv4 (127.0.0.1:5173 -> 0100007F:1435),
    // IPv6 ::1:5173, and IPv4-mapped ::ffff:127.0.0.1:3000 (0BB8), while ignoring state 01 (TCP_ESTABLISHED).
    let tcp4 = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1435 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 424242 1 0000000000000000 100 0 0 10 0
   1: 0100007F:1435 0100007F:D120 01 00000000:00000000 00:00000000 00000000  1000        0 999999 1 0000000000000000 20 4 30 10 -1
";
    assert_eq!(parse_proc_net_tcp_inodes(tcp4, 5173), vec![424242]);
    assert!(parse_proc_net_tcp_inodes(tcp4, 3000).is_empty());

    let tcp6 = "\
  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:1435 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 555555 1 0000000000000000 100 0 0 10 0
   1: 0000000000000000FFFF00000100007F:0BB8 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 777777 1 0000000000000000 100 0 0 10 0
";
    assert_eq!(parse_proc_net_tcp_inodes(tcp6, 5173), vec![555555]);
    assert_eq!(parse_proc_net_tcp_inodes(tcp6, 3000), vec![777777]);
}

#[test]
fn browser_kind_and_window_title_url_extraction_work_for_linux_browsers() {
    assert!(browser_kind("google-chrome").is_some());
    assert!(browser_kind("firefox").is_some());
    assert!(browser_kind("brave-browser").is_some());
    assert_eq!(
        extract_localhost_url_from_title("localhost:5173/dashboard - Google Chrome").as_deref(),
        Some("http://localhost:5173/dashboard")
    );
    assert_eq!(
        extract_localhost_url_from_title("http://127.0.0.1:3000 — Mozilla Firefox").as_deref(),
        Some("http://127.0.0.1:3000")
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `taskpolicy -c utility cargo test -j 3 --offline proc_net_tcp_parser browser_kind_and_window_title -- --test-threads=2`
Expected: FAIL with `cannot find function parse_proc_net_tcp_inodes` and `cannot find function extract_localhost_url_from_title`.

- [ ] **Step 3: Implement `/proc/net/tcp*` socket-to-cwd resolution and Linux browser URL lookup in `src/routing.rs`**

- Implement `parse_proc_net_tcp_inodes(content: &str, port: u16) -> Vec<u64>`: skip header, split whitespace, check column 3 (`st`) == `"0A"`, parse hex port after `:` in column 1 (`local_address`), and parse column 9 (`inode`) as `u64` (ignoring `0`).
- On Linux, `listener_directory(port: u16)` reads `/proc/net/tcp` and `/proc/net/tcp6`, collects listening inodes for `port`, scans `/proc/<pid>/fd/*` via `std::fs::read_link` for `socket:[<inode>]`, and returns `std::fs::read_link("/proc/<pid>/cwd")` (filtered for `dir != Path::new("/")`), falling back to `lsof` (`/usr/bin/lsof` or `/usr/sbin/lsof`) if `/proc` yields nothing.
- Extend `browser_kind` to recognize Linux `WM_CLASS` names (`google-chrome`, `chromium`, `chromium-browser`, `brave-browser`, `firefox`, `zen`, `vivaldi-stable`, `microsoft-edge`), and implement `extract_localhost_url_from_title` + Linux `browser_tab_url` (checking `web::recent_origin()` first, then scanning the active browser window's `_NET_WM_NAME` title for `http(s)://localhost:<port>...` or bare `localhost:<port>` / `127.0.0.1:<port>`).

- [ ] **Step 4: Run tests to verify they pass**

Run: `taskpolicy -c utility cargo test -j 3 --offline routing:: -- --test-threads=2`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/routing.rs
git commit -m "feat(routing): add Linux /proc/net/tcp* port-to-repo resolver and browser URL detection"
```

---

### Task 9: FFmpeg Video Helper, `tiny-skia` Ink Export & Retention

**Files:**
- Modify: `src/helper.rs:100-293`
- Modify: `src/export/ink.rs:235-320`
- Modify: `src/export/mod.rs:75-115,300-385`
- Modify: `src/retention.rs:335-355,625-635`

**Interfaces:**
- Consumes: `tiny_skia::{Pixmap, Paint, PathBuilder, Stroke, Transform}`, `ffmpeg` / `ffprobe` CLI
- Produces:
  - `src/helper.rs`:
    - `pub fn parse_ffprobe_json(json: &str) -> Result<Probe>`
    - Linux `#[cfg(target_os = "linux")]` implementations of `probe`, `frames`, `shrink`, `export_audio`, `qr`, `devices` (empty `Vec`), `permissions` (all `Status::Granted` when X11 + `cpal` + `ffmpeg` present)
  - `src/export/ink.rs`:
    - `pub fn render_frame_ink(pixmap: &mut tiny_skia::PixmapMut<'_>, plan: &InkPlan, camera: [f32; 4], t_ms: u64)`
  - `src/export/mod.rs`:
    - `pub fn default_output(bundle_dir: &Path) -> PathBuf` (on Linux: `dirs::video_dir().unwrap_or("~/Videos").join("Scrawl/<name>.mp4")`)
  - `src/retention.rs`:
    - `pub fn trash(path: &Path) -> Result<()>` (on Linux: `gio trash <path>` with fallback to moving into `~/.local/share/Trash/files`)

- [ ] **Step 1: Write the failing tests in `src/helper.rs` and `src/export/ink.rs`**

In `src/helper.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffprobe_json_parses_dimensions_fps_duration_and_audio_tracks() {
        let sample = r#"{
            "streams": [
                {"codec_type": "video", "width": 1920, "height": 1080, "r_frame_rate": "30/1", "duration": "12.500"},
                {"codec_type": "audio", "duration": "12.500"}
            ],
            "format": {"duration": "12.500"}
        }"#;
        let probe = parse_ffprobe_json(sample).unwrap();
        assert_eq!((probe.width, probe.height), (1920, 1080));
        assert_eq!(probe.duration_ms, 12_500);
        assert_eq!(probe.audio_tracks, 1);
        assert!((probe.fps - 30.0).abs() < 1e-3);
    }
}
```

In `src/export/ink.rs`:
```rust
#[test]
fn tiny_skia_ink_renderer_draws_visible_red_stroke_and_click_ripple_on_frame() {
    let mut b = bundle();
    b.strokes.push(stroke(200, &[(5000, 5000, 0), (6000, 5000, 100)]));
    b.events.push(InteractionEvent::Click {
        t: 250,
        x: 0.25,
        y: 0.25,
        button: Some("left".into()),
        count: 1,
        modifiers: Vec::new(),
        label: None,
    });
    let plan = plan(&b, (320, 180), Layers { ink: true, clicks: true });
    let mut pixmap = tiny_skia::Pixmap::new(320, 180).unwrap();
    render_frame_ink(&mut pixmap.as_mut(), &plan, [0.0, 0.0, 1.0, 1.0], 200);
    // Midpoint of stroke at y=0.5 (row 90), x=0.55 (col 176) should have red ink:
    let p = pixmap.pixel(176, 90).unwrap();
    assert!(p.red() > 200 && p.alpha() > 200 && p.green() < 80);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `taskpolicy -c utility cargo test -j 3 --offline ffprobe_json_parses tiny_skia_ink_renderer -- --test-threads=2`
Expected: FAIL with `cannot find function parse_ffprobe_json` and `cannot find function render_frame_ink`.

- [ ] **Step 3: Implement `ffprobe`/`ffmpeg` helpers, `render_frame_ink`, and Linux retention trash/priority**

- In `src/helper.rs`: implement `parse_ffprobe_json(json: &str) -> Result<Probe>` and gate `scrawl-helper` subprocess calls with `#[cfg(target_os = "macos")]`. Under `#[cfg(target_os = "linux")]`, implement `probe` (`ffprobe -v quiet -print_format json -show_streams -show_format`), `frames` (`ffmpeg` raw RGBA extraction at timestamps or interval), `shrink` (`nice -n 19 ionice -c 3 ffmpeg -y -i <in> -vf scale=-2:720 -r 15 -c:v libx264 -crf 28 -preset veryfast <out>`), and `export_audio` (`ffmpeg -y -i <movie> -vn -ac 1 -ar 16000 -f wav <wav>`).
- In `src/export/ink.rs`: implement `pub fn render_frame_ink(pixmap: &mut tiny_skia::PixmapMut<'_>, plan: &InkPlan, camera: [f32; 4], t_ms: u64)` drawing active stroke outline polygons (`#ff3b30`), laser trails, pins, and click ripples mapped through the current `camera` rect `[cx0, cy0, cx1, cy1]`.
- In `src/export/mod.rs`: on `#[cfg(target_os = "linux")]`, `default_output` uses `dirs::video_dir().unwrap_or_else(|| home.join("Videos")).join("Scrawl")`, and `run_helper` decodes input frames via `ffmpeg`, crops/scales to the interpolated `camera` rect, calls `ink::render_frame_ink`, and pipes RGBA frames into `ffmpeg -f rawvideo -pix_fmt rgba ... -c:v libx264 -pix_fmt yuv420p` (under `nice -n 19 ionice -c 3` when `background` is `true`).
- In `src/retention.rs`: on `#[cfg(target_os = "linux")]`, implement `trash(path)` using `gio trash` (falling back to moving `path` into `dirs::data_local_dir()/Trash/files/` with a `.trashinfo` file in `Trash/info/`), and implement `lower_priority()` using `unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 19); }`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `taskpolicy -c utility cargo test -j 3 --offline helper:: export:: retention:: -- --test-threads=2`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/helper.rs src/export/ink.rs src/export/mod.rs src/retention.rs
git commit -m "feat(media): add Linux ffprobe/ffmpeg helpers, tiny-skia export compositor, and XDG trash"
```

---

### Task 10: Linux System Tray, GPUI Windows & Onboarding

**Files:**
- Modify: `src/ui.rs:13-35,450-1120,5520-5960`
- Modify: `src/onboarding.rs:640-930`
- Modify: `src/settings.rs:850-1170`

**Interfaces:**
- Consumes: `tray_icon::{TrayIconBuilder, Icon}`, `muda::{Menu, MenuItem, PredefinedMenuItem, MenuEvent}`, `crate::parakeet::{is_model_ready_in, model_dir, ensure_parakeet_model}`
- Produces:
  - `src/onboarding.rs`:
    - `pub struct LinuxReadiness { pub x11_session: bool, pub audio_input: bool, pub ffmpeg_installed: bool, pub parakeet_ready: bool }`
    - `pub fn check_linux_readiness() -> LinuxReadiness`
  - `src/ui.rs`:
    - Cross-platform `install_status_bar() -> Option<flume::Receiver<MenuAction>>` (AppKit `NSStatusBar` on macOS, `tray-icon` + `muda` on Linux)
    - Cross-platform GPUI `WindowOptions` (`app_id: Some("scrawl".into())` on Linux, gating `ns_window` tweaks under `#[cfg(target_os = "macos")]`)

- [ ] **Step 1: Write the failing test for Linux Onboarding readiness and shortcut labels in `src/onboarding.rs` and `src/settings.rs`**

In `src/onboarding.rs`:
```rust
#[test]
fn linux_readiness_maps_to_onboarding_grants() {
    let mut r = LinuxReadiness {
        x11_session: true,
        audio_input: true,
        ffmpeg_installed: true,
        parakeet_ready: false,
    };
    let g = r.to_grants();
    assert!(g.granted(Permission::ScreenRecording));
    assert!(!g.granted(Permission::Microphone), "Microphone row stays ungranted until Parakeet model is ready");
    assert!(g.granted(Permission::Accessibility));

    r.parakeet_ready = true;
    assert!(r.to_grants().granted(Permission::Microphone));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `taskpolicy -c utility cargo test -j 3 --offline linux_readiness_maps_to_onboarding_grants -- --test-threads=2`
Expected: FAIL with `cannot find struct LinuxReadiness`.

- [ ] **Step 3: Implement `LinuxReadiness`, GPUI Onboarding/Settings windows on Linux, and `tray-icon` status bar in `src/onboarding.rs`, `src/settings.rs`, and `src/ui.rs`**

- In `src/onboarding.rs`:
  - Define `LinuxReadiness { pub x11_session: bool, pub audio_input: bool, pub ffmpeg_installed: bool, pub parakeet_ready: bool }` and `impl LinuxReadiness { pub fn to_grants(&self) -> Grants }`:
    - `x11_session` (`std::env::var("DISPLAY").is_ok()`) + `ffmpeg_installed` -> `Permission::ScreenRecording`
    - `audio_input` (`cpal` default input device available) + `parakeet_ready` -> `Permission::Microphone`
    - `x11_session` -> `Permission::Accessibility`
  - On `#[cfg(target_os = "linux")]`, `read_grants()` returns `Some(check_linux_readiness().to_grants())`, and `Input::Grant(Permission::Microphone)` triggers `parakeet::ensure_parakeet_model` in a background thread so the Onboarding checklist downloads `parakeet-tdt-0.6b-v3` in-process with progress feedback before advancing to `Step::TryIt` (`Alt+Shift+S` test capture) and `Step::Done` (`Alt+Shift+V` paste test).
- In `src/settings.rs`:
  - On `#[cfg(target_os = "linux")]`, open file manager via `xdg-open` instead of `/usr/bin/open` in `Effect::ShowCaptures`, and write/remove `~/.config/autostart/scrawl.desktop` in `set_login_item(on: bool)`.
- In `src/ui.rs`:
  - Gate `objc2` imports, `NSStatusBar`, and `NSWindow` window-level helpers under `#[cfg(target_os = "macos")]`.
  - Under `#[cfg(target_os = "linux")]`, implement `install_status_bar()` using `tray-icon` (`assets/menubar-icon@2x.png`) and `muda::Menu` with the exact same `MenuAction` items (`Record Screen`, `Choose What to Record…`, `Open Latest Capture`, `Export Last Capture as Video…`, `Settings…`, `Quit Scrawl`), and set `app_id: Some("scrawl".into())` on GPUI `WindowOptions`.

- [ ] **Step 4: Run full test suite to verify everything compiles and passes**

Run: `taskpolicy -c utility cargo test -j 3 --offline -- --test-threads=2`
Expected: PASS (all existing macOS tests and all new cross-platform/Linux unit tests pass)

- [ ] **Step 5: Commit**

```bash
git add src/ui.rs src/onboarding.rs src/settings.rs
git commit -m "feat(ui): enable GPUI windows, tray-icon menu, and Parakeet onboarding on Linux X11"
```
