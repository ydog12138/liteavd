# liteavd product definition

**English** | [简体中文](../PRODUCT.md)

Last updated: 2026-08-09. Competitor observations are dated product-decision
evidence, not permanent market claims.

## One-sentence position

liteavd is a local Android multi-device workspace for Linux: launch, view,
operate, and diagnose multiple official Android Virtual Devices together in
one native Wayland application.

It is not another generic AVD manager. AVD creation, image downloads, and a
Java-free installation path reduce setup friction; simultaneous device
viewports, batch operations, reliable scheduling, and fault isolation are the
product itself.

## Target users and jobs

Primary users are:

1. Android developers comparing one feature across API levels, display sizes,
   or device configurations;
2. mobile QA engineers installing one APK on a device set, exercising it, and
   collecting independent results;
3. local automation and agent developers who need addressable, recoverable
   device sessions without port confusion.

The first-release workflow is:

```text
Open the project
  → choose 2–4 existing AVDs
  → start once, with visible queue reasons when resources are exhausted
  → keep every device visible
  → route focused input to exactly one device
  → install APKs / take screenshots / stop selected devices in a batch
  → restore workspace intent and live instance state on the next launch
```

Game multi-instance tooling, consumer Android emulators, cloud device farms,
and a full Android Studio replacement are outside the initial target.

## Two SDK entry paths

| Mode | Priority | Behavior |
|---|---:|---|
| Adopt an existing SDK | Preferred entry | The user selects existing SDK/AVD data; liteavd does not call `sdkmanager` or `avdmanager` and enters the device workspace directly |
| liteavd-managed SDK | Complete entry | Parse Google repositories, display licenses, download/install components, and create AVDs without requiring Java at runtime |

Existing-SDK adoption delivers the multi-device value quickly. Managed mode
closes the clean-machine and Flatpak setup path. Both use the same
`InstanceRegistry`, scheduler, and viewport model.

## MVP boundary

The MVP includes:

- adoption of existing SDK and AVD data;
- native embedded display for 2–4 local AVDs;
- per-device state, logs, and input context;
- isolated focus and explicit focused/selected/all-running operation targets;
- atomic port reservation, bounded launch concurrency, resource queues, and
  cancellation;
- batch APK installation, screenshots, and stop with per-device results;
- live-instance and workspace recovery after an application restart.

The following do not block the MVP:

- H.264/WebRTC remote display;
- automatic AVD RAM reduction or silent GPU fallback;
- iOS, Windows, macOS, or ARM hosts;
- a complete SDK Manager or every system-image channel;
- team accounts, cloud orchestration, or device rental.

## Competitive boundary

As of 2026-08-09, the project audit did not identify a mature product combining
Linux, official Google AVDs, native multi-viewports, local resource scheduling,
and a Java-free managed path. Each individual capability has direct
competition, so `-share-vid` itself is not treated as a moat.

| Product | Existing capability | Boundary liteavd must establish |
|---|---|---|
| [Android Studio Device Manager](https://developer.android.com/studio/run/managing-avds) | Official AVD management; Emulator integration in Running Devices | A standalone Linux multi-device workspace with batch operations and scheduling rather than an IDE single-device tool window |
| [CoreDeck](https://coredeck.dev/) | Standalone AVD creation, image, and launch management | Go beyond management UI: running devices remain simultaneously operable in one workspace |
| [SimDeck](https://simdeck.sh/guide/) | Start/stop, display, input, install, automation, browser/API | SimDeck currently targets Apple Silicon macOS; liteavd targets Linux/Wayland, native GTK multi-viewports, and local resource orchestration |
| [SimDeck video](https://simdeck.sh/guide/video) | Reads Android `-share-vid` BGRA and encodes it for WebRTC | `-share-vid` is an implementation mechanism; differentiation comes from a native, zero-encode Linux viewport and multi-device workflow |
| [simmer](https://github.com/joshdholtz/simmer) | Browser-based side-by-side Emulator display and control | Native Linux shared-memory latency plus AVD lifecycle and scheduling |
| [Genymotion Desktop](https://docs.genymotion.com/usage/desktop/overview/) | Mature local virtual-device management | Official AVDs and validated stable parallel operation; Genymotion Desktop [does not design for simultaneous devices](https://support.genymotion.com/hc/en-us/articles/15006454206877-How-many-devices-can-I-run-simultaneously) |
| [Anbox Cloud](https://canonical.com/anbox-cloud/docs/explanation/anbox-cloud/) | Android lifecycle, resource management, and streaming | A local developer workspace rather than a server/cloud Android container platform |

Recheck competitors before each release milestone.

## Product principles

1. Simultaneous visibility beats raw count: make three devices stable,
   responsive, and isolated before pursuing density.
2. Batch targets are explicit: focused, selected, and all running must never be
   conflated.
3. Scheduling is explainable: show queue reasons instead of replacing them with
   random launch failures.
4. Device configuration never changes silently: RAM or GPU degradation needs
   user confirmation and a record.
5. External instances are adopted resources: closing the UI does not kill an
   instance merely because liteavd did not create it in the current process.
6. Metrics serve user tasks: p95 latency matters, but cannot replace launch
   success, exact batch results, or recovery correctness.

## Success metrics

| Metric | MVP target |
|---|---|
| Time from opening liteavd to three interactive existing AVDs | Establish a repeatable baseline; later versions must not regress without explanation |
| Port collisions during three concurrent starts | 0 |
| Focused input delivered to a non-target device | 0 |
| Batch operation target/result mismatch | 0 |
| Single-device input-to-new-frame p95 | `<50ms` over at least 500 observable samples |
| Three-device soak | 30 minutes without a crash or unbounded RSS/fd/thread growth |
| Live-instance adoption after app restart | Recover every surviving instance whose identity passes verification |

See the [architecture](ARCHITECTURE.md) for implementation boundaries and the
[validated facts](../VALIDATED_FACTS.md) for dated environment evidence.
