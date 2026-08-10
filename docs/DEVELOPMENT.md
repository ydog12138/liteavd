# Developing liteavd

**English** | [简体中文](DEVELOPMENT.zh-CN.md)

## Repository contract

Product and architecture facts live in the Chinese canonical documents:

- `docs/PRODUCT.md` — product position and MVP boundary;
- `docs/ARCHITECTURE.md` — implementation and trust boundaries;
- `docs/VALIDATED_FACTS.md` — machine/version/date-dependent evidence.

The English architecture document explains the same current design for an
international audience. It must not promote a target state into a completed
claim without repeatable evidence.

## Toolchain

- Rust 2024 edition;
- declared MSRV: Rust 1.88;
- current developer toolchain: `mise.toml`;
- GTK4/libadwaita for the default `gui` feature;
- vendored `protoc` binary through `protoc-bin-vendored`;
- Android Emulator proto inputs vendored under `proto/`.

The single crate exposes a default `gui` feature. Core-only code must continue
to compile without GTK/GDK/GLib/Pango/Cairo in its normal dependency graph.

## Build and quality gates

```bash
cargo fmt --all -- --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo +1.88.0 test --locked --no-default-features --lib
cargo +1.88.0 clippy --locked --no-default-features --lib -- -D warnings
```

GUI changes additionally need the smallest relevant Xvfb smoke test. Real
SDK/Emulator tests are `#[ignore]` and document their environment variables,
side effects, and cleanup requirements in the source file.

## Module ownership

- `src/core/` contains repositories, download/install, AVD/runtime state,
  scheduling, adb, authenticated gRPC, capture, operations, audio, and
  microphone logic. It must not reference GTK types.
- `src/ui/` composes GTK/libadwaita and projects core state. GTK objects stay on
  the main thread; workers hold pure `Send` data or `glib::SendWeakRef`.
- `proto/` contains version-pinned Emulator definitions and checksums.
- `tests/` contains hermetic tests by default and explicit ignored real-system
  gates.

Prefer explicit state models and interfaces for new cross-module behavior. Do
not expand thread-local global containers or create a new Tokio runtime for each
UI operation; the application owns a shared long-lived executor.

## Testing real Android components

Never use a personal SDK/AVD destructively. Use:

- a dedicated `AVDM_SDK_ROOT`;
- a unique temporary `ANDROID_AVD_HOME`;
- unique AVD names;
- verified process identity before signaling;
- RAII cleanup for processes, ports, auth material, shared memory, Pulse
  modules/FIFOs, files, and AVD definitions.

Real tests should run serially when they mutate process-wide environment
variables:

```bash
AVDM_SDK_ROOT=/path/to/test-sdk \
  cargo test --test operation_real -- --ignored --nocapture --test-threads=1
```

Select the smallest relevant real-system test for the changed boundary.
Generated prost code may receive lint configuration at its include boundary;
do not lower Clippy standards for project source.

## Android fixture policy

APK and WAV fixtures must be small, versioned, deterministic, and documented.
Keep source, manifest, generation instructions, and SHA-256 beside the binary.
Build Tools or a JDK used to generate a fixture are development-only and must
not become liteavd runtime dependencies.

Do not use a non-deterministic Clock ringtone as an exact audio route oracle.
The audio fixture uses Android `AudioTrack`; microphone verification uses a
deterministic recorder and waveform analysis.

## Cargo and Flatpak sources

`Cargo.lock` is part of the application/release boundary. After changing it,
regenerate `flatpak/cargo-sources.json` with the official
`flatpak-builder-tools` Cargo generator and rebuild the manifest offline.

The version in `Cargo.toml`, the newest AppStream release, the changelog, and
the release tag must agree.

```bash
flatpak/build-bundle.sh 0.1.0
```

The script builds a versioned bundle and checksum under `dist/`; both
directories are ignored build outputs.

## Vendored Emulator proto

The current proto snapshot comes from Android Emulator 37.1.11.0 build
15917651. An upgrade must:

1. replace only the required source proto files;
2. record the exact upstream version and generation command in
   `proto/README.md`;
3. update `proto/SHA256SUMS`;
4. rerun authenticated gRPC integration tests;
5. review allowlist methods and generated API differences.

Never introduce a bare `-grpc` fallback.

## Documentation and release evidence

Implementation claims need current code or a repeatable test. Machine-,
version-, and date-dependent results belong in `VALIDATED_FACTS.md`. Keep
historical failures when they explain a retained limitation.

Before tagging:

1. run the full hermetic gates and relevant hardware tests;
2. validate desktop/AppStream/Flatpak metadata;
3. audit tracked files for credentials and build output;
4. confirm version consistency;
5. push the release commit and require GitHub CI success;
6. create an annotated `v<version>` tag;
7. inspect the draft bundle and checksum before publishing the prerelease.
