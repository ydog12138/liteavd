# Flatpak 构建与沙箱策略

[English](README.md)

仓库根目录的 `io.github.ydog12138.liteavd.yml` 使用 GNOME 50 runtime 和匹配的 Freedesktop 25.08 Rust extension。Cargo 依赖来自 `Cargo.lock`，在离线沙箱构建前下载。

## 本地构建

```bash
flatpak/build-bundle.sh 0.1.0
flatpak install --user dist/liteavd-0.1.0-x86_64.flatpak
flatpak run io.github.ydog12138.liteavd
flatpak info --show-permissions io.github.ydog12138.liteavd
```

本地安装也应使用单文件 bundle，不要用 `flatpak-builder --install` 作为持久开发安装：它会注册指向 `.flatpak-builder/cache` 的启用状态本地 remote，并另外创建调试符号 remote。清理构建 cache 后，系统更新工具查询这些 remote 会失败。安装 bundle 时 Flatpak 会创建 disabled、non-enumerated origin；后续仍通过安装新 bundle 更新。

元数据检查：

```bash
desktop-file-validate data/io.github.ydog12138.liteavd.desktop
appstreamcli validate --no-net data/io.github.ydog12138.liteavd.metainfo.xml
flatpak-builder --show-manifest io.github.ydog12138.liteavd.yml >/dev/null
```

`Cargo.lock` 变更后，使用官方 `flatpak-builder-tools` cargo generator 更新 `flatpak/cargo-sources.json`：

```bash
python3 flatpak-cargo-generator.py Cargo.lock -o flatpak/cargo-sources.json
```

## GitHub Releases

Flatpak 是唯一发布包格式；当前通过 GitHub Releases 发布单文件 bundle，Flathub 暂缓。上述本地构建命令会同时生成版本化 bundle 和 checksum。

版本必须与 `Cargo.toml` 和最新 AppStream release 一致。`v*` tag 会触发 `.github/workflows/release-flatpak.yml`：在 Ubuntu 24.04 构建、安装验证并校验 SHA-256，然后创建带 `.flatpak` 和 `.sha256` 的 draft prerelease，供维护者验收后发布。bundle 不内含 GNOME runtime；安装时会按内嵌提示从 Flathub 获取 runtime。

## 持久化与权限

应用默认没有宿主文件系统权限。Flatpak 私有 XDG 目录位于 `~/.var/app/io.github.ydog12138.liteavd/`；托管 SDK 和 AVD 分别写入其中的 `data/liteavd/android-sdk` 与 `data/liteavd/avd`。

静态权限仅包括：

- GTK 原生 Wayland，以及仅供用户显式选择 `DesktopHost` 时复用的 XWayland/X11；
- focused guest 扬声器与用户显式开启的宿主/WAV 虚拟麦克风所需 PulseAudio socket；
- GTK 和 Emulator 图形渲染所需 DRI；
- Android Emulator 所需 `/dev/kvm`；
- Google 下载和 localhost console/adb/gRPC 所需网络。

没有 `home`、`host`、宿主 `/dev/shm`、USB、输入设备或 D-Bus 通配权限。虚拟麦克风默认关闭、不持久化，也不会隐式打开宿主输入。`HeadlessSwangle` 不需要显示服务器；`DesktopHost` 使用现有桌面 XWayland，在没有硬件 renderer 证据时明确失败。Xvfb 只用于 synthetic CI/测试，不是产品运行时依赖。

## 显式接管宿主 SDK

默认应使用私有托管 SDK。如需接管宿主 SDK/AVD，只授权确切路径：

```bash
flatpak override --user \
  --filesystem="$HOME/Android/Sdk:ro" \
  --filesystem="$HOME/.android/avd" \
  --env=AVDM_SDK_ROOT="$HOME/Android/Sdk" \
  --env=ANDROID_AVD_HOME="$HOME/.android/avd" \
  io.github.ydog12138.liteavd
```

只有在确实需要 liteavd 修改该 SDK 时才授予写权限。由于 Flatpak 的进程隔离，沙箱外已经运行的 Emulator 不满足 liteavd 的 `/proc` 身份验证边界，不能假定可控。

## 已验证边界

2026-08-10 的 GNOME 50 安装包已验证：空数据许可/下载/安装/AVD 创建、KVM、JWT gRPC、adb、广告发现、私有 share-vid、GTK 显示与输入、两种 GPU policy、guest 输出、显式虚拟麦克风、APK/文件 portal、三设备部分失败与取消，以及停止后的精确清理。详细机器与版本证据见 [`docs/VALIDATED_FACTS.md`](../docs/VALIDATED_FACTS.md)。
