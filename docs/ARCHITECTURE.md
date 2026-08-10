# liteavd 架构与实现边界

[English](en/ARCHITECTURE.md) | **简体中文**

更新时间：2026-08-10。本文描述当前 `master` 工作树的实现事实，并把目标架构显式标为“目标”。

## 1. 产品分层

产品承诺不是“管理更多 AVD”，而是让 Android 开发、QA 和本地智能体在同一工作区可靠地同时使用多个设备。`share-vid`、gRPC 和零 Java安装链都是实现手段；同时可见、目标明确的跨设备操作与可解释调度才是对外能力。详见 [PRODUCT.md](PRODUCT.md)。

| 层 | 职责 | 当前状态 |
|---|---|---|
| 产品层 | share-vid 视口、输入、多设备交互、资源调度 | 响应式多视口、exact route 焦点/选择、逐设备结果、重启恢复、可配置预算与带故障长期稳定性已贯通 |
| 控制层 | 实例生命周期、状态、gRPC/adb 操作 | 有 managed/recovered/adopted session、广告/控制恢复、认证 client/capture/input 与 exact-route operation coordinator |
| 基础设施层 | repo、下载、许可、安装、AVD 文件 | 托管组件 service/GUI、事务式 AVD 创建与版本化 settings 已贯通 |
| 交付层 | Flatpak、CI、日志、升级 | session 日志、CI、Flatpak manifest/元数据/私有路径及空数据沙箱全链已实现；`v0.1.0` GitHub prerelease 的 bundle/checksum 已通过线上与本机安装验收 |

项目当前是一个同时提供 lib 与 GUI binary 的单 crate，edition 为 Rust 2024，声明最低 Rust 1.88。默认 `gui` feature 开启 GTK4/libadwaita/glib 和 binary；关闭默认 feature 后只构建 `core` lib，依赖图不含 GTK/GDK/GLib/Pango/Cairo。CI 分别验证这个 core-only 面与默认 GUI 面。

## 2. 当前结构

```text
main.rs
  └─ ui/main_window.rs
       ├─ ui/workspace.rs ── 响应式多设备 FlowBox + 焦点
       │  └─ ui/device_list.rs ── core/avd.rs
       │                       ├─ core/emulator.rs ── 广告文件 + /proc
       │                       └─ core/adb.rs
       ├─ ui/create_wizard.rs ── core/avd.rs
       ├─ ui/images_page.rs ── core/package_service.rs
       │                    ├─ core/repo.rs
       │                    ├─ core/download.rs
       │                    └─ core/install.rs
       └─ ui/settings_page.rs ── core/settings.rs

core/grpc.rs ── 认证 tonic client + 状态/截图/快照/input/audio/microphone-state RPC
core/microphone.rs ── per-session Pulse FIFO/source/sink + WAV/有界 buffer + 单路 coordinator
core/input.rs ── GTK 无关的 contain 坐标变换与 touch 状态机
core/audio.rs ── 音频格式校验 + 有界 PCM buffer + exact-route stream pump
core/grpc_auth.rs ── 每 session ES256 身份、JWK 投递、allowlist 与崩溃残留清理
core/device_state.rs ── generation 状态模型与扫描合并
core/scheduler.rs ── console port 原子预留 + FIFO 启动/内存/GPU reservation
core/instance.rs ── DeviceRuntime + InstanceRegistry + EmulatorSession
core/workspace.rs ── exact session route + focused/selected/all-running 状态
core/recovery.rs ── versioned/atomic workspace intent 持久化
core/advertisement.rs ── inotify 提示 + 迟创建目录重绑定
core/process_log.rs ── stdout/stderr 有界双文件轮转
core/stream.rs ── 安全 mmap reader + latest-frame capture
core/telemetry.rs ── 有界的输入→新帧分段时延样本
ui/viewport.rs ── GdkMemoryTexture + GtkPicture latest-frame 视口
ui/input.rs ── GTK controller + 有界/合并输入 worker
ui/device_controls.rs ── per-card exact-route 导航/设备音量/截图/麦克风快捷控制
ui/audio.rs ── focused route + CPAL PulseAudio sink + 静音/音量/状态
ui/microphone.rs ── 宿主 CPAL input/WAV 来源 + 非持久隐私状态
ui/workspace.rs ── 1–3 列产品网格 + 焦点视觉/快捷键
```

### 模块清单

