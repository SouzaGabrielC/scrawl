# Linux (Ubuntu / Zorin OS X11) Support Design Specification

**Date:** 2026-10-19
**Repository:** `/Users/gabriel/Documents/github/scrawl` (fork of `kitlangton/scrawl`)
**Target Environment:** Ubuntu-based Linux desktops (specifically Zorin OS / GNOME Shell) running on **X11** (`$XDG_SESSION_TYPE=x11`)

---

## 1. Intent & Why

Scrawl turns visual feedback into compact, LLM-ready context bundles (`.scrawl`) by letting a developer hold a global shortcut (`Alt+Shift+S`), point and draw freehand in red (`Shift` + mouse move) over any screen or window while explaining the issue out loud, and then paste (`Alt+Shift+V`) or auto-deliver a compiled bundle (`contact-sheet.png`, `frames/frame-XX.png`, `summary.md`, `scrawl.json`, and optional `recording.mp4`) directly into an AI coding agent.

Today, Scrawl's core data model, sketch geometry, keyframe selection, autocrop, contact-sheet renderer, audio speech-island detector, localhost web server, and agent delivery adapters (~45% of the codebase) are already platform-independent Rust. However, the build pipeline (`Cargo.toml`, `build.rs`) and the OS integration layer (`capture.rs`, `windows.rs`, `screens.rs`, `laser_overlay.rs`, `native/pill/*`, `transcribe.rs`, `handoff.rs`, `routing.rs`, `helper.rs`, `export/mod.rs`, `retention.rs`) are tightly coupled to macOS frameworks (`ScreenCaptureKit`, `Carbon`, `ApplicationServices`, `AppKit`, `Metal`, `CoreText`, `AVFoundation`, and the Swift `scrawl-helper` binary).

The goal of this project is to make Scrawl a first-class Linux desktop application on Ubuntu/Zorin OS (X11) with full capability parity with the macOS version, tailored for seamless handoff into **OpenChamber** (GUI wrapper on top of OpenCode) and **OpenCode TUI** (terminal), while running **Parakeet** (`parakeet-tdt-0.6b-v3`) speech-to-text completely in-process with zero external runtime dependencies.

---

## 2. Goals, Non-Goals, and Anti-Goals

### Goals
1. **Full Capability Parity on Linux (X11)**:
   - Hold-to-record and tap-to-latch global hotkey (`Alt+Shift+S` default, user-configurable) and paste-last-capture global hotkey (`Alt+Shift+V` default).
   - High-frequency cursor tracking (`mouse.json`), clicks (`clicks.json`), keystrokes (`keys.json`), and scroll wheel events (`scrolls.json`).
   - Hold-`Shift` freehand red ink sketching (`strokes.json`) with live click-through fullscreen GPU shader overlay (`laser_overlay.rs`) that hides the system cursor while drawing and restores it on release.
   - Top-center floating HUD pill ("Dynamic Island" style in `native/pill/*`) with live microphone waveform bars, active mode badge, recording timer, transcription progress spinner, and completion preview thumbnail.
   - System tray icon and menu (`tray-icon` + `muda` via `libayatana-appindicator3` / StatusNotifierItem) for Record, Review, Settings, Onboarding, and Quit.
   - Native GPUI windows (`ui/review.rs`, `ui/settings.rs`, `ui/onboarding.rs`) running on Linux X11 (`gpui = "0.2.2"`).
   - High-accuracy local speech transcription using **Parakeet (`parakeet-tdt-0.6b-v3` ONNX)** downloaded and executed directly inside the Scrawl Rust binary (`ort`), producing native word-level timestamps (`WordTiming` with `estimated: false`) for spoken-cue keyframe extraction ("now", "here", "look", "watch").
   - Rich clipboard + synthetic paste handoff (`handoff.rs`) supporting both **OpenChamber (GUI)** via `Ctrl+V` and **OpenCode TUI (terminal)** via `Ctrl+Shift+V`, plus file/image clipboard targets (`image/png`, `text/uri-list`, `UTF8_STRING`).
   - Localhost dev-server routing (`routing.rs`) via `/proc/net/tcp*` + `/proc/<pid>/cwd` inspection and `/sdk.js` DOM element breadcrumbs.
   - Optional `.mp4` screen recording during capture and `scrawl export --zoom` spring-physics MP4 re-export (`export/mod.rs`), plus background retention cleanup (`retention.rs`).

