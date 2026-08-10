# liteavd documentation

[简体中文](README.zh-CN.md)

This directory separates user-facing guidance, contributor guidance, design
authority, and dated validation evidence. Product claims in the overview must
remain consistent with the architecture and validation records.

## Start here

| Document | Purpose |
|---|---|
| [Installation](INSTALLATION.md) | Install a GitHub Release bundle, build from source, update, and troubleshoot |
| [User guide](USER_GUIDE.md) | Prepare an SDK, create and operate devices, configure audio and GPU policy |
| [Architecture](en/ARCHITECTURE.md) | Components, state ownership, data paths, security boundaries, and limitations |
| [Product definition](en/PRODUCT.md) | Target users, MVP boundary, competitive boundary, principles, and success metrics |
| [Development](DEVELOPMENT.md) | Toolchain, build commands, tests, repository layout, and change-specific gates |
| [Contributing](../CONTRIBUTING.md) | Contribution workflow and review expectations |
| [Security](../SECURITY.md) | Supported versions, threat boundary, and private reporting |

## Design and validation records

- [Chinese product definition](PRODUCT.md)
- [Architecture authority](ARCHITECTURE.md)
- [Validated facts](VALIDATED_FACTS.md)

When a dated result changes, update `VALIDATED_FACTS.md`. Never turn a target
or an ignored integration test into an unqualified completion claim.