| 模块 | 当前职责 | 主要缺口 |
|---|---|---|
| `repo.rs` | 组件/系统镜像 XML、license、archive 选择 | channel 集合固定；远端变化契约测试不足 |
| `download.rs` | Range/200/416、重试、流式 hash、父目录、`.part` rename 与 16MiB 文本上限 | 同步文件 I/O 仍位于 async 路径 |
| `install.rs` | zip 安装、路径校验、备份/回滚、每组件 `flock`、卸载使用检测、license hash 历史 | 安装解压仍是同步任务，必须由 worker service 调用 |
| `package_service.rs` | 稳定 XDG cache、下载/根配额 lease、复验、最旧可回收项清理、文本许可门禁、typed operation/event/result | cache 管理 UI 与访问时间统计 |
| `avd.rs` | versioned profile/创建配置、镜像结构复验、`config.ini`、事务创建/列举/删除保护 | 自定义 profile 编辑 UI 属于后续 package |
| `emulator.rs` | 启动、广告解析、端口/同名校验、PID 身份、engine/launcher/cancel 与 stale shm 清理 | host GPU 发现；真正外部 adopted session 授权 |
| `advertisement.rs` | 文件事件提示、迟创建目录重绑定、显式全量 rescan | 崩溃残留广告需要后续事件/手工 rescan 才再次过滤 |
| `process_log.rs` | stdout/stderr 有界双文件轮转、0600 权限、有界读取/过滤与 no-clobber 导出 | 保留周期与 cache 配额策略 |
| `device_state.rs` | `Stopped/Queued/Starting/Booting/Running/Recovering/Stopping/Error` 与 generation | Error 语义仍需随 command 类型解释 |
| `scheduler.rs` | console port 原子预留、FIFO 启动许可、内存/GPU reservation、排队取消、adopted 预算重建与持久设置接线 | 真实 RSS 估算 |
| `instance.rs` | registry、managed/recovered/adopted session、console/resource reservation、进程/日志/JWT/client/capture ownership、健康 revision 与长流 reset revision | input worker 仍由 viewport 生命周期持有 |
| `workspace.rs` | exact session/generation route、焦点、选择、pending restart intent 与 operation scope | 布局几何/窗口状态尚未保存 |
| `recovery.rs` | workspace intent 的 version/大小校验、0600 原子保存、损坏与符号链接拒绝 | schema 迁移只有 v1 拒绝路径 |
| `operation.rs` | operation id、冻结目标/确认重验、截图、单/split APK、no-clobber 文件推送、停止 executor、用户取消/阶段事件、exact-route snapshot/硬件键与逐设备结果；真实三设备/256MiB/取消门禁已建立 | 并行执行与持久历史 |
| `adb.rs` | boot 判定、64KiB 有界输出、显式 deadline/取消/kill+wait 的通用 runner、单/split APK 安装 | boot 轮询尚未复用通用 runner |
| `grpc.rs` | JWT interceptor、unary/input deadline、状态、PNG 截图、snapshot、键鼠/触摸、实时模式 `streamAudio` 与麦克风状态 get/set | 固定 Emulator 的 `injectAudio` 与 VirtioSnd 不兼容，明确不进入产品 allowlist/client |
| `grpc_auth.rs` | session ES256/JWK、最小 allowlist、私有恢复副本与独占 lease、死进程目录清理 | 密钥轮换和真正外部 adopted session 授权 |
| `input.rs` | contain/letterbox 坐标映射、clamp 规则、单触点 lifecycle 与导航键表 | 多点触控 |
| `audio.rs` | 48kHz/stereo/S16LE 与 packet 上限校验、60/120ms 有界 buffer、欠载/溢出统计、可取消 exact-route stream pump | 确定性音画事件、三设备焦点与长期门禁均已建立 |
| `stream.rs` | share-vid mmap 校验、一致帧复制、重 attach、latest-frame 与统计 | 无协议级通知，必须短周期观察 counter |
| `telemetry.rs` | 有界 pending/样本、单调时钟关联、p50/p95/p99 分段报告 | 当前是进程内验收 telemetry，未接用户可见诊断 UI |
| `settings.rs` | schema v1、v0 迁移、1MiB 上限/损坏报告、0600/0700 原子保存、SDK/调度/cache/日志/GPU policy | 后续 schema 的迁移函数 |
| `device_list.rs` | registry 投影、启停命令、选择框、原卡片刷新、managed interactive viewport attach/detach | 启动进度仍需更细粒度事件 |
| `microphone.rs` | 私有 Pulse FIFO source/null sink 生命周期与恢复元数据、48k mono S16 有界 host buffer、流式 PCM WAV 转换和全局单路 exact-route coordinator；Flatpak FIFO 位于宿主/沙箱共享的 0700 app runtime 目录 | 三设备互斥/故障、30 分钟资源、portal WAV 与宿主人工输入门禁均已通过 |
| `ui/device_controls.rs` | 每卡片响应式导航、电源、guest 音量、截图、宿主麦克风与 WAV 选择/拖放；按键串行且每项固化 exact route | WAV 暂不支持 MP3/AAC/FLAC |
| `ui/microphone.rs` | CPAL Pulse host input、固定 20ms callback、来源互斥、WAV 暂停/继续/取消、首 callback fail-closed、持续隐私状态与应用退出取消 | 非 PCM WAV 编解码不在首版范围 |
| `viewport.rs` | BGRA MemoryTexture、宽高比、4ms main-context latest-frame 泵、隐藏暂停、终态、session input/focus 与 telemetry 绑定 | 用户可见 per-viewport 诊断 |
| `ui/input.rs` | GTK touch/mouse/key/IM、capacity-1 motion、可靠 release/cancel、exact route、断线退避重连与健康投影 | 多点触控、滚轮、per-RPC 详细诊断 |
| `ui/audio.rs` | focused managed/recovered session 的 10ms CPAL PulseAudio 输出、应用级静音/音量、独立 unavailable/error 状态、sink 复用与 transient reconnect | adopted session 没有产品 JWT 私钥时保持明确不可用 |
| `ui/workspace.rs` | 1–3 列 FlowBox、焦点视觉、Ctrl+数字切换、独立卡片与 revision-only 刷新 | 布局几何持久化 |
| `ui/operations.rs` | focused/selected/all scope、截图、单/split APK flags、文件推送的 portal 选择/拖放、可取消逐设备阶段对话框、停止与最终报告；安装 Flatpak chooser/drop→guest 已验证 | 持久操作历史 |
| `ui/snapshots.rs` | focused exact session 的 snapshot 列表、保存/加载/删除与确认 | 批量 snapshot 策略未定义 |
| `ui/session_log.rs` | focused managed session 的日志查看、stdout/stderr 过滤与导出 | recovered/adopted session 不重建旧 stdout/stderr pipe |
| `images_page.rs` | 在线/已安装投影、显式许可决定、package operation 进度/错误与双区重建 | cache 管理 UI |
| `create_wizard.rs` | 完整本地镜像三步创建、镜像管理跳转/选择恢复与最终复验 | 自定义 profile 编辑 UI |