### Non-Goals
1. **Apple Hardware-Specific Features**:
   - USB cabled iPhone mirroring (`CoreMediaIO` in `helper/main.swift`) is macOS/iOS hardware-specific and remains `#[cfg(target_os = "macos")]` (or hidden in the Linux UI).
   - Apple `SpeechAnalyzer` (`macOS 26+` system speech framework) is macOS-specific; on Linux, **Parakeet (`Engine::Parakeet`)** is the primary default engine, with `whisper-rs` (`Engine::Whisper`) retained as a secondary fallback option.
   - Sparkle (`sparkle.rs`) macOS `.app` bundle auto-updater remains macOS-only; Linux uses standard package/binary updates.
2. **Wayland Compositor Portal Multiplexing in v1**:
   - While GPUI and several crates support Wayland, X11 is the primary display server target for this design (`$XDG_SESSION_TYPE=x11` on Zorin OS). Wayland-specific portals (`xdg-desktop-portal` PipeWire screencast + `libei`) are out of scope for this phase so X11 window/cursor/hotkey/overlay behavior can be rock-solid without portal permission popups on every capture.

### Anti-Goals
1. **No External Daemon, Python, or Sidecar App Dependencies for Transcription**:
   - Scrawl MUST NOT require the user to install Python, `sherpa-onnx` CLI binaries, Ollama, or a local HTTP transcription server. Model weights (`parakeet-tdt-0.6b-v3`) are downloaded directly by Scrawl from HuggingFace and inferred in-process via ONNX Runtime (`ort`).
2. **No Fork Divergence of Portable Core**:
   - Modules that are already portable (`model.rs`, `sketch.rs`, `keyframes.rs`, `render.rs`, `zoom.rs`, `autocrop.rs`, `audio.rs`, `web.rs`, `inbox.rs`, `delivery.rs`) must remain shared across macOS and Linux rather than duplicated.

---

## 3. Constraints

1. **Display Server & Coordinate System**:
   - Target session is X11 on Ubuntu/Zorin OS (GNOME Shell / Muffin / Mutter X11).
   - X11 uses a **top-left origin (`Y-down`)** globally across RandR monitors, EWMH window geometry, and `XInput2` root pointer coordinates. Unlike macOS (`NSScreen` bottom-left `Y-up` vs `CGWindow` top-left `Y-down`), `screens::flip` on Linux is an identity transform (no vertical inversion needed).
2. **Self-Contained Transcription Lifecycle**:
   - Model files must be stored under the XDG data directory (`~/.local/share/scrawl/models/parakeet-tdt-0.6b-v3/`).
   - If the model is not yet downloaded when the user records or opens Onboarding/Settings, Scrawl downloads it in-process with progress feedback in the UI/pill and caches the loaded `ort::Session` across captures in daemon mode.
3. **Target Agent Paste Semantics**:
   - On Linux X11, GUI apps (including **OpenChamber**, Electron/WebKit/GTK apps) paste on `Ctrl+V`, whereas terminal emulators running **OpenCode TUI** (`gnome-terminal`, `ptyxis`, `kitty`, `alacritty`, `wezterm`, `foot`, `ghostty`, `konsole`, `tilix`, `xfce4-terminal`, `xterm`) intercept `Ctrl+V` as literal-next (`^V`) and require `Ctrl+Shift+V` to paste clipboard text.
   - X11 clipboard ownership is asynchronous (the selection owner must stay alive to serve `SelectionRequest` events from the target app). Because Scrawl runs as a persistent daemon (`scrawl daemon`), its clipboard manager (`arboard::Clipboard`) must remain alive in the daemon process (or for CLI one-shot commands, stay alive briefly until paste completes).

---

## 4. User-Decided Architecture

