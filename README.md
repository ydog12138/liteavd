# liteavd

**English** | [简体中文](README.zh-CN.md)

[![CI](https://github.com/ydog12138/liteavd/actions/workflows/ci.yml/badge.svg)](https://github.com/ydog12138/liteavd/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

liteavd is a local Android multi-device workspace for Linux. It launches,
renders, controls, and diagnoses multiple official Android Virtual Devices in
one native Wayland application. It is designed for Android developers, mobile
QA engineers, and local automation that needs stable, inspectable device
sessions.

liteavd is not a general-purpose replacement for the Android SDK Manager. Its
product is the low-latency embedded viewport, isolated multi-device focus,
exact batch operations, and explainable resource scheduling. Managed SDK and
AVD creation exist to make that workspace usable on a clean Linux machine.

> **Status: pre-alpha / 0.1.0 prerelease.** The scoped product and
> local Flatpak validation are complete, but production stability and backward
> compatibility are not promised. Android Emulator 37.1.11 has a narrowly
> scoped upstream-specific crash under repeated Google Clock timer stress with
> HeadlessSwangle; deterministic continuous audio and the final 30-minute gate
> pass.

## Highlights

- Native GTK4/libadwaita Wayland UI with a responsive one-to-three-column
  device workspace.
- Latest-frame `-share-vid` capture without a visible Emulator window or a
  production Xvfb dependency.
- Separate managed, recovered, and adopted sessions with explicit ownership of
  console ports, process identity, JWT material, and resource reservations.
- Per-session ES256/JWT gRPC control; input, screenshots, snapshots, and audio
  never fall back to unauthenticated gRPC.
- Focused, selected, or all-running operations for screenshots, single/split
  APK installation, ordinary-file push, and exact stop.
- Focused guest speaker output plus an explicit, default-off virtual microphone
  sourced from host input or a PCM WAV file.
- FIFO launch scheduling, memory budgets, and host-GPU slots. HeadlessSwangle
  is the default; DesktopHost is an explicit opt-in policy.
- Adopt an existing SDK or parse Google's repositories, show licenses, and
  install a managed SDK without Java, `sdkmanager`, or `avdmanager`.
- Flatpak-private SDK/AVD storage, minimal permissions, and per-file document
  portal access.

## Quick install

GitHub Releases is the only distribution channel for now; Flathub is deferred.

```bash
sha256sum --check liteavd-0.1.0-x86_64.flatpak.sha256
flatpak install --user ./liteavd-0.1.0-x86_64.flatpak
flatpak run io.github.ydog12138.liteavd
```

The bundle carries a Flathub runtime hint, so Flatpak can obtain the matching
GNOME runtime. The current prerelease targets Linux x86_64 and requires working
`/dev/kvm` access and a Wayland session. The default HeadlessSwangle policy does
not require X11 or Xvfb. See the [installation guide](docs/INSTALLATION.md) for
complete prerequisites, source builds, and narrowly scoped host-SDK access.

## First run

1. Open **Images & Components**, read the required Google license text, and
   explicitly accept it.
2. Install Emulator, Platform Tools, and an x86_64 system image.
3. Create an AVD. HeadlessSwangle is the display-independent default.
4. Start one or more devices and click a card to change focus.
5. Choose focused, selected, or all-running scope before running screenshot,
   APK, file-push, or stop operations.

The [user guide](docs/USER_GUIDE.md) covers device controls, audio and virtual
microphone behavior, snapshots, logs, settings, recovery, and troubleshooting.

## Capability status

| Area | Status | Evidence boundary |
|---|---|---|
| Managed SDK and images | Validated | Repository parsing, license-text hash, Range resume, SHA-1/SHA-256, transactional install, cache quota |
| AVD lifecycle | Validated | Transactional creation, advertisement discovery, JWT recovery, exact stop, process/port/shm cleanup |
| Multi-device viewport/input | Validated | Responsive grid, focus isolation, rotation mapping, single/three-device long gates |
| APK and file deployment | Validated | Single APK, explicit split set, no-clobber push, three-device partial failure/cancel, Flatpak chooser/drop |
| Guest audio output | Validated | Focused-only, `MODE_REAL_TIME`, 10ms callback, 160ms A/V p95, three-device 30-minute gate |
| Virtual microphone | Validated | Explicit host input or PCM WAV, focused-only, default-off, three-device/30-minute/portal gates |
| DesktopHost GPU | Validated | Existing desktop XWayland, required hardware `/dev/dri` evidence, no silent fallback |
| GStreamer/H.264 | Not implemented | The current share-vid path meets the defined gates, so this dependency is not justified yet |
| AAB/APKS/XAPK | Out of initial scope | No Java or bundletool; the first release supports single APK and explicitly selected split APKs |

## Security and privacy boundaries

- Managed gRPC listens on loopback and uses a per-session ES256/JWT identity,
  minimum allowlist, and explicit deadlines.
- Stop verifies the console port, executable identity inside the selected SDK,
  and exact session route; PID liveness alone is never sufficient.
- The Flatpak has no `home` or `host` filesystem permission. Chooser and drop
  operations use document portal grants for exact files.
- Host microphone capture is default-off and non-persistent. Stopping capture
  closes the source; PCM is not persisted in logs or application data.
- liteavd does not bundle Android SDK components. Google license text must be
  shown and explicitly accepted before a managed download starts.

Report vulnerabilities privately according to [SECURITY.md](SECURITY.md).

## Development

```bash
cargo build --locked
cargo test --locked --all-targets
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo +1.88.0 test --locked --no-default-features --lib
```

The default GUI build needs GTK4, libadwaita, and PulseAudio-compatible
development libraries. `protoc` is vendored through a build dependency. Real
Emulator tests are ignored by default and must use an isolated SDK/AVD home,
unique AVD names, and reliable cleanup. See the
[development guide](docs/DEVELOPMENT.md) and [contributing guide](CONTRIBUTING.md).

## Documentation

- [Installation](docs/INSTALLATION.md)
- [User guide](docs/USER_GUIDE.md)
- [Architecture](docs/en/ARCHITECTURE.md) · [中文架构文档](docs/ARCHITECTURE.md)
- [Development](docs/DEVELOPMENT.md)
- [Product definition](docs/en/PRODUCT.md) · [中文产品定义](docs/PRODUCT.md)
- [Validated external facts (Chinese)](docs/VALIDATED_FACTS.md)
- [Flatpak build and sandbox policy](flatpak/README.md)

## License

liteavd is licensed under the [MIT License](LICENSE). Android Emulator,
Platform Tools, and system images remain subject to their respective Google
licenses; liteavd neither relicenses nor bundles those components.