## 3. 当前运行模型

### UI 与后台线程

GTK 对象只应在主线程访问。UI 的网络、进程与 operation future 提交到共用的两线程长存 Tokio runtime；同步日志 I/O 进入其 blocking pool，不再为每个 callback 新建 runtime。worker 使用纯数据或 `glib::SendWeakRef`，通过 `MainContext::invoke` 更新 UI。下载进度由 `AtomicU64` 写入，GTK 主线程上的 200ms timer 读取。

`main_window` 创建一个应用级 `Arc<DeviceRuntime>`，所有设备卡片和后台命令共享。其内部 `InstanceRegistry` 持有 `DeviceStateStore`、session id、AVD/port 索引和 `EmulatorSession`；generation 拒绝旧异步结果，广告扫描不会抹掉 `Starting/Booting/Stopping/Error`。`WorkspaceState` 以 AVD name + session id + generation 保存焦点和选择，替换 session 不继承旧目标。`AdvertisementMonitor` 使用原生文件事件；目标目录不存在时监视最近的现存父目录，事件/错误只作为全量 rescan 提示。状态刷新复用原卡片，只有 AVD 集合变化、SDK 设置或显式结构刷新才重建网格。

managed session 提交到 registry 后，设备行从 runtime 取得新的 capture subscription 并附加 viewport；stop 失败时 session/capture/viewport 保留，stop 成功或实例消失时移除。当前 session 在 adb boot 完成后才提交，因此 viewport 尚不显示 boot 动画；adopted session 没有 liteavd 启动的 `-share-vid`/capture，也不会出现空的伪视口。

每个 interactive viewport 同时取得该 managed session 的认证 `GrpcClient` 与弱 `InputRouteGuard`。每个 `InputJob` 固化 AVD name + session id + generation；GTK 入队、worker 发 RPC 和安全重试前都复验 registry，替换/停止 session 会取消并 drain 旧事件。GTK 主线程只做坐标变换与入队；一个专用 current-thread Tokio worker 串行发送输入。tonic `Channel` 的 transport task 属于创建它的 Tokio runtime，因此 worker 保留 endpoint/auth 配置并在自己的长存 runtime 内重连，不跨 runtime 复用 launch 阶段的 channel。transport 断开后只自动重放完整绝对状态的 mouse/touch；结果未知的键盘事件不重放，避免双击或重复文本。