### 4.1 Build System (`Cargo.toml` & `build.rs`)
- **`Cargo.toml`**:
  - Move `gpui = "0.2.2"` from `[target.'cfg(target_os = "macos")'.dependencies]` to shared `[dependencies]` (or enable for both `macos` and `linux`), as `gpui` 0.2.2 has native Linux X11/Wayland support.
  - Keep Apple-only crates (`screencapturekit`, `core-foundation`, `core-graphics`, `objc2*`, `block2`, `dispatch2`, `metal`, `foreign-types`) under `[target.'cfg(target_os = "macos")'.dependencies]`.
  - Add Linux dependencies under `[target.'cfg(target_os = "linux")'.dependencies]`:
    - `x11rb` (with `randr`, `shm`, `xfixes`, `shape`, `xinput`, `xtest`, `render`, `composite` features) for X11 display, window, input, and overlay management.
    - `wgpu` + `raw-window-handle` + `bytemuck` for GPU-accelerated WGSL shaders in the laser overlay and floating HUD pill.
    - `cosmic-text` (or `ab_glyph`) + `tiny-skia` for pure-Rust text glyph rasterization in `src/native/pill/atlas.rs` and `src/export/ink.rs`.
    - `arboard` for X11 clipboard text and PNG image support.
  - Add shared or Linux-enabled transcription dependency:
    - `ort` (ONNX Runtime with CPU execution provider and optional CUDA/OpenVINO feature flags) + `ndarray` for in-process `parakeet-tdt-0.6b-v3` inference.
- **`build.rs`**:
  - Retain Metal shader compilation (`xcrun metal`) and Swift helper compilation (`xcrun swiftc`) strictly inside `if target_os == "macos"`.
  - On Linux (`target_os == "linux"`), `build.rs` does not invoke `xcrun`; WGSL shaders (`laser_overlay.wgsl`, `pill.wgsl`) are embedded via `include_str!` and validated/compiled at runtime by `wgpu`.

### 4.2 Display & Window Introspection (`src/screens.rs`, `src/windows.rs`)
- **`src/screens.rs` (RandR + EWMH Workarea)**:
  - Query active monitors via X11 RandR (`x11rb::protocol::randr::get_screen_resources_current` + `get_output_info` + `get_crtc_info`, or `get_monitors` on RandR 1.5).
  - Map each monitor to `ScreenInfo`:
    - `id`: RandR output/CRTC ID.
    - `frame`: `(x, y, width, height)` in X11 root coordinates.
    - `visible_frame`: intersect monitor `frame` with EWMH `_NET_WORKAREA` (or per-window `_NET_WM_STRUT_PARTIAL` dock/panel struts) so the top bar / Zorin taskbar are respected when positioning the HUD pill.
    - `scale`: derived from desktop DPI (`Xft.dpi` in `RESOURCE_MANAGER` / `GDK_SCALE` / physical mm dimensions, defaulting to `1.0` or `2.0` on HiDPI).
    - `notch_width`: `0.0` on Linux (so the HUD pill uses the standard non-notch top-center hover zone).
  - `screens::flip(y, h)`: returns `y` unchanged on Linux since X11 is already top-left (`Y-down`).
- **`src/windows.rs` (EWMH Stacking + Window Properties)**:
  - Query stacking order via EWMH `_NET_CLIENT_LIST_STACKING` on the root window (iterated back-to-front, reversed to front-to-back to match `CGWindowListCopyWindowInfo`).
  - For each client window, resolve:
    - Geometry via `GetGeometry` + `TranslateCoordinates` to root space (accounting for frame extents `_NET_WM_EXTENTS` / `_GTK_FRAME_EXTENTS`).
    - Visibility via `GetWindowAttributes` (`map_state == MapState::VIEWABLE`) and `_NET_WM_STATE` (skipping `_NET_WM_STATE_HIDDEN`).
    - Title via `_NET_WM_NAME` (`UTF8_STRING`) with fallback to `WM_NAME`.
    - Owner app name via `WM_CLASS` (`res_class` / `res_name`).
    - Owner PID via `_NET_WM_PID`, and executable/process info via `/proc/<pid>/comm` and `/proc/<pid>/cmdline`.
  - `frontmost_Application()`: reads `_NET_ACTIVE_WINDOW` from the root window and resolves its `WM_CLASS`, `_NET_WM_NAME`, and `_NET_WM_PID` into `FrontmostApp { name, bundle_id, pid }` (where `bundle_id` on Linux is the normalized lowercase `WM_CLASS` class name, e.g., `"openchamber"`, `"code"`, `"org.gnome.terminal"`).
  - `activate_app(pid, bundle_id)`: sends an EWMH `_NET_ACTIVE_WINDOW` client message to the root window for the target window matching `pid` or `WM_CLASS`.

