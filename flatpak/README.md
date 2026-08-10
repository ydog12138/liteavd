# Flatpak build and sandbox policy

[简体中文](README.zh-CN.md)

The repository manifest is `io.github.ydog12138.liteavd.yml`. It targets the
GNOME 50 runtime and uses the matching Freedesktop 25.08 Rust extension. Cargo
dependencies are generated from `Cargo.lock` and fetched before the sandboxed,
offline build.

## Local build

```bash
flatpak-builder \
  --user \
  --force-clean \
  --install-deps-from=flathub \
  --install \
  build/flatpak \
  io.github.ydog12138.liteavd.yml

flatpak run io.github.ydog12138.liteavd
flatpak info --show-permissions io.github.ydog12138.liteavd
```

Static metadata checks also run in CI:

```bash
desktop-file-validate data/io.github.ydog12138.liteavd.desktop
appstreamcli validate --no-net data/io.github.ydog12138.liteavd.metainfo.xml
flatpak-builder --show-manifest io.github.ydog12138.liteavd.yml >/dev/null
```

The manifest uses the current checkout as a `dir` source. The public repository
is the source of tagged checkouts exported as single-file bundles through
GitHub Releases. Release artifacts are accepted only after the workflow's
install and checksum checks and a maintainer's draft review.

Regenerate Cargo sources after every `Cargo.lock` change with the official
`flatpak-builder-tools` cargo generator:

```bash
python3 flatpak-cargo-generator.py Cargo.lock -o flatpak/cargo-sources.json
```

## GitHub Releases

Flatpak is the only package format. For the current pre-alpha phase, GitHub
Releases is the distribution channel; Flathub submission is deferred. Build a
versioned bundle and checksum locally with:

```bash
flatpak/build-bundle.sh 0.1.0
flatpak install --user dist/liteavd-0.1.0-x86_64.flatpak
```

The version must match both `Cargo.toml` and the newest AppStream release. A
`v*` tag starts `.github/workflows/release-flatpak.yml`, rebuilds the bundle on
Ubuntu 24.04, verifies an install and checksum, then creates a draft prerelease
containing the `.flatpak` and `.sha256`. Re-running the same tag may refresh an
existing draft, but it will refuse to overwrite a published release. The owner
inspects the draft before publishing it.

A single-file bundle does not include the GNOME runtime and does not act as an
AppStream repository. The embedded runtime-repository hint lets Flatpak obtain
the required runtime from Flathub during installation. A future Flathub
submission must instead use a checksum-pinned public release archive and pass
Flathub's metadata review.

## Persistence and permissions

The default sandbox has no host filesystem grant. Flatpak supplies private XDG
directories below `~/.var/app/io.github.ydog12138.liteavd/`; liteavd places its
managed SDK and AVDs below `data/liteavd/android-sdk` and `data/liteavd/avd`,
with settings, cache, logs, and recovery state in their corresponding private
XDG locations.

The static permissions are limited to:

- native Wayland for GTK, explicit X11 for the Emulator's opt-in desktop-host
  XWayland path, and shared IPC for X11 performance;
- the standard PulseAudio socket for playing the focused managed guest's
  speaker stream and, only after an explicit per-device action, feeding the
  focused guest's private virtual-microphone source;
- DRI for GPU-accelerated GTK rendering and validated Emulator host rendering;
- `/dev/kvm` for the Android Emulator;
- network access for Google downloads and localhost console/adb/gRPC sockets.

There is deliberately no `home`, `host`, host `/dev/shm`, USB, input-device,
or D-Bus wildcard permission. Host input is never opened implicitly: the
virtual microphone defaults off, is not persisted, and requires the user to
select the focused managed/recovered device and enable a host-input or WAV
source. Managed Emulator children and liteavd share the sandbox's private
`/dev/shm`; the virtual-microphone FIFO uses Flatpak's app-specific shared
runtime directory because the host Pulse daemon cannot see a mount-namespace-
private auth directory. Xvfb is not bundled and is not
required by either managed GPU policy. The default headless swangle policy
needs no display; desktop host inherits the session's XWayland `DISPLAY` and
fails visibly if hardware renderer evidence is unavailable.

## Explicit host SDK adoption

The private managed SDK is the default. To launch AVDs stored in an existing
host SDK/AVD home, grant only those paths and set both roots explicitly, for
example:

```bash
flatpak override --user \
  --filesystem="$HOME/Android/Sdk:ro" \
  --filesystem="$HOME/.android/avd" \
  --env=AVDM_SDK_ROOT="$HOME/Android/Sdk" \
  --env=ANDROID_AVD_HOME="$HOME/.android/avd" \
  io.github.ydog12138.liteavd
```

Use a writable SDK grant only when liteavd should install or remove components
there. A process already running outside the sandbox is not considered
controllable: Flatpak process isolation prevents the `/proc` identity checks
required by liteavd's adopted-session safety boundary.

## Current validation boundary