主窗口另持有一个 GTK 无关的 `AudioController`，每 50ms 对照 workspace 的 focused exact route。它只为 managed/recovered session 建立认证 `streamAudio`，请求固定使用 `MODE_REAL_TIME`，让 Emulator 覆盖旧队列而不是把积压变成可闻延迟；adopted session 没有私钥时显示 unavailable。焦点、route 或 Running phase 失效会先清 buffer 再取消旧流。CPAL/PulseAudio output sink 在有效 managed 焦点之间复用，旧 pump 完成清空和取消后才把 sink 移交给新 route，避免每次握手造成的切换抖动；无有效焦点、stop、禁用或应用退出会在 worker 上关闭 sink。snapshot load 成功会递增 `control_stream_revision`，即使 session identity 不变也会主动重建长流。PCM 固定验证为 48kHz/stereo/S16LE 后进入 120ms 总容量的 latest-audio buffer：新 exact route 收到 5ms 有效样本即可低延迟首响，发生欠载后则必须重新积累 60ms 才恢复；route 切换用互斥的 5ms 淡出/淡入避免爆音和新旧设备重叠。CPAL 请求固定 480-frame（10ms）callback；callback 只 `try_lock`、缩放或填静音，不执行网络、GTK、阻塞等待或动态分配。传输失败保持独立音频状态并按 transient/permanent 分类重连，不修改输入/截图健康状态。

虚拟麦克风不使用 Emulator `injectAudio`。每个 managed session 在 JWT 私有 runtime 目录创建 mode 0600 FIFO，并通过 `pactl module-pipe-source` 暴露唯一 source；guest 扬声器侧固定路由到同 session 的 null sink，避免为了启用 Emulator 音频引擎而重复播放输出。Linux headless Emulator build 不含 PulseAudio，因此有端点的 session 使用普通 Emulator binary + `-qt-hide-window`；HeadlessSwangle 同时设置 offscreen Qt，DesktopHost 继承现有 XWayland，两者都不创建 Xvfb。Emulator 只打开这个空的私有 source，启动返回前用认证 `setMicrophoneState(false)` 并复验；用户显式开启后，宿主 CPAL input 或流式 WAV producer 才按实时节拍写 FIFO。全应用 coordinator 串行 enable/disable，焦点、exact route、控制 revision、stop 或取消任一变化都会在 20ms 内停止 producer 并关闭旧状态。Pulse module id、source/sink identity 和 FIFO 路径写入同一私有恢复目录；应用退出可随 JWT lease 保留，显式 stop 则在后台卸载并删除。

主窗口使用 `FlowBox` 作为单窗口产品工作区：卡片最小宽度 300px、最多三列，在窄分配下回落为单列。FlowBox selection 与 viewport accent 显示当前焦点，点击或 Ctrl+1…9 更新 exact session route。每张卡片的响应式快捷区提供返回、主页、最近任务、电源、guest 音量、截图、宿主麦克风开关和 WAV 选择/拖放；键盘可对 focused card 使用 Alt+方向/导航、Ctrl+音量和 Ctrl+Shift+S。每个按钮事件在入队前固化 session route，同一卡片的硬件键串行执行；adopted/停止/替换 session 不借用其他控制面。麦克风来源默认关闭且不持久化，开启时持续显示隐私状态；宿主 Toggle 只表示 host source，WAV 文件按钮在播放期间切换暂停/继续，独立停止按钮取消任一来源。路由 identity 同时标在 viewport widget 上，session 替换只重建对应 capture/input；普通广告刷新和另一设备停止不会重建未变设备。

APK 与 guest 文件操作共用冻结目标和确认后二次授权。单 APK 使用 `install -r -t`，用户明确多选的全 `.apk` 集合使用 `install-multiple -r -t`；`-d/-g` 只来自确认页开关，不解析 AAB/APKS/XAPK。普通文件固定进入 `/sdcard/Download/liteavd/`，保留清理后的扩展名，以 operation id/index 生成唯一名称；每台设备先把全部文件流式 `adb push` 到 `.part`，再用 `mv -n` 发布并复验，不覆盖已有目标。用户取消或 route 失效会 kill+wait 当前 adb；只有 exact route 仍有效时才尽力清理本 operation 的 staging/已发布文件，避免端口复用后误触新实例。stdout/stderr 各只保留最后 64KiB，逐设备结果与阶段事件顺序确定。

普通 key/down 队列硬限制为 128，连续 motion 只保留最新一个；release/cancel 不受普通上限影响，drag 结束前会把最后 motion 提升到可靠 FIFO，防止卡触点或快速滑动退化。focus lost/detach 会补齐 touch pressure=0、key-up 和右键 release。

`PortAllocator` 在 spawn 前同时考虑广告文件、registry session 和尚未出现广告的 reservation。managed session 在 boot 完成后接管 reservation，直到 stop 成功或 session 被确认消失才释放；失败的 stop 保留 session 与 reservation。adopted session 不伪造 reservation，但 registry 中的端口仍参与后续分配。

