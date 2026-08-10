# liteavd

[English](README.md) | **简体中文**

[![CI](https://github.com/ydog12138/liteavd/actions/workflows/ci.yml/badge.svg)](https://github.com/ydog12138/liteavd/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

liteavd 是 Linux 上的本地 Android 多设备实验台：在一个 Wayland 原生工作区中，同时启动、查看、操作和诊断多个官方 Android Virtual Device。它面向需要并排验证多个设备的 Android 开发者、移动 QA，以及需要稳定设备会话的本地自动化工具。

liteavd 不是另一个通用 AVD Manager。低延迟嵌入视口、多设备焦点与批量操作、可靠资源调度是产品本体；AVD 创建、SDK 组件安装与零 Java 托管是支撑能力。

> **状态：pre-alpha / 0.1.0 预发布。** 产品功能、Flatpak 本地验收和 GitHub artifact 验收已闭环，但仍不承诺生产稳定性或向后兼容。固定 Android Emulator 37.1.11 在 `HeadlessSwangle` 下反复触发 Google Clock timer 时存在上游特定崩溃限制；确定性连续音频与最终 30 分钟门禁不受影响。

## 亮点

- Wayland 原生 GTK4/libadwaita 单窗口，1–3 列响应式多设备工作区。
- `-share-vid` 最新帧捕获，无可见 Emulator 窗口，生产运行不依赖 Xvfb。
- managed/recovered/adopted session 分离，console port、进程身份、JWT 和资源所有权明确。
- session 独立 ES256/JWT gRPC 控制面；输入、截图、snapshot 和音频均不回退裸 gRPC。
- focused/selected/all-running 的截图、单/split APK、普通文件推送和停止操作。
- 焦点设备 guest 音频输出，以及显式、默认关闭的宿主麦克风或 PCM WAV 虚拟麦克风。
- FIFO 启动队列、内存预算和 host-GPU slot；默认 HeadlessSwangle，也可显式选择 DesktopHost。
- 可接管现有 SDK，或直接解析 Google repository、展示许可并零 Java 托管安装。
- Flatpak 私有 SDK/AVD、最小权限、精确 document portal 文件授权。

## 快速安装

当前唯一发布格式是 GitHub Releases 中的 Flatpak bundle，Flathub 暂缓。

```bash
sha256sum --check liteavd-0.1.0-x86_64.flatpak.sha256
flatpak install --user ./liteavd-0.1.0-x86_64.flatpak
flatpak run io.github.ydog12138.liteavd
```

Flatpak 会按 bundle 中的 runtime hint 从 Flathub 获取 GNOME runtime。宿主需要 Linux x86_64、可用的 `/dev/kvm` 和 Wayland 会话；默认 HeadlessSwangle 不需要 X11/Xvfb。完整前置条件、源码构建和精确宿主 SDK 授权见[安装指南](docs/INSTALLATION.zh-CN.md)。

## 首次使用

1. 打开“镜像与组件”，阅读并明确接受所需 Google 许可。
2. 安装 Emulator、Platform Tools 和一个 x86_64 系统镜像。
3. 创建 AVD；默认 GPU 策略是无需 display 的 HeadlessSwangle。
4. 启动一个或多个设备，在工作区中点击卡片切换焦点。
5. 从顶部作用域选择焦点、已选或全部运行设备，再执行截图、APK、文件推送或停止。

详细操作、音频/麦克风、snapshot、日志与故障处理见[用户指南](docs/USER_GUIDE.zh-CN.md)。

## 当前能力

| 能力 | 状态 | 说明 |
|---|---|---|
| 托管 SDK/镜像 | 已验证 | repository 解析、许可文本 hash、Range 续传、SHA-1/SHA-256、事务安装和 cache 配额 |
| AVD 生命周期 | 已验证 | 事务创建、广告发现、JWT 恢复、exact stop、进程/端口/shm 清理 |
| 多设备视口与输入 | 已验证 | 响应式网格、焦点隔离、旋转坐标、单/三设备长期门禁 |
| APK 与文件部署 | 已验证 | 单 APK、显式 split 集合、no-clobber push、三设备部分失败/取消、Flatpak chooser/drop |
| guest 音频输出 | 已验证 | focused-only、`MODE_REAL_TIME`、10ms callback、音画 p95 160ms、三设备 30 分钟 |
| 虚拟麦克风 | 已验证 | 显式宿主输入或 PCM WAV、focused-only、默认关闭、三设备/30 分钟/Flatpak portal |
| DesktopHost GPU | 已验证 | 复用桌面 XWayland，要求硬件 `/dev/dri` 证据，不静默回退 |
| GStreamer/H.264 | 未实现 | 当前 share-vid 路径满足已定义门禁，因此尚未引入 |
| AAB/APKS/XAPK | 不在首版范围 | 不引入 Java 或 bundletool；首版只处理单 APK 和用户明确选择的 split APK |

## 安全与隐私边界

- managed gRPC 仅监听 loopback，使用 session 独立 ES256/JWT、最小 allowlist 和 deadline。
- 停止实例前复验 console port、SDK 内进程身份和 exact session route，不只判断 PID 存活。
- Flatpak 没有 `home`/`host` 文件系统权限；选择器和拖放使用逐文件 portal grant。
- 宿主麦克风默认关闭、不持久化；停止后关闭采集，PCM 不写入日志或应用数据。
- liteavd 不包含或重新分发 Android SDK；Google 许可必须在下载前由用户明确接受。

安全问题请按[安全策略](SECURITY.zh-CN.md)私下报告。

## 开发

```bash
cargo build --locked
cargo test --locked --all-targets
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo +1.88.0 test --locked --no-default-features --lib
```

默认 GUI 构建需要 GTK4、libadwaita 和 PulseAudio 兼容开发库；`protoc` 由构建依赖 vendored。真实模拟器测试默认 `#[ignore]`，必须使用隔离 SDK/AVD home、唯一 AVD 名称并可靠清理。详见[开发指南](docs/DEVELOPMENT.zh-CN.md)和[贡献指南](CONTRIBUTING.zh-CN.md)。

## 文档

- [安装指南](docs/INSTALLATION.zh-CN.md)
- [用户指南](docs/USER_GUIDE.zh-CN.md)
- [架构与实现边界](docs/ARCHITECTURE.md) · [English architecture](docs/en/ARCHITECTURE.md)
- [开发指南](docs/DEVELOPMENT.zh-CN.md)
- [产品定义](docs/PRODUCT.md)
- [已验证事实](docs/VALIDATED_FACTS.md)
- [Flatpak 构建与沙箱策略](flatpak/README.md)

## 许可证

liteavd 自身使用 [MIT License](LICENSE)。Android Emulator、Platform Tools 和系统镜像受 Google 各自许可约束；liteavd 不重新许可或捆绑这些组件。