### 4.3 Global Hotkeys, Input Stream & Screen Capture (`src/capture.rs`)
- **Global Hotkeys (`XGrabKey` + `XInput2` Key State)**:
  - Parse shortcut strings (`"Alt+Shift+S"`, `"Alt+Shift+V"`, `"Super+Shift+S"`) into X11 keysyms/keycodes and modifier masks.
  - Register passive key grabs on the X11 root window using `GrabKey`, grabbing all 4 lock-modifier permutations (`0`, `Mod2Mask` NumLock, `LockMask` CapsLock, `Mod2Mask | LockMask`) so hotkeys work reliably regardless of CapsLock/NumLock state.
  - Maintain the existing dual hold/latch gesture semantics (`HOLD_THRESHOLD = 350ms`):
    - Quick tap (< 350ms) latches recording on; pressing the hotkey again (or `Enter`) stops and finalizes; pressing `Escape` cancels.
    - Hold (>= 350ms) records while held; releasing the hotkey stops and finalizes.
- **Passive Input Tracking (`XInput2` Raw Events + 120 Hz Poller)**:
  - Subscribe to `XI_RawMotion`, `XI_RawButtonPress`, `XI_RawButtonRelease`, and `XI_RawKeyPress` on `XIAllMasterDevices` on the root window.
  - Poll pointer position + modifier state at 120 Hz (`XQueryPointer` / `XQueryKeymap`) in parallel with raw event listeners, identical to the macOS architecture:
    - Normal movement appends to `mouse.json`.
    - Button 1/2/3 press/release appends to `clicks.json`; Button 4/5/6/7 wheel events append to `scrolls.json`.
    - Key presses (when not drawing) translate keycodes via X11 keyboard mapping (`GetKeyboardMapping` / `xkbcommon`) into `KeyEvent` entries in `keys.json`.
- **Hold-`Shift` Freehand Red Drawing & Cursor Hiding**:
  - When `Shift` is held (`ShiftMask` in `XQueryPointer` mask) and the user moves the pointer (> 6 px threshold) without typing shifted characters:
    - Transition to drawing mode, hide the system cursor globally via `XFixesHideCursor` on the root window, and stream normalized stroke points to `strokes.json` and the live `laser_overlay` window.
    - When `Shift` is released, commit the `Stroke` and call `XFixesShowCursor` to restore the pointer immediately.
- **Screen Frame Capture (`XShmGetImage`)**:
  - Use `x11rb::protocol::shm` (`XShmGetImage` into shared memory segments, with `GetImage` fallback) to capture raw BGRA frames of the target screen at 30–60 fps with minimal CPU overhead.
  - Downscale/encode frames for live review and optional `.mp4` recording (`recording.mp4`), feeding the exact same `keyframes.rs`, `autocrop.rs`, and `render.rs` pipeline used on macOS.

### 4.4 GPU Overlays: Click-Through Red Ink & Dynamic Island Pill (`src/laser_overlay.rs`, `src/native/pill/*`)
- **Click-Through Fullscreen Ink Overlay (`src/laser_overlay.rs` + `src/laser_overlay.wgsl`)**:
  - Create a borderless, override-redirect 32-bit ARGB X11 window covering the active monitor (`_NET_WM_STATE_ABOVE`, `_NET_WM_STATE_STICKY`, `_NET_WM_WINDOW_TYPE_UTILITY`).
  - Set an **empty input region** using the X11 Shape extension (`xfixes::set_window_shape_region` / `shape::rectangles` with `shape::Sk::INPUT` and 0 rectangles) so 100% of mouse clicks, scrolls, and drags pass straight through to the underlying desktop/apps.
  - Port `src/laser_overlay.metal` (~142 lines) 1:1 to `src/laser_overlay.wgsl` (tldraw `getStrokeOutlinePoints` polygon + SDF rounded caps + 2.5s fade-out + synthetic red pen nib cursor at the live pointer position when the system cursor is hidden) rendered via `wgpu` with `CompositeAlphaMode::PreMultiplied`.