`LaunchScheduler` 在端口分配之前按 FIFO 授予启动许可。默认配置只允许一个 AVD 处于 launch/boot，boot 完成即释放启动名额；设置可把并发改为 1–4，并启用内存/host-GPU slot 预算，预算继续由 `ResourceReservation` 随 managed session 持有。排队项显示当前位置和阻断原因，可在 spawn 前取消；permit、session、stop、启动失败和进程消失均通过 RAII 释放并唤醒队首。内存需求当前取 AVD `hw.ramSize`，缺失/非法/零值按保守的 2048MiB 计算；managed `HeadlessSwangle` 不占 host GPU slot，`DesktopHost` 与 adopted `hw.gpu.mode=host` 各计一个 slot。设置不会擅自缩小 RAM 或切换 GPU；两种 managed policy 都必须由用户显式选择。

### SDK 安装链

```text
Google XML
  → Repo::parse
  → 选择 archive + license
  → 用户显式许可
  → PackageService 按 checksum/URL hash 定位稳定 cache
  → Downloader 写 archive.zip.part（可跨进程续传）
  → checksum / size 验证
  → rename 为 archive.zip；复用前重新流式复验
  → 获取每组件跨进程 flock
  → install_component 解压 staging
  → 旧组件 rename 为 backup
  → 新组件 rename 到目标并验证
  → 删除 backup；失败则回滚
```

校验和必须服从 XML 的 `type`：实际仓库同时使用 SHA-1 与 SHA-256。license 只有在 ID 和规范化文本 SHA-1 都匹配时才是已接受，同 ID 的历史文本 hash 保留为多行。官方 zip 可能带单层包装目录且携带 Unix mode。

下载 cache 上限来自 settings，默认 8192MiB。`PackageService` 在每个 key 的下载 lease 外持有 cache 根配额 lease，因此不同 key 不能并发越过同一个容量判断；空间不足时按目录修改时间清理最旧且能取得 lease 的 `archive.zip`/`.part` 条目，正在使用的条目和当前 key 不删除。仓库没有给出归档大小时拒绝新下载，避免先超限再补救。

### 设置持久化

`settings.toml` 当前 schema 为 v1；无 `schema_version` 的旧 SDK-only 文件按 v0 读入，只有用户显式保存时才升级。文件读取限制为 1MiB 并拒绝符号链接/非普通文件；解析、版本或校验失败会保留原文件、返回安全默认值，并在启动诊断与设置页显示原因。保存使用 0700 目录、同目录 0600 `create_new` 临时文件、file fsync、rename 和 directory fsync；rename 前失败由 guard 清理临时文件且不改旧设置。

有效的 `AVDM_SDK_ROOT` 高于设置文件并在 UI 中作为只读覆盖显示。Flatpak 内的新路径必须位于应用私有 data 根；外部路径必须已经通过 filesystem grant 可见且包含 Emulator，避免继承的宿主变量把托管安装导向非持久沙箱视图。启动并发、内存预算与 host-GPU slot 在创建应用级 `DeviceRuntime` 时固定，修改后下次启动生效；managed GPU policy 在 scheduler 没有排队、managed 或 external allocation 时可保存后热更新，只影响后续启动，否则保持旧 runtime 并在下次启动应用时生效。cache 配额由新建 `PackageService` 读取，应用日志级别可在保存后立即生效。managed GPU 明确分为默认的 `HeadlessSwangle` 与用户选择的 `DesktopHost`，两者共享相同 registry/session/share-vid 路径，不存在自动 backend fallback。

### SDK 入口

- 接管模式：读取用户指定的现有 SDK/AVD，liteavd 不调用 `sdkmanager`、`avdmanager` 或 Java；
- 托管模式：liteavd 自行完成 repository、许可、下载、安装和 AVD 文件生成。

两种模式从 AVD 发现开始汇合，必须共用相同的 registry、scheduler、session 和视口。SDK 来源不能渗入运行期状态机。

### AVD 与实例身份

向导只展示同时含 `system.img` 与 `source.properties` 的完整系统镜像；从镜像管理安装/卸载返回时，以 `api/tag/abi` identity 恢复选择，最终提交前再次复验。profile catalog 与创建硬件默认值各自携带 schema version，GTK 不持有业务默认常量。

创建先取得每 AVD 的非阻塞跨进程锁，在同一 AVD root 写入并 fsync 0700 staging 目录与临时 `.ini`，再用 `RENAME_NOREPLACE` 发布；任一发布/同步失败由 guard 同时清理 staging、`.avd` 和 `.ini`。同名 `.ini` 或 `.avd` 已存在时不会覆盖，列表也不投影缺少 `config.ini` 的半成品。删除取得同一锁并先检查存活广告中的 AVD name，运行中实例必须先停止。

不同字段不能混用：

