# 安装 liteavd

[English](INSTALLATION.md) | **简体中文**

本文覆盖受支持的 GitHub Flatpak bundle、贡献者源码构建、首次 Android SDK 配置，以及对宿主已有 SDK 的精确授权。

## 1. 系统要求

0.1.0 预发布当前面向 Linux x86_64：

- 支持 GTK4 的 Wayland 桌面会话；
- 固件已启用硬件虚拟化，当前用户可读写 `/dev/kvm`；
- 使用正式安装包时需要 Flatpak；
- 下载 GNOME runtime 或 Google SDK 组件时需要网络；
- guest 音频输出和虚拟麦克风需要 PipeWire-Pulse 或 PulseAudio；可选麦克风端点不可用时，设备基本启动仍可继续；
- 只有显式选择 DesktopHost GPU 策略时，才需要 XWayland 和可用的硬件 `/dev/dri` renderer。

默认 HeadlessSwangle 不需要 `DISPLAY`、X11、可见 Emulator 窗口或 Xvfb。

安装前检查 KVM：

```bash
test -r /dev/kvm && test -w /dev/kvm
```

失败时应在固件中开启虚拟化，并修正宿主 KVM 用户组或 ACL；不要用扩大 Flatpak 设备权限来绕过。

## 2. 安装 GitHub Flatpak bundle

从 [GitHub Releases](https://github.com/ydog12138/liteavd/releases) 下载同一版本的两个文件：

- `liteavd-0.1.0-x86_64.flatpak`
- `liteavd-0.1.0-x86_64.flatpak.sha256`

校验并安装：

```bash
sha256sum --check liteavd-0.1.0-x86_64.flatpak.sha256
flatpak install --user ./liteavd-0.1.0-x86_64.flatpak
flatpak run io.github.ydog12138.liteavd
```

bundle 带有对应 GNOME runtime 的 Flathub hint；尚未安装 runtime 时，Flatpak 会从该 remote 获取。

检查沙箱权限：

```bash
flatpak info --show-permissions io.github.ydog12138.liteavd
```

预期静态权限只有 Wayland、供 DesktopHost 使用的显式 X11、PulseAudio、IPC、DRI、KVM 和 network，不应出现 `home` 或 `host` filesystem grant。

## 3. 首次配置托管 SDK

Flatpak 推荐使用私有托管 SDK，位置为：

```text
~/.var/app/io.github.ydog12138.liteavd/data/liteavd/android-sdk
~/.var/app/io.github.ydog12138.liteavd/data/liteavd/avd
```

在应用中：

1. 打开“镜像与组件”；
2. 选择 Emulator、Platform Tools 和一个系统镜像；
3. 阅读显示的 Google 许可文本并明确接受；
4. 等待下载、checksum 复验和事务安装；
5. 从已安装镜像创建 AVD。

拒绝、关闭许可对话框或许可状态写入失败都会中止安装。liteavd 运行时不调用 Java、`sdkmanager` 或 `avdmanager`。

## 4. 使用宿主已有 SDK

非 Flatpak 下 SDK 根优先级为：

1. 有效的 `AVDM_SDK_ROOT` 环境覆盖；
2. liteavd settings；
3. 平台默认值，通常为 `~/Android/Sdk`。

AVD home 使用 `ANDROID_AVD_HOME` 或 `~/.android/avd`。

Flatpak 默认看不到宿主路径。只授权精确 SDK 与 AVD 目录：

```bash
flatpak override --user \
  --filesystem="$HOME/Android/Sdk:ro" \
  --filesystem="$HOME/.android/avd" \
  --env=AVDM_SDK_ROOT="$HOME/Android/Sdk" \
  --env=ANDROID_AVD_HOME="$HOME/.android/avd" \
  io.github.ydog12138.liteavd
```

只有需要 liteavd 安装或移除宿主 SDK 组件时才授予 SDK 写权限。查看与重置 override：

```bash
flatpak override --user --show io.github.ydog12138.liteavd
flatpak override --user --reset io.github.ydog12138.liteavd
```

沙箱外已经运行的 Emulator 不视为可控 adopted session，因为 Flatpak 的进程隔离阻止 liteavd 完成所需 `/proc` 身份复验。

## 5. 从源码构建

需要：

- Rust 1.88 或更新版本；仓库 `mise.toml` 选择当前开发工具链；
- C/C++ 构建工具和 `pkg-config`；
- GTK4、libadwaita 与 PulseAudio 兼容开发库；
- 只有打包时才需要 Flatpak/flatpak-builder。

常见发行版示例：

```bash
# Arch Linux / CachyOS
sudo pacman -S --needed base-devel rust gtk4 libadwaita libpulse elfutils flatpak flatpak-builder appstream desktop-file-utils

# Ubuntu / Debian 系
sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev libasound2-dev libpulse-dev elfutils librsvg2-common
```

构建并运行：

```bash
git clone https://github.com/ydog12138/liteavd.git
cd liteavd
cargo build --locked
cargo run --locked
```

不安装 GTK 的 core-only 开发：

```bash
cargo test --locked --no-default-features --lib
```

## 6. 本地构建 Flatpak

```bash
flatpak remote-add --user --if-not-exists flathub \
  https://dl.flathub.org/repo/flathub.flatpakrepo

flatpak-builder \
  --user \
  --force-clean \
  --install-deps-from=flathub \
  --install \
  build/flatpak \
  io.github.ydog12138.liteavd.yml
```

Flatpak 解析完 `flatpak/cargo-sources.json` 后在沙箱内离线构建。修改 `Cargo.lock` 后必须重新生成该文件，详见[开发指南](DEVELOPMENT.zh-CN.md)。

## 7. 更新与卸载

安装新 bundle 仍使用 `flatpak install --user`。只卸载应用、保留私有数据：

```bash
flatpak uninstall --user io.github.ydog12138.liteavd
```

同时删除私有数据：

```bash
flatpak uninstall --user --delete-data io.github.ydog12138.liteavd
```

第二条会永久删除 Flatpak 私有目录中的托管 SDK、AVD、设置、日志和 cache；请先备份需要的 AVD 数据。

## 8. 安装故障排查

### `/dev/kvm` 不可用

修复宿主固件/KVM 权限。应用不会静默回退到无加速模拟。

### DesktopHost 拒绝启动

DesktopHost 要求非空 XWayland `DISPLAY`、`/dev/dri` 访问，以及 Emulator 确实打开硬件 renderer 的证据。真无头主机请使用 HeadlessSwangle。

### Flatpak 拒绝继承的 SDK 路径

该路径存在于宿主但在沙箱中不可见。按上文配置精确 override，或使用私有托管 SDK。

### 音频或虚拟麦克风不可用

确认宿主 PulseAudio 兼容服务运行，且 Flatpak 保留 `pulseaudio` socket。没有 liteavd 私有 JWT 身份的 adopted session 不能使用这些受控流。

### 文件选择器可用，但任意宿主路径不可访问

这是预期安全边界。选择器和拖放只授权 portal 导出的精确文件；liteavd 刻意不申请整个 home 权限。
