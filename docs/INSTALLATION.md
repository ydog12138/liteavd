# Installing liteavd

**English** | [简体中文](INSTALLATION.zh-CN.md)

This guide covers the supported GitHub Flatpak bundle, source builds for
contributors, first-run Android SDK setup, and explicit access to an existing
host SDK.

## 1. Requirements

The 0.1.0 prerelease targets Linux x86_64.

- A Wayland desktop session with GTK4 support.
- Hardware virtualization enabled in firmware and a usable `/dev/kvm`.
- Flatpak for the supported packaged installation.
- Network access when downloading the GNOME runtime or Google SDK components.
- PipeWire-Pulse or PulseAudio for guest speaker output and virtual microphone
  features. Device launch remains available when optional microphone endpoint
  setup cannot be completed.
- XWayland and a working hardware `/dev/dri` renderer only when selecting the
  optional DesktopHost GPU policy.

The default HeadlessSwangle policy does not require a `DISPLAY`, X11, a visible
Emulator window, or Xvfb.

Check KVM before installing:

```bash
test -r /dev/kvm && test -w /dev/kvm
```

If this fails, enable virtualization in firmware and configure the host's KVM
group/ACL policy. Do not grant broad device access to the Flatpak as a shortcut.

## 2. Install the GitHub Flatpak bundle

Download both files for the same version from
[GitHub Releases](https://github.com/ydog12138/liteavd/releases):

- `liteavd-0.1.0-x86_64.flatpak`
- `liteavd-0.1.0-x86_64.flatpak.sha256`

Verify and install:

```bash
sha256sum --check liteavd-0.1.0-x86_64.flatpak.sha256
flatpak install --user ./liteavd-0.1.0-x86_64.flatpak
flatpak run io.github.ydog12138.liteavd
```

The bundle references the matching GNOME runtime on Flathub. If the runtime is
not installed, Flatpak will request it from that remote.

Inspect the sandbox boundary:

```bash
flatpak info --show-permissions io.github.ydog12138.liteavd
```

The expected static permissions are Wayland, explicit X11 for DesktopHost,
PulseAudio, IPC, DRI, KVM, and network. There should be no `home` or `host`
filesystem grant.

## 3. First-run managed SDK

The recommended Flatpak path is a private managed SDK. It is stored below:

```text
~/.var/app/io.github.ydog12138.liteavd/data/liteavd/android-sdk
~/.var/app/io.github.ydog12138.liteavd/data/liteavd/avd
```

In the application:

1. Open **Images & Components**.
2. Select Emulator, Platform Tools, and a system image.
3. Read the displayed Google license text and explicitly accept it.
4. Wait for download, checksum verification, and transactional installation.
5. Create an AVD from the installed image.

Declining, closing, or failing to persist the license decision aborts the
installation. liteavd does not call Java, `sdkmanager`, or `avdmanager` at
runtime.

## 4. Use an existing host SDK

Outside Flatpak, SDK root resolution is:

1. a valid `AVDM_SDK_ROOT` environment override;
2. the saved liteavd setting;
3. the platform default, normally `~/Android/Sdk`.

The AVD home uses `ANDROID_AVD_HOME` or `~/.android/avd`.

Inside Flatpak, host paths are invisible unless they receive an explicit
filesystem override. Grant only the exact SDK and AVD directories:

```bash
flatpak override --user \
  --filesystem="$HOME/Android/Sdk:ro" \
  --filesystem="$HOME/.android/avd" \
  --env=AVDM_SDK_ROOT="$HOME/Android/Sdk" \
  --env=ANDROID_AVD_HOME="$HOME/.android/avd" \
  io.github.ydog12138.liteavd
```

Use a writable SDK grant only when liteavd should install or remove components
there. Review current overrides with:

```bash
flatpak override --user --show io.github.ydog12138.liteavd
```

Reset all per-user overrides with:

```bash
flatpak override --user --reset io.github.ydog12138.liteavd
```

An Emulator already running outside the sandbox is not considered controllable
because Flatpak process isolation prevents the required `/proc` identity
verification.

## 5. Build and run from source

Required build tools:

- Rust 1.88 or newer; the repository `mise.toml` selects the current developer
  toolchain.
- a C/C++ build toolchain and `pkg-config`;
- GTK4 and libadwaita development packages;
- PulseAudio-compatible development libraries;
- Flatpak/flatpak-builder only for packaging.

Common package examples:

```bash
# Arch Linux / CachyOS
sudo pacman -S --needed base-devel rust gtk4 libadwaita libpulse elfutils flatpak flatpak-builder appstream desktop-file-utils

# Ubuntu / Debian family
sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev libasound2-dev libpulse-dev elfutils librsvg2-common
```

Build and run:

```bash
git clone https://github.com/ydog12138/liteavd.git
cd liteavd
cargo build --locked
cargo run --locked
```

For core-only development without GTK:

```bash
cargo test --locked --no-default-features --lib
```

## 6. Build the Flatpak locally

Add Flathub if necessary, then build and install:

```bash
flatpak remote-add --user --if-not-exists flathub \
  https://dl.flathub.org/repo/flathub.flatpakrepo

flatpak-builder \
  --user \
  --force-clean \
  --install-deps-from=flathub \
  --install \
  build/flatpak \
  io.github.ydog12138.liteavd.yml
```

The build is offline after Flatpak resolves the sources listed in
`flatpak/cargo-sources.json`. Regenerate that file after any `Cargo.lock`
change; see [DEVELOPMENT.md](DEVELOPMENT.md).

## 7. Update and uninstall

Install a newer downloaded bundle with the same `flatpak install --user`
command. Remove only the application while retaining private data:

```bash
flatpak uninstall --user io.github.ydog12138.liteavd
```

Remove the application and its private data:

```bash
flatpak uninstall --user --delete-data io.github.ydog12138.liteavd
```

The second command permanently deletes the managed SDK, AVDs, settings, logs,
and cache in the Flatpak-private directory. Back up any required AVD data first.

## 8. Installation troubleshooting

### `/dev/kvm` is unavailable

Fix host firmware/KVM permissions. The application intentionally does not fall
back to unaccelerated emulation.

### DesktopHost refuses to start

DesktopHost requires a non-empty XWayland `DISPLAY`, access to `/dev/dri`, and
evidence that the Emulator opened a hardware renderer. Use HeadlessSwangle on a
truly headless host.

### An inherited SDK path is rejected in Flatpak

The path exists on the host but is not visible in the sandbox. Apply an exact
override as shown above or use the private managed SDK.

### Audio or virtual microphone is unavailable

Confirm that the host PulseAudio-compatible service is running and that the
Flatpak retains its `pulseaudio` socket permission. Adopted sessions without
liteavd's private JWT identity cannot use these controlled streams.

### File chooser access works but arbitrary host paths do not

This is expected. File chooser and drag-and-drop access exact portal-exported
files; liteavd intentionally has no broad home filesystem permission.