| 身份 | 用途 |
|---|---|
| AVD name | 持久配置与广告文件匹配 |
| console port | adb serial `emulator-<port>`、share-vid shm 名与运行实例主键 |
| adb port | 广告字段，不是 adb serial |
| gRPC port | localhost 控制端点 |
| qemu engine PID | 广告文件名与最终终止目标 |
| launcher PID | `Command::spawn` 返回值；通常不是 engine PID |

当前启动流程在 spawn 前拒绝同名实例、占用端口和推荐范围外端口；等待广告文件时同时匹配 AVD name 与 reservation console port。registry 只有在 AVD + console port + engine PID 全部一致时才刷新原 session，端口被新 PID 复用时会创建新 session。广告文件先过滤 dead/zombie PID；收养和停止进一步要求 `/proc/<pid>/exe` 位于当前 SDK，且 exe/cmdline 具有 emulator/qemu 特征。pending launch 持有 launcher child，取消或失败会清理 engine/launcher；成功后 reaper 回收 launcher，session 保存其 PID、SDK 与日志路径。`share_vid=true` 时 launch 显式传 `-share-vid`，认证 gRPC 验证后启动可迟 attach 的 capture，session 与 capture 同生共灭。早期 host-GPU spike 没有显式 `-grpc`，其“无广告”现象不能归因于 GPU mode；正式 JWT/gRPC 启动路径已在桌面 XWayland 下正常产生广告并进入同一 session 生命周期。

gRPC session 目录使用 `<pid>-<key-id>` 命名，identity drop 时删除。为覆盖崩溃和测试进程提前退出，新 identity 创建时会在私有的 mode 0700 父目录内回收 PID 已不存在的旧目录；当前 PID、仍存活 PID、无法识别的名字和符号链接均保留，避免越界删除。

### gRPC 安全边界

Emulator 37.1.11 的实测与正式策略已经固定：

- 同时使用显式 `-port` 而省略 `-grpc` 时，5–30 秒内没有 gRPC 广告文件，不能满足 liteavd 的发现模型；
- 仅传 `-grpc <port>` 会产生 wildcard listener（实测 `*:8571`），匿名 `getStatus` 成功，不能作为产品配置；
- 正式启动固定传 `-grpc <port> -grpc-use-jwt -grpc-allowlist <file>`；实测 listener 为 IPv4-mapped loopback，匿名与未注册密钥都被拒绝。

每个 managed session 生成独立 ES256 私钥；运行期密钥在内存中签名。只有 engine 身份与 active JWK 已验证后，才在用户私有 runtime 目录写入 mode 0600 的 PKCS#8 恢复副本和身份 record，并持有非阻塞独占 `flock`。GUI/runtime 退出不 kill engine，因此仍在 registry 的 session 保留该副本；下一进程必须同时匹配 engine PID、AVD、console/gRPC 端口与 active key，才能恢复为 `SessionOrigin::Recovered`。显式 stop、crash 或 session 回收会删除私钥、公钥和目录；另一个仍持 lease 的 liteavd 进程不能被抢占。

公钥以 mode 0600 的 JWKS 文件写入广告文件的 `grpc.jwks` 目录，并等待 `grpc.jwk_active` 确认。allowlist 只开放当前状态、截图、snapshot list/save/load/delete、键鼠/触摸、`streamAudio` 和麦克风状态 get/set；不含会在固定 Emulator + VirtioSnd 下崩溃的 `injectAudio`。`GrpcClient` 固定连接 `127.0.0.1`；一般 unary 使用 10 秒 deadline，输入使用 2 秒 deadline。`streamAudio` 请求 `MODE_REAL_TIME`，只对 response/首包建立阶段使用 5 秒 timeout；建立后的长流不携带会中途截断的 `grpc-timeout`。恢复 session 先持有不绑定 Tokio reactor 的连接配置，实际 screenshot/input/snapshot/audio worker 在长存 runtime 内重连。

真正 adopted 的外部实例没有 liteavd 私钥，当前只能发现和基于已验证 PID 停止，不能假定可调用其 gRPC；它与可验证恢复的 `Recovered` origin 不混淆。升级 emulator 后必须重跑 `grpc_auth_spike`，因为监听和 JWK loader 都是外部协议事实。

## 4. 目标架构

WP-1.2–1.5 已建立并验证 capture/session/viewport/input 边界，WP-2.1–2.4 已完成资源调度、多视口、跨设备操作、重启恢复和带故障三设备 30 分钟门禁，M2 已关闭。长期边界如下：