The installed Wayland application smoke test passes. A production managed
session has also passed inside the sandbox with a read-only, explicitly granted
test SDK: KVM launch, advertisement discovery, JWT-authenticated localhost
gRPC/adb, private `share-vid` shared memory, GTK rendering, input, exact stop,
and pre-exit process/port/shm/auth cleanup all succeeded. This path did not
install or start Xvfb. Separate hermetic sandbox tests also pass for a local
HTTP/zip UI installation and for declining or closing a fixture license dialog.

The empty-data boundary was completed on 2026-08-10: starting with no private
SDK or AVD, the user explicitly accepted Google's real license, installed the
Emulator, Platform Tools and an Android 35 image, created an AVD, then booted,
viewed, controlled and stopped it inside the installed Flatpak. No Xvfb was
used. This run exposed stale inherited SDK overrides and exact-stop shared
memory cleanup; both now have regressions in the source tree. A rebuilt 0.1.0
bundle was installed and the existing private AVD repeated the product
start/stop path: engine, adb entry, ports, auth session and shared memory were
gone while the AVD remained. A further regression invalidates viewport input
routes as soon as stopping begins, preventing reconnect log spam while the
engine is exiting.

The installed product also passed the opt-in desktop-host path on the Wayland
session's XWayland `DISPLAY`: the Emulator used `-gpu host -no-window
-share-vid`, selected the RX 7900 XTX/RADV device, held five
`/dev/dri/renderD128` descriptors, and loaded no known software renderer. The
viewport, input, Quick Boot save, exact stop, ports, JWT state, and private shm
cleanup all passed without Xvfb. An interrupted historical Quick Boot lock
found during the first attempt now has conservative dead-PID plus BSD/POSIX
lock recovery tests.

WP-3.5 adds the standard PulseAudio socket and CPAL's explicit Pulse backend.
The updated GNOME 50 release builds offline and the sandbox can create and run
a 48 kHz stereo i16 output callback. CPAL's pure-Rust Pulse client initially
failed in a clean Flatpak home because it encoded a missing cookie as a
zero-length authentication blob; liteavd now creates a no-clobber, mode 0600,
256-byte zero placeholder only in the private Flatpak Pulse config directory.
The server still authenticates the local connection with same-UID peer
credentials. Real AVD output chains pass in this sandbox under both managed GPU
policies without Xvfb. A three-device desktop-host run completed 30 minutes and
60 focused-audio handoffs with a 60 ms p95, bounded RSS/threads/fds, no dropped
samples, and exact cleanup. Installed-GUI audibility and focus isolation were
confirmed, 20 audio/visual events measured a 160 ms p95, and the final
three-device headless-swangle 30-minute gate passed. A repeated Google Clock
timer workload can still crash the fixed Emulator 37.1.11 swangle engine; this
is recorded as an upstream-specific limitation rather than hidden by fallback.

WP-3.7 reuses the PulseAudio socket without adding filesystem, D-Bus, or device
permissions. Its first sandbox endpoint probe exposed that the host Pulse
daemon cannot open a FIFO inside liteavd's private auth namespace. The FIFO is
therefore a mode 0600, PID/session-key-bound object under the app-specific,
current-user mode 0700 `$XDG_RUNTIME_DIR/app/io.github.ydog12138.liteavd`
directory; metadata, recovery, and dead-owner cleanup all revalidate identity.
GNOME 50 release commit
`4b6ad4c9720d90fd1efffbe768b922a112d21e4d11bef06160766d1db2f75e44`
passes endpoint recovery, a fixed 20 ms CPAL host-input callback, and a real
KVM/WAV-to-guest recording chain in the actual finish-args sandbox, under
unchanged permissions and without Xvfb. A separate three-device DesktopHost
gate completed 30 minutes and 60 exact-source handoffs with a 9 ms p95, bounded
RSS/threads/fds and exact cleanup. The installed GTK chooser then passed a
document-portal WAV-to-guest run, and a physical host microphone produced an
8.02-second guest recording whose speech was confirmed by listening. The
dedicated stop control removed the CPAL source-output, proving that privacy
stop closes host capture. WP-3.7 is therefore complete; this does not close the
separate guest-output validation tracked by WP-3.5.

WP-3.6 does not add a filesystem permission. APK multi-select and ordinary-file
push consume local paths returned by GTK's file chooser portal or `GdkFileList`
drop, then stream them through the sandbox's existing adb process. The final
installed build used GTK's real chooser to install a signed fixture APK and
accepted a file-manager drop into the guest. Both portal grants were limited to
the exact source files and revoked after validation. A separate three-device
gate installed the APK on every guest, pushed and hashed 256 MiB files,
preserved per-device partial failures, and canceled a live adb transfer without
leaving a process or `.part` file. WP-3.6 is complete.

WP-4.2 and WP-4.3 are complete. The public `v0.1.0` prerelease passed the tag
workflow, CI installation and checksum checks, followed by an independent local
download, checksum, and reinstall. A public screenshot remains useful for the
release page but is not a gate for the validated product boundary.