- **Top-Center Dynamic Island HUD Pill (`src/native/pill/*` + `src/native/pill/pill.wgsl`)**:
  - Create an always-on-top, borderless 32-bit ARGB X11 window positioned at the top center of the primary monitor (`_NET_WM_STATE_ABOVE`, `override_redirect = true`).
  - Port `src/native/pill/pill.metal` (~574 lines: SDF rounded pill morphing, spring animations, audio spectrum bars, spinner ring, hover highlights, and glyph quad sampling) 1:1 to `src/native/pill/pill.wgsl` rendered via `wgpu`.
  - Dynamically update the X11 `ShapeInput` region to match the current expanded/collapsed pill bounding box so clicks on the pill buttons (Stop, Cancel, Mode switcher, Review) register on the pill while clicks outside the pill pass through to the desktop.
  - Replace `CoreText` in `src/native/pill/atlas.rs` with pure-Rust font rasterization (`cosmic-text` / `ab_glyph` loading system sans-serif / monospace fonts via `fontdb`) into the single-channel alpha texture atlas.

### 4.5 Self-Contained In-Process Parakeet Transcription (`src/transcribe.rs`)
- **Engine Addition**:
  - Add `Engine::Parakeet` (`"parakeet"`) to `src/transcribe.rs` and set it as the **default engine on Linux** (`Engine::default()` returns `Engine::Parakeet` on Linux, while retaining `Engine::Whisper` as an alternative in Settings).
- **Direct Model Download & Storage**:
  - Store `parakeet-tdt-0.6b-v3` ONNX model artifacts under `~/.local/share/scrawl/models/parakeet-tdt-0.6b-v3/`:
    - `encoder-model.int8.onnx` (or `model.onnx` / quantized int8 variant for fast CPU inference)
    - `decoder_joint-model.int8.onnx`
    - `vocab.txt` (SentencePiece / BPE token vocabulary)
  - Implement `ensure_parakeet_model(progress_cb)` in Rust using `ureq` / `reqwest` to download the files directly from HuggingFace (`https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main/...`) with atomic `.part` file rename and byte-progress callbacks wired to the Onboarding window, Settings window, and HUD pill status text.
- **In-Process Inference & Word-Level Timestamps**:
  - Compute 80-bin log-mel filterbank features (16 kHz mono audio, 25ms window, 10ms hop) directly in pure Rust from the recorded audio buffer (processed through `audio::speech_islands` just like Whisper/Apple Speech so silent gaps are skipped and island time offsets are preserved).
  - Run the Token-and-Duration Transducer (TDT) greedy decoding loop via `ort::Session` in-process:
    - TDT jointly predicts token IDs and frame durations (`0..=4` frames per step).
    - Accumulate subword tokens into words and convert encoder frame indices (each encoder subsampling frame = 80ms, i.e., 8 × 10ms hop) + speech-island offsets into exact `WordTiming { word, start_ms, end_ms, confidence, estimated: false }`.
  - Cache the loaded Parakeet `ort::Session` in a global `OnceLock<Mutex<Option<ParakeetModel>>>` inside the daemon process so subsequent captures transcribe in ~100–300ms without reloading weights from disk.

### 4.6 Agent Handoff: OpenChamber (GUI) & OpenCode TUI (`src/handoff.rs`)
- **XDG Captures Directory**:
  - On Linux, `default_captures_dir()` resolves to `dirs::data_local_dir().unwrap_or("~/.local/share").join("scrawl/captures")` (overridable via `SCRAWL_CAPTURES_DIR` or Settings).
- **Multi-Target X11 Clipboard (`arboard`)**:
  - `copy_bundle_to_clipboard(bundle_dir)`: writes the formatted prompt text (`@contact-sheet.png` + transcript + keyframe paths + dev-server source locations) to the X11 clipboard.
  - `copy_png_to_clipboard(png_path)`: decodes the PNG (`image` crate) and sets RGBA image data on the X11 clipboard via `arboard::Clipboard::set_image`.
  - Keep a persistent `Mutex<Option<arboard::Clipboard>>` singleton inside the running process so X11 `CLIPBOARD` selection requests continue to be served after the copy call returns.