```text
                      ┌──────────────────────────┐
UI actions/events ───▶│ AppState / Commands      │
                      └────────────┬─────────────┘
                                   │
               ┌───────────────────┼──────────────────┐
               ▼                   ▼                  ▼
       InstanceRegistry        Scheduler        OperationService
       生命周期/状态流      端口/内存/GPU/队列    APK/截图/快照
               │                   │                  │
               └──────────────┬────┴──────────────────┘
                              ▼
                       EmulatorSession
                 process + adb + gRPC + capture
                              │
                    ┌─────────┴─────────┐
                    ▼                   ▼
              ShareVidCapture      GrpcInput
                    │                   │
                    └─────────┬─────────┘
                              ▼
                         GTK Viewport
```

- `InstanceRegistry` 持有每个设备的持久状态机：`Stopped → Queued → Starting → Booting → Running → Stopping`，并保留错误而不是立刻重建丢失。
- `Scheduler` 已原子预留 console port 和可选资源预算；任务取消、启动失败、stop 和进程退出都会释放 reservation。后续只扩展持久设置与诊断，不在 UI 重写规则。
- `EmulatorSession` 是运行实例句柄，统一持有 engine identity、控制端点、capture 生命周期和日志。
- `ShareVidCapture` 只发布最新完整帧；UI 慢时覆盖旧帧，不能无界排队。
- UI 订阅状态和帧，不自行扫描文件系统或决定资源规则。

## 5. share-vid 生产边界

已验证布局记录在 [VALIDATED_FACTS.md](VALIDATED_FACTS.md)。当前生产 reader/capture 已满足：

1. 打开 `/dev/shm/videmulator<console-port>` 后检查至少 24 字节；
2. 使用 checked arithmetic 计算 `24 + width * height * 4`，设置合理的宽高/总大小上限；
3. 映射长度与 header 一致后才读取像素；
4. counter 变化时执行一致性读取：读取 counter/header、复制像素、再次读取 counter，不一致则丢帧重试；
5. `FrameMeta` 显式携带 BGRA stride；viewport 把它传给 GDK texture，并在实际 buffer 尺寸变化时重建纹理；
6. capture 停止、shm 残留或实例重启时可重新 attach；
7. capture 线程不得持 GTK 对象。

capture 使用容量 1 的 latest-frame 槽；消费者未读取上一 sequence 时，新帧覆盖旧帧并增加 dropped 统计。`CaptureHandle` 负责 attach/reconnect/cancel，`EmulatorSession` 负责其所有权，GTK 不接触 mmap。viewport 在 4ms GTK main-context source 中调用 `take_latest()`，未 mapped 时暂停上传；该 source 与消费槽各只有一个，不会积压每帧 callback。视口以 unpremultiplied `B8g8r8a8` 和显式 stride 创建 `GdkMemoryTexture`；`glib::Bytes` 持有原 `Arc<Frame>`，所以 texture 生命周期内像素不会悬空。

端到端延迟指标必须定义测量点。`LatencyProbe` 用同进程 `Instant` 记录 queue、RPC、RPC completion→counter observation、帧复制和 `GtkPicture::set_paintable` 提交；对同一新 counter 之前已完成的所有输入结算，pending 与样本均有硬上限。KPI 的起点是输入 RPC send，终点是新 texture 设入 `GtkPicture`；它不声称测到显示器 scanout/photon。Emulator 37.1.11 的两次 500 样本正式测量均低于 p95 `<50ms`：Fedora 为 p50 21.501ms / p95 38.441ms / p99 44.019ms；CachyOS 的 30 分钟单设备门禁为 p50 13.582ms / p95 43.695ms / p99 50.404ms，单次 copy p95 1.445ms，0 failure/pending/drop。

CachyOS 三设备 30 分钟门禁使用三个独立 managed session/viewport，而不是尚未实现的产品工作区：三路分别提交 7419/7363/7289 帧，capture 各发布约 9.6k 帧，0 attach retry、0 unstable、无 capture error；三路最大 UI pump gap 约 126.2ms。测试进程 RSS 从 666.9MB、峰值 730.6MB 到结束前 559.1MB，thread 62、fd 52 无增长，随后三台实例及其端口、shm、认证材料和 AVD 全部清理。

### 显示 broker 与 GPU 策略

`-share-vid` 是像素交换接口，不要求 X server。没有麦克风 endpoint 时，默认的 `HeadlessSwangle` 使用 `-no-window -share-vid -gpu swangle_indirect`；已预置 endpoint 时因 Linux headless Emulator 不含 PulseAudio，改用普通 binary + `-qt-hide-window` 与 `QT_QPA_PLATFORM=offscreen`。两条都能在没有 `DISPLAY`、没有 Xvfb 的环境启动；liteavd 自身的正常窗口直接运行在 Wayland。

