# Changelog / 更新日志

All notable release changes are documented here. / 本文件记录各发布版本的重要变化。

## Unreleased / 未发布

### Fixed / 修复

- Local Flatpak instructions now install a single-file bundle instead of
  leaving enabled `flatpak-builder` cache remotes that can break system update
  checks. / 本地 Flatpak 说明改为安装单文件 bundle，避免留下会导致系统更新检查报错的启用状态构建 cache remote。

## [0.1.0] - 2026-08-10

First public pre-alpha release. / 首个公开 pre-alpha 预发布。

### Added / 新增

- Responsive GTK4/libadwaita multi-device Wayland workspace with embedded
  `-share-vid` viewports and exact input routing. / 响应式 GTK4/libadwaita
  Wayland 多设备工作区、嵌入 `-share-vid` 视口与精确输入路由。
- Managed/recovered/adopted session model, FIFO resource scheduler, atomic port
  reservation, bounded logs, and restart recovery. / session 来源模型、FIFO
  资源调度、原子端口预留、有界日志与重启恢复。
- Per-session ES256/JWT gRPC for input, screenshots, snapshots, speaker audio,
  and microphone state. / session 独立 ES256/JWT 控制面。
- Java-free Google repository, license, cache, component installation, and
  transactional AVD creation flow. / 零 Java repository、许可、cache、组件安装
  与事务 AVD 创建。
- Focused/selected/all-running screenshots, single/split APK installation,
  no-clobber file push, and exact stop with per-device results. / 多作用域截图、
  单/split APK、no-clobber 文件推送和逐设备 stop 结果。
- Focused guest speaker output and explicit host-input/PCM-WAV virtual
  microphone. / 焦点 guest 音频输出与显式宿主输入/PCM WAV 虚拟麦克风。
- Minimal-permission GNOME 50 Flatpak, private SDK/AVD storage, CI, and
  versioned GitHub bundle workflow. / 最小权限 GNOME 50 Flatpak、私有 SDK/AVD、
  CI 与版本化 GitHub bundle workflow。

### Validation / 验证

- Rust 1.88 core-only and stable all-target tests with strict Clippy. / Rust
  1.88 core-only、stable 全目标测试与 strict Clippy。
- Single- and three-device long-running viewport, fault isolation, audio, and
  microphone gates on controlled hardware. / 受控硬件上的单/三设备视口、故障
  隔离、音频与麦克风长门禁。
- Installed Flatpak empty-data SDK flow, both GPU policies, APK/file portal
  operations, audio/microphone, and exact cleanup without production Xvfb. /
  安装 Flatpak 的空数据 SDK、两种 GPU、portal 部署、音频/麦克风与无 Xvfb
  清理全链。

### Known limitations / 已知限制

- Android Emulator 37.1.11 may crash under repeated Google Clock timer plus
  HeadlessSwangle audio-stream stress; deterministic product audio passes the
  final 30-minute gate. / 固定 Emulator 在重复 Clock timer + HeadlessSwangle
  音频压力下可能崩溃；确定性产品音频已通过最终 30 分钟门禁。
- No AAB/APKS/XAPK parsing, bundletool, compressed microphone formats,
  multi-display, remote Emulator, or non-Linux platforms. / 暂不支持上述扩展。

[0.1.0]: https://github.com/ydog12138/liteavd/releases/tag/v0.1.0
