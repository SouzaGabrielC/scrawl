# Building and Running Scrawl on Linux (Ubuntu / Zorin OS X11)

This guide covers installing dependencies, building, and running Scrawl on Linux (Ubuntu, Zorin OS, and Debian-based systems running on **X11**).

---

## 1. Prerequisites

### Session Type (X11)
The Linux port is designed for **X11** (`$XDG_SESSION_TYPE=x11`). Verify your session type:

```bash
echo $XDG_SESSION_TYPE
```

> **Note:** If your output is `wayland`, log out and select **"Ubuntu on Xorg"** or **"Zorin Desktop on Xorg"** from the session gear menu on the login screen.

---

## 2. Rust Toolchain

If you haven't installed Rust yet:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

---

## 3. System Dependencies

Scrawl relies on system libraries for X11 screen capture, input hooks, audio capture, GPU rendering (HUD pill & laser overlay), system tray, and media helpers.

### Automatic Installation
Run the included dependency script from the repository root:

```bash
./scripts/install-deps-ubuntu.sh
```

### Manual Installation
Alternatively, install packages directly via `apt`:

```bash
sudo apt-get update && sudo apt-get install -y \
  build-essential \
  pkg-config \
  libx11-dev \
  libxcb1-dev \
  libxrandr-dev \
  libxi-dev \
  libxtst-dev \
  libxfixes-dev \
  libxkbcommon-dev \
  libxkbcommon-x11-dev \
  libasound2-dev \
  libgtk-3-dev \
  libayatana-appindicator3-dev \
  libxdo-dev \
  libvulkan-dev \
  vulkan-tools \
  ffmpeg
```

### Package Roles:
- **`libx11-dev`, `libxcb1-dev`, `libxrandr-dev`, `libxi-dev`, `libxtst-dev`, `libxfixes-dev`**: X11 screen capture (`XShm`), global hotkeys, input tap, display geometries, and click-through red ink overlay.
- **`libasound2-dev`**: ALSA audio headers used by `cpal` for microphone recording.
- **`libvulkan-dev`, `vulkan-tools`**: Vulkan graphics drivers for `wgpu` rendering (floating HUD pill and laser overlay).
- **`libgtk-3-dev`, `libayatana-appindicator3-dev`, `libxdo-dev`**: StatusNotifierItem / AppIndicator support for the system tray menu (`tray-icon` + `muda`); `libxdo-dev` provides `-lxdo` for linking `muda`.
- **`ffmpeg`**: Subprocess helper for encoding video and processing audio.

---

## 4. Build and Test

### Run Test Suite
```bash
cargo test -j 3 -- --test-threads=2
```

### Build Release Binary
```bash
cargo build --release
```

The resulting binary will be located at `target/release/scrawl`.

---

## 5. Running Scrawl

Launch the application:

```bash
./target/release/scrawl
```

### Shortcuts & Controls:
- **`Alt+Shift+S`**: Tap to record display under pointer; hold to show target switcher (Screen / Window / Area). Tap again to stop and finish.
- **`Shift` + Mouse Move**: Freehand red ink sketch overlay.
- **`Alt+Shift+V`**: Paste last capture bundle into your frontmost application (OpenChamber GUI or OpenCode terminal).

---

## 6. Speech Transcription on Linux

On Linux, Scrawl runs **Parakeet TDT** (`parakeet-tdt-0.6b-v3`) completely in-process using ONNX Runtime (`ort`).
- **Zero extra daemons or Python setup required**.
- The model weights and tokenizer (~600MB) are automatically downloaded from HuggingFace to `~/.cache/scrawl/models/` during first onboarding or upon recording your first capture.