`DesktopHost` 是独立的产品策略，而不是原始 GPU mode 别名。无麦克风 endpoint 时使用 `-no-window -gpu host`；有 endpoint 时使用隐藏的普通 binary，并显式设 `QT_QPA_PLATFORM=xcb`，因为固定 Emulator 的 Qt bundle 不含 Wayland plugin。两者都在分配端口或 spawn 前要求继承非空 `DISPLAY`（桌面 Wayland 会话通常由 XWayland 提供）；认证 gRPC 成功后再次验证 SDK 内 engine 身份、至少一个已打开的 `/dev/dri/*` fd，且进程 maps 中没有 SwiftShader、llvmpipe、swrast 等已知软件 renderer。任一条件失败都会沿 managed launch 回滚停止，绝不自动切换到 swangle。该策略占用一个可配置 host-GPU slot；默认仍为不占 host slot 的 `HeadlessSwangle`。真无头环境不启动 Xvfb，而是明确拒绝 `DesktopHost`。Xvfb 仅保留给 synthetic GTK 门禁和历史实验，不是应用依赖。

## 6. 交付边界

GitHub Actions 使用锁定 commit 的 checkout，在 Ubuntu 24.04 上运行两个 job：Rust 1.88 core-only test/strict Clippy 与 stable GUI fmt/hermetic tests/Xvfb smoke/strict Clippy。真实 SDK、模拟器、GPU 和长时间测试需要受控主机，仍保持 ignored/manual；公网 CI 不假定 KVM 或接受 Google 许可。

vendored proto 固定来自 Emulator 37.1.11.0 build 15917651；来源、逐文件哈希和生成命令记录在 `proto/README.md` 与 `proto/SHA256SUMS`。升级 proto 必须更新这两个文件并重跑认证 gRPC 集成测试。

Flatpak 是唯一计划发布格式，当前渠道选择为 GitHub Releases 的单文件 bundle，Flathub 延后。`v*` tag 工作流会校验 Cargo/AppStream 版本、构建和安装验证 bundle、校验 SHA-256，并只创建待维护者检查的 draft prerelease。持久化决策已关闭：托管 SDK/AVD 默认放在应用私有 `$XDG_DATA_HOME/liteavd/`，不请求 home/host 权限；接管宿主 SDK 只通过用户对 SDK/AVD 确切路径的显式 override。沙箱外已运行的 Emulator 因 `/proc` 身份隔离不当作可控 adopted session。

GNOME 50 manifest 的静态权限只包含 Wayland、X11、PulseAudio、IPC、DRI、KVM 和 network。Wayland 用于 GTK 主界面，X11 只为用户显式选择的 Emulator `DesktopHost` 提供现有 XWayland display；PulseAudio socket 同时承载 focused guest 输出、用户显式开启的宿主输入和每 session 私有 FIFO source，没有 home/host 文件系统或 Xvfb 运行时。Pulse module 运行在宿主服务，因此 FIFO 不放在 mount namespace 私有的 auth 目录，而是放在宿主/沙箱共享且仅本用户可访问的 `$XDG_RUNTIME_DIR/app/io.github.ydog12138.liteavd/`；文件仍为 0600，名称携带 owner PID/session key，恢复元数据与死 PID 规则保护正常恢复并清理孤儿。CPAL 的纯 Rust Pulse backend 在 Flatpak 私有 HOME 无宿主 cookie 时会发送零长度 auth blob，PipeWire-Pulse 返回 `Invalid`；端点创建前仅在 Flatpak 私有配置中 no-clobber 创建 mode 0600 的 256B 零值占位，server 仍以同 UID socket peer credential 鉴权。已在空白私有 data 的沙箱内验证用户显式接受真实 Google 许可、下载/安装/创建、`/dev/kvm`、私有 `/dev/shm`、XDG runtime、localhost adb/gRPC、广告文件、`share-vid`、模拟器子进程、GTK 显示/输入与停止；production swangle 链不需 Xvfb。APK/file 部署已通过安装 Flatpak 的真实 chooser/drop→guest、逐文件 portal 授权和撤销，以及隔离三设备的部分失败、取消与 256MiB 有界资源门禁，WP-3.6 已关闭。guest 输出已通过两种 GPU policy 的沙箱短链、人工听辨、20 事件音画 p95 160ms、三设备焦点/故障与最终 HeadlessSwangle 30 分钟门禁，WP-3.5 已关闭；虚拟麦克风也已通过 GNOME 50 安装构建、端点恢复、CPAL input、真实 guest/portal WAV、宿主说话人工回放、三设备故障和 DesktopHost 30 分钟资源门禁，WP-3.7 已关闭。固定 Emulator 的重复 Google Clock timer + swangle 崩溃保留为上游特定限制。GitHub `v0.1.0` prerelease 已完成线上构建/安装、checksum 与本机下载重装验收，WP-4.3 已关闭。
