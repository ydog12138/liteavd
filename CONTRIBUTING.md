# Contributing to liteavd

**English** | [简体中文](CONTRIBUTING.zh-CN.md)

Thank you for improving liteavd. The project welcomes focused bug reports,
documentation corrections, reproducible performance evidence, and changes that
preserve the product and security boundaries.

## Before opening an issue

- Search existing issues before opening a new report.
- For a security vulnerability, do not open a public issue; follow
  [SECURITY.md](SECURITY.md).
- For an Emulator-specific failure, record the Emulator build, system image,
  GPU policy, host compositor/GPU, and whether Xvfb was involved.
- Remove JWTs, SDK license records, private guest data, and host paths that
  should not be public.

## Development setup

Follow [docs/INSTALLATION.md](docs/INSTALLATION.md) for dependencies and
[docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) for module boundaries and test
policy.

Create a topic branch from `master`. Keep each change scoped to one problem;
do not combine unrelated cleanup with a behavioral fix.

## Required engineering practices

- Prove a behavior defect with a test or minimal reproduction before fixing it.
- Core modules must remain free of GTK types.
- GTK objects stay on the main thread.
- Downloads, hashing, and extraction must not buffer whole large files or block
  the GTK main thread.
- Never weaken managed gRPC authentication or fall back to bare `-grpc`.
- Never stop an Emulator using only PID liveness; verify port and process
  identity.
- Preserve latest-frame semantics for `-share-vid`.
- Use isolated SDK/AVD roots for destructive or real-device tests.
- Keep license acceptance explicit and tied to the current text hash.

## Local checks

Run at least:

```bash
cargo fmt --all -- --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets --all-features -- -D warnings
```

Core changes must also pass Rust 1.88 core-only tests and Clippy. UI changes
need the relevant Xvfb smoke. Process/adb/gRPC/capture/scheduling changes need
the matching ignored integration gate on an isolated test SDK when practical.

## Documentation

Update both English and Simplified Chinese user-facing documents when behavior,
installation, permissions, or controls change. Machine/version/date evidence
belongs in `docs/VALIDATED_FACTS.md`; do not turn an intended design into a
completed claim.

## Pull request checklist

- [ ] The change has one clear objective and no unrelated edits.
- [ ] Observable behavior has regression coverage.
- [ ] `cargo fmt`, locked tests, and strict Clippy pass.
- [ ] Required Xvfb or ignored integration tests are listed with results.
- [ ] Failure, cancellation, and cleanup paths were considered.
- [ ] Security, permissions, and license behavior did not expand silently.
- [ ] English and Chinese documentation are consistent.
- [ ] No credentials, user SDK state, build output, or AI-assistant files are
      included.

By contributing, you agree that your contribution is provided under the
project's MIT License.
