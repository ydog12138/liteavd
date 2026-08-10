# liteavd user guide

**English** | [简体中文](USER_GUIDE.zh-CN.md)

## 1. Mental model

liteavd presents Android Virtual Devices as session-bound cards in one
workspace. A card is not identified only by its AVD name: operations are bound
to the exact session ID, generation, console port, and verified process. If a
device stops and a new process reuses its name or port, an old operation cannot
cross into the replacement session.

There are three session origins:

- **Managed** — launched by the current liteavd process and fully controllable.
- **Recovered** — launched by liteavd earlier and recovered with its private
  identity after an application restart.
- **Adopted** — observed externally. It remains visible, but features requiring
  liteavd's private JWT key are intentionally unavailable.

## 2. Images, components, and licenses

Open **Images & Components** to inspect installed and online packages. Managed
mode installs Emulator, Platform Tools, and system images directly from Google
repository archives without Java tooling.

Before a licensed component is installed, liteavd shows the license text. An
acceptance record is tied to both the license ID and normalized text hash. A
changed license is shown again. Decline, window close, or a persistence error
aborts the operation.

Downloads use a stable cache with resume, checksum verification, quota
enforcement, and transactional installation. Do not manually edit an active
`.part` or temporary install directory.

## 3. Create and manage AVDs

The creation wizard requires a fully verified local system image. Choose a
device profile, memory/data size, name, and GPU policy. Creation publishes the
`.ini` and `.avd` pair transactionally; an existing name is never overwritten.

An AVD cannot be deleted while its verified Emulator instance is running.

## 4. Start, focus, and select devices

Start requests enter a FIFO scheduler. A request may wait for:

- an even console port in `5554..=5586`;
- the configured concurrent-start limit;
- the configured memory budget;
- an available host-GPU slot when DesktopHost is selected.

The UI explains why a device is queued and allows cancellation before spawn.

Click a device card to focus it. `Ctrl+1`, `Ctrl+2`, and `Ctrl+3` focus visible
cards without accepting extra modifiers. The selection checkbox is independent
of focus and is used for batch operations.

The operation scope menu has three explicit values:

- focused device;
- selected devices;
- all running devices.

## 5. Viewport and input

Managed and recovered sessions render their latest complete `-share-vid` BGRA
frame in a GTK picture. Older unread frames are overwritten instead of queued.

The viewport forwards:

- primary-button touch/drag/release;
- mouse hover and secondary button events;
- navigation keys;
- UTF-8 input-method commits.

Coordinates are mapped to the actual shared-video buffer with letterbox
handling. Input outside the image is ignored; an active drag is clamped and is
always released when focus or attachment is lost.

## 6. Card shortcuts

Each managed/recovered card exposes responsive controls for common device
actions, including Android navigation, guest volume, screenshot, microphone
source, and power/stop. Controls freeze the exact route when invoked, so a
replacement session cannot receive a delayed action.

## 7. Screenshots, APKs, and files

The operation toolbar applies to the chosen scope and reports every target in a
stable order.

### Screenshots

Screenshots use authenticated gRPC, validate PNG output, write a same-directory
temporary file, and publish without overwriting an existing name.

### APK installation

- A single APK uses `adb install -r -t`.
- An explicitly selected all-APK set uses `adb install-multiple -r -t`.
- `-d` (allow downgrade) and `-g` (grant runtime permissions) are opt-in
  confirmation choices.
- Mixed types, `.aab`, `.apks`, XAPK, symlinks, and non-regular files are
  rejected.

Choose files with the GTK portal or drop them on the APK button. Review the
exact target sessions and flags before confirming.

### Ordinary-file push

Choose or drop regular files on the file-push button. Files are streamed to:

```text
/sdcard/Download/liteavd/
```

The remote name contains a sanitized basename, operation ID, and item index.
Each file is first written to a unique `.part` and then published without
overwriting an existing target. Failure or cancellation attempts to remove
staging files while the original route remains valid.

## 8. Guest speaker output

Only the focused managed/recovered session plays through liteavd. The route
uses authenticated real-time `streamAudio`, a bounded 120ms buffer, and a 10ms
host callback. Focus changes clear the old route and apply short fades to avoid
clicks or overlap.

The header controls provide independent playback enable/mute and application
volume. Guest Android media volume remains a separate control.

Adopted sessions without the private key show audio as unavailable. A recovered
session launched by an older liteavd allowlist may require a device restart.

## 9. Virtual microphone

The microphone is default-off, non-persistent, and restricted to one exact
focused managed/recovered session. Two mutually exclusive sources are
available:

- live host input through the default PulseAudio-compatible capture device;
- a PCM WAV file selected or dropped by the user.

Supported WAV input is PCM U8 or S16, mono or stereo, at up to 48kHz. It is
streamed and converted to 48kHz mono S16. The UI provides pause/resume and a
dedicated stop action. MP3, AAC, FLAC, and compressed WAV are not supported.

Stopping, changing focus, replacing the route, resetting control state, or
exiting the application cancels the old source. PCM is not persisted.

## 10. Snapshots and logs

The snapshot dialog operates on the focused exact session and supports list,
save, load, and delete. Loading a snapshot resets long-lived control streams;
the UI reconnects after the Emulator control plane becomes available again.

Managed session logs are bounded and rotated. The viewer loads them off the GTK
thread, filters stdout/stderr, and exports without clobbering an existing file.
Do not treat adopted sessions as having a recoverable managed log pipe.

## 11. Settings and GPU policies

Settings use a versioned, mode-0600 atomic file. Available resource controls
include concurrent starts, memory budget, host-GPU slots, cache quota, log
level, and managed GPU policy.

- **HeadlessSwangle** is the default. It needs no display and consumes no
  host-GPU slot.
- **DesktopHost** requires the desktop's XWayland `DISPLAY`, consumes one
  host-GPU slot, and verifies that the Emulator opened `/dev/dri` without a
  known software renderer. It never silently falls back.

A GPU policy change applies immediately only while the scheduler has no queued,
active, or external allocation. Otherwise it takes effect after restarting
liteavd.

## 12. Stop and recovery

Stop first invalidates input and long-lived streams, verifies the exact process,
requests Emulator termination, waits, and escalates only for the same verified
process. The port and resource reservation remain owned until stop completes.

Closing liteavd does not implicitly kill a healthy managed Emulator. On the
next launch, private recovery leases, advertisement files, process identity,
JWT material, capture, focus, and selection are reconciled. Truly external
instances remain adopted rather than being silently granted control.

## 13. Known limitations

- Android Emulator 37.1.11 may SIGSEGV under repeated Google Clock timer stress
  with HeadlessSwangle and concurrent audio streaming. Deterministic continuous
  audio passes the 30-minute product gate; the upstream-specific stimulus is
  retained as a known limitation.
- The first release does not parse AAB/APKS/XAPK or invoke bundletool.
- The virtual microphone supports PCM WAV only.
- Multi-display, remote Emulator/WebRTC, ARM hosts, Windows, and macOS are out
  of scope.
- DesktopHost is not a true-headless policy and will fail without XWayland and
  hardware renderer evidence.

For installation failures, see [INSTALLATION.md](INSTALLATION.md). For a bug,
collect the exact AVD name, session state, GPU policy, bounded session log, and
reproduction steps without publishing credentials or private guest data.