- **Smart Synthetic Paste (`Ctrl+V` vs `Ctrl+Shift+V`)**:
  - Update `is_frontmost_terminal()` to recognize Linux terminal `WM_CLASS` / process names: `gnome-terminal`, `gnome-terminal-server`, `ptyxis`, `kitty`, `alacritty`, `wezterm`, `foot`, `ghostty`, `konsole`, `tilix`, `xfce4-terminal`, `xterm`, `urxvt`, `terminator`, `warp`, `rio`, `hyper`, `tabby`.
  - Recognize **OpenChamber** (`openchamber`) and GUI IDEs (`code`, `cursor`, `zed`, `windsurf`) as GUI targets.
  - Implement `post_synthetic_paste(is_terminal: bool)` on Linux using X11 `XTest` (`x11rb::protocol::xtest::fake_input`):
    - First release any currently held physical modifier keys (`Alt`, `Shift`, `Super`, `Control`) via synthetic key release or wait up to 150ms for physical key release so `Alt+Shift+V` modifiers don't collide with the synthetic paste chord.
    - If `is_frontmost_terminal()` is `false` (e.g., **OpenChamber GUI**): synthesize `Control_L` down -> `v` down -> `v` up -> `Control_L` up (`Ctrl+V`).
    - If `is_frontmost_terminal()` is `true` (e.g., **OpenCode TUI** in terminal): synthesize `Control_L` down -> `Shift_L` down -> `v` down -> `v` up -> `Shift_L` up -> `Control_L` up (`Ctrl+Shift+V`).
  - In `paste_bundle_image_and_text(bundle_dir)`:
    - For **OpenCode TUI** (terminal): copy the text prompt (which includes absolute file paths to `contact-sheet.png`, `frames/*.png`, and `summary.md` that OpenCode reads from disk) and send `Ctrl+Shift+V`.
    - For **OpenChamber GUI** (and other GUI apps): copy `contact-sheet.png` image to clipboard, send `Ctrl+V`, wait 120ms, copy the text prompt to clipboard, and send `Ctrl+V` (matching the two-step image + text paste flow on macOS).

### 4.7 Localhost Dev-Site Routing (`src/routing.rs`)
- **`/sdk.js` Browser Bridge**:
  - Works unchanged on Linux: any localhost web app loading `<script src="http://127.0.0.1:4765/sdk.js"></script>` streams hovered DOM selectors, React/Vue component names, and page URLs to `src/web.rs`.
- **Browser URL & Port Detection on Linux**:
  - When a browser window (`chrome`, `chromium`, `brave`, `firefox`, `zen`, `vivaldi`, `edge`) is focused or under the cursor, extract `localhost:<port>` / `127.0.0.1:<port>` from:
    1. Any recent `/sdk.js` telemetry ping (`web.rs`), or
    2. The X11 window title (`_NET_WM_NAME`), or
    3. Optional AT-SPI2 / browser accessibility address-bar lookup if available.
- **Port-to-Repo Resolution (`listener_directory(port)`)**:
  - Implement a native Linux `/proc` resolver in `src/routing.rs`:
    1. Parse `/proc/net/tcp` and `/proc/net/tcp6` for sockets in state `0A` (`TCP_LISTEN`) matching `local_address` port == `port`, extracting the socket `inode`.
    2. Scan `/proc/<pid>/fd/*` symlinks for `socket:[<inode>]` to find the owning `pid`.
    3. Read `/proc/<pid>/cwd` via `std::fs::read_link` to obtain the dev server's working directory directly (faster and more reliable than spawning `lsof`).
    4. Keep `/usr/bin/lsof -nP -iTCP:<port> -sTCP:LISTEN -Fpcn` as a fallback if `/proc` permissions restrict inspection.

