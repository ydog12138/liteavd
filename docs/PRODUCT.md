# liteavd 产品定义

[English](en/PRODUCT.md) | **简体中文**

更新时间：2026-08-09

## 一句话定位

liteavd 是 Linux 上的本地 Android 多设备实验台：在一个 Wayland 原生工作区中，同时启动、查看、操作和诊断多个官方 Android Virtual Device。

它不是另一个 AVD Manager。AVD 创建、镜像下载和零 Java 安装是降低使用门槛的基础设施；可同时工作的多设备视口、批量操作、可靠调度和故障隔离才是产品本体。

## 目标用户与任务

第一目标用户：

1. Android 开发者：并排观察不同 API、屏幕尺寸或配置下的同一功能；
2. 移动 QA/测试工程师：向一组设备安装同一 APK，执行操作并收集独立结果；
3. 本地自动化与智能体开发者：需要可寻址、可恢复且不会端口串线的设备会话。

首版必须优化的任务：

```text
打开项目
  → 选择 2–4 个已有 AVD
  → 一次启动（超出资源时显示队列原因）
  → 所有设备同时可见
  → 焦点输入只进入一台设备
  → 向选中设备批量安装 APK / 截图 / 停止
  → 下次打开恢复工作区与实例状态
```

游戏多开、消费者 Android 模拟器、云设备农场和完整 Android Studio 替代品不属于首版目标。

## 两种 SDK 入口

产品必须允许两条入口并存：

| 模式 | 优先级 | 行为 |
|---|---:|---|
| 接管现有 SDK | 首选入口 | 用户指定已有 SDK/AVD；liteavd 不调用 `sdkmanager`/`avdmanager`，直接进入设备工作区 |
| liteavd 托管 SDK | 完整入口 | 直接解析 Google repository、展示许可、下载并安装组件、创建 AVD；运行 liteavd 不需要 Java |

“接管现有 SDK”让用户先获得多设备价值；“托管 SDK”保证 Linux/Flatpak 和空机器安装闭环。两者共享同一个 `InstanceRegistry`、`Scheduler` 与视口，不形成两套运行模型。

## MVP 边界

MVP 必须具备：

- 接管已有 SDK 和 AVD；
- 2–4 个本地 AVD 的原生嵌入显示；
- 每设备独立状态、日志和输入上下文；
- 焦点隔离及明确的 focused/selected/all 操作目标；
- 端口原子预留、启动限流、资源不足排队与取消；
- 批量 APK 安装、截图和停止，逐设备报告结果；
- 应用重启后的外部实例接管和工作区恢复。

不阻塞 MVP：

- H264/WebRTC 远程显示；
- 自动降低 AVD RAM 或静默切换 GPU；
- iOS、Windows、macOS、ARM host；
- 完整 SDK Manager、所有 system-image channel；
- 团队账号、云编排和设备租用。

## 竞争边界

截至 2026-08-09，没有发现同时满足“Linux + 官方 Google AVD + 原生多视口 + 本地资源调度 + 零 Java 托管”的成熟产品；但各局部能力已有直接竞争，不能把 `share-vid` 本身当作壁垒。

| 产品 | 已覆盖能力 | liteavd 必须形成的边界 |
|---|---|---|
| [Android Studio Device Manager](https://developer.android.com/studio/run/managing-avds) | 官方 AVD 管理；模拟器可嵌入 Running Devices | 独立 Linux 多设备工作区、批量操作和调度，而不是 IDE 内单设备工具窗 |
| [CoreDeck](https://coredeck.dev/) | 独立 AVD 创建、镜像和启动管理 | 不停留在管理 GUI；运行设备必须在同一工作区中可同时操作 |
| [SimDeck](https://simdeck.sh/guide/) | 启停、显示、输入、安装、自动化、浏览器/API | 当前要求 Apple Silicon macOS；liteavd 聚焦 Linux/Wayland、本地 GTK 多视口和资源编排 |
| [SimDeck video](https://simdeck.sh/guide/video) | Android 同样读取 `-share-vid` BGRA，并编码为 WebRTC | `share-vid` 是实现手段，不是独占价值；差异来自 Linux 原生零编码本地视口与多设备工作流 |
| [simmer](https://github.com/joshdholtz/simmer) | 浏览器并排显示和控制多个模拟器 | Linux 原生、低延迟共享内存、AVD 生命周期与调度 |
| [Genymotion Desktop](https://docs.genymotion.com/usage/desktop/overview/) | 成熟本地虚拟设备管理 | 使用官方 AVD，并把稳定并行作为验收目标；Genymotion Desktop [不以并行设备为设计目标](https://support.genymotion.com/hc/en-us/articles/15006454206877-How-many-devices-can-I-run-simultaneously) |
| [Anbox Cloud](https://canonical.com/anbox-cloud/docs/explanation/anbox-cloud/) | Android 实例生命周期、资源管理和流式访问 | 本地开发者工作区而非服务器/云端 Android 容器平台 |

竞品变化需要在每个发布里程碑前复核；本表是产品决策证据，不是永久市场结论。

## 产品原则

1. 同时可见优先于设备数量：先保证 3 台稳定、低延迟和输入不串流，再追求更高密度。
2. 批量操作必须显式选择目标：focused、selected、all running 不能混淆。
3. 调度必须可解释：显示等待原因，不以随机启动失败代替排队。
4. 不静默改设备配置：RAM/GPU 降级需要用户确认并记录。
5. 外部实例是可接管资源：默认不因 GUI 退出而杀死并非本次会话创建的实例。
6. 技术指标服务于任务：`p95 <50ms` 重要，但不能替代多设备启动成功率、批量结果准确性和会话恢复。

## 成功指标

| 指标 | MVP 目标 |
|---|---|
| 从启动 liteavd 到 3 台已有 AVD 可交互 | 可重复测量并建立基线；后续版本不得无解释回退 |
| 3 台并发启动端口冲突 | 0 |
| 焦点输入串入非目标设备 | 0 |
| 批量操作目标/结果错配 | 0 |
| 单设备输入到新帧 p95 | `<50ms`，至少 500 个可观察样本 |
| 3 设备 soak | 30 分钟无崩溃、无无界 RSS/fd/线程增长 |
| 应用重启实例接管 | 所有存活且身份校验通过的实例可恢复 |

实现边界见 [ARCHITECTURE.md](ARCHITECTURE.md)，已验证的外部事实见 [VALIDATED_FACTS.md](VALIDATED_FACTS.md)。
