#!/usr/bin/env bash
set -euo pipefail

echo "Installing Scrawl build and runtime dependencies for Ubuntu / Zorin OS..."

SUDO=""
if [ "$(id -u)" -ne 0 ]; then SUDO="sudo"; fi
$SUDO apt-get update
$SUDO apt-get install -y \
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
  ffmpeg \
  pkg-config \
  build-essential

echo "Ubuntu dependencies installed successfully."
