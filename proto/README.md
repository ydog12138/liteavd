# Vendored Android Emulator proto

The emulator-specific `*.proto` files in this directory are byte-for-byte copies from:

```text
Android Emulator 37.1.11.0
build_id 15917651
$ANDROID_SDK_ROOT/emulator/lib/*.proto
```

This was rechecked on 2026-08-10 against `/data/Projects/liteavd-sdk`. The four files under `google/protobuf/` are the pre-existing minimal well-known-type snapshot required by these definitions; their exact content is pinned by `SHA256SUMS` because the emulator archive does not ship that directory.

From the repository root, verify the vendored inputs and regenerate the Rust output with:

```bash
sha256sum --check proto/SHA256SUMS
cargo build --locked --no-default-features --lib
```

`build.rs` sorts the root proto inputs before calling `tonic-build 0.12` / `prost-build 0.13`. Generated Rust stays in Cargo's `OUT_DIR` and is not committed. The generated include modules in `src/core/grpc.rs` allow only Clippy's two upstream-prose lints (`doc_overindented_list_items` and `doc_lazy_continuation`); project source remains under strict `-D warnings`.

When upgrading Emulator, copy the complete root proto set from one fixed emulator build, update this file and `SHA256SUMS`, then rerun the authenticated gRPC integration tests. Do not mix definitions from multiple emulator builds.