### 4.8 Video Helper, MP4 Export & Retention (`src/helper.rs`, `src/export/mod.rs`, `src/retention.rs`)
- **Replacing `scrawl-helper` on Linux (`src/helper.rs`, `src/export/mod.rs`)**:
  - On macOS, `scrawl-helper` (Swift) uses `AVFoundation` for video metadata probing, frame extraction, background video shrinking, and `export` video encoding.
  - On Linux, implement these four operations in `src/helper.rs` and `src/export/mod.rs` using `ffmpeg` / `ffprobe` (standard on Ubuntu/Zorin OS, with clear diagnostic check in Onboarding/Settings) + pure-Rust frame rendering:
    - `probe_video(path)`: runs `ffprobe -v quiet -print_format json -show_streams` to read width, height, duration, and FPS.
    - `extract_frames(video, timestamps_ms)`: decodes requested frame timestamps via `ffmpeg` raw image pipe (or directly from the in-memory frame buffer captured during `capture.rs`).
    - `shrink_video(input, output)`: runs `ffmpeg -y -i <input> -vf scale=-2:720 -r 15 -c:v libx264 -crf 28 -preset veryfast <output>` under `nice -n 19 ionice -c 3` (replacing macOS `/usr/sbin/taskpolicy -c utility` in `src/retention.rs`).
    - `export::run(opts)`: computes spring-physics camera path (`export/camera.rs`) and autocrop (`export/crop.rs`), composites red ink strokes (`export/ink.rs` using `tiny-skia` on Linux instead of `CoreGraphics`), and pipes composited RGBA frames into `ffmpeg -f rawvideo -pix_fmt rgba ... -c:v libx264 -pix_fmt yuv420p` to produce the zoomed `.mp4` in `dirs::video_dir().unwrap_or("~/Videos").join("Scrawl")`.

### 4.9 GPUI Windows, Tray & Onboarding (`src/ui/*`, `src/tray.rs`)
- **GPUI Linux Windowing (`src/ui/mod.rs`, `review.rs`, `settings.rs`, `onboarding.rs`)**:
  - Remove `#[cfg(target_os = "macos")]` gates around `ui::*` in `main.rs` and `ui/mod.rs`.
  - Guard only the raw `ns_window` (`objc2_app_kit::NSWindow`) window-level tweaks inside `#[cfg(target_os = "macos")]`; on Linux, use standard GPUI `WindowOptions` (`WindowKind::PopUp` or `WindowKind::Normal` with `app_id: Some("scrawl".into())`).
- **Linux Onboarding (`src/ui/onboarding.rs`)**:
  - On macOS, Onboarding prompts for Screen Recording, Microphone, Accessibility, and Input Monitoring TCC permissions.
  - On Linux X11, X11 does not require TCC permission popups for screen capture or input monitoring. Adapt the Linux Onboarding steps to:
    1. **System Readiness Check**: Verify X11 session, audio input device (`cpal`), and `ffmpeg` availability.
    2. **Parakeet Model Setup**: Download `parakeet-tdt-0.6b-v3` in-process with a live progress bar (or verify it is already cached).
    3. **Interactive Test Capture**: Hold `Alt+Shift+S`, draw with `Shift`, speak a test phrase, and preview the generated bundle.
    4. **Agent Paste Test**: Test pasting into OpenChamber or OpenCode TUI with `Alt+Shift+V`.

---

## 5. What Is Left to the Builder (Builder Technical Calls)

The following implementation details are delegated to the builder during planning and execution:
1. **Exact `ort` Crate Version & Mel Spectrogram Implementation**: Choosing the exact `ort` 2.0.x version and implementing the 80-bin log-mel spectrogram (using `rustfft` / `realfft` + Hann window + Slaney mel scale matching NeMo `parakeet-tdt-0.6b-v3` preprocessor config).
2. **HuggingFace ONNX Repository Layout**: Using `istupakov/parakeet-tdt-0.6b-v3-onnx` (or equivalent standard exported NeMo TDT ONNX artifacts on HuggingFace) and structuring the tokenizer/decoder step loop.
3. **Internal Module Organization**: Deciding whether platform-specific implementations in `screens.rs`, `windows.rs`, `capture.rs`, `laser_overlay.rs`, and `native/pill/mod.rs` live in inline `#[cfg(target_os = "linux")]` blocks or submodule files (e.g., `src/linux/*.rs`).
4. **System Package Dependencies List**: Documenting the standard Ubuntu/Zorin `apt` build dependencies (`libx11-dev`, `libxcb1-dev`, `libxrandr-dev`, `libxi-dev`, `libxtst-dev`, `libxfixes-dev`, `libxkbcommon-dev`, `libxkbcommon-x11-dev`, `libasound2-dev`, `libgtk-3-dev`, `libayatana-appindicator3-dev`, `vulkan-tools` / `libvulkan-dev`, `ffmpeg`) in a build script / README section.
