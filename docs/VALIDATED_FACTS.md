# 已验证的外部事实

本页只保存会影响实现、但不能从 Rust 类型本身推出的环境/协议事实。2026-08-06 至 2026-08-09 的原始矩阵使用 Fedora 44、RX 7900 XTX/RADV；2026-08-10 的复验使用 CachyOS kernel 7.1.6。两者均使用 Android Emulator 37.1.11。升级 emulator、Mesa、GTK 或 GStreamer 后必须重验相关项。

## Android repository 与安装包

- 组件索引为 `https://dl.google.com/android/repository/repository2-3.xml`。
- 系统镜像没有可用的根 `sys-img2-3.xml`；索引位于 `sys-img/<tag>/sys-img2-3.xml`。当前代码读取 `google_apis`、`google_apis_playstore`、`aosp_atd`。
- archive URL 通常是相对路径。组件相对 repository 根；系统镜像相对 `sys-img/<tag>/`。
- checksum 类型不能写死：已见 emulator/系统镜像使用 SHA-1，platform-tools 使用 SHA-256。
- 官方 zip 可能有 `emulator/` 或 `platform-tools/` 单层包装目录，且可执行权限来自 zip 的 Unix mode。
- 零 Java真实安装链曾完成 emulator 15917651、platform-tools 37.0.1 与 android-35/google_apis/x86_64 r09。可重复入口是 `tests/install_chain.rs`，但它下载约 2GB 数据，默认 ignored。

## 广告文件、端口与进程

- 广告目录为 `$XDG_RUNTIME_DIR/avd/running`，回退 `/run/user/$UID/avd/running`；文件名为 `pid_<qemu-engine-pid>.ini`。
- 已见字段：`avd.name`、`port.serial`、`port.adb`、`grpc.port`、`grpc.allowlist`。JWT 模式还包含 `grpc.jwks` 与 `grpc.jwk_active`；显式无认证模式没有 `grpc.token`。
- emulator launcher 与 qemu engine 是不同进程；广告文件以 engine PID 命名，不能用 `Child::id()` 推导运行实例。
- adb serial 是 `emulator-<console-port>`，不是 adb port。
- console port 的可用范围是 5554–5586、步长 2。超出该范围时 adbd 行为不可靠。
- 正常退出通常删除广告文件；崩溃可能遗留，至少要用 `/proc/<pid>` 与进程身份过滤。
- 2026-08-09 在 Emulator 37.1.11 重跑 managed JWT 链路：production launch 创建 mode 0600 的有界日志，认证状态/截图/快照成功；停止后 engine、launcher 和 session JWK 目录均无残留。launcher 与 engine 仍须作为两个独立 PID 清理。
- 2026-08-10 的 30 分钟测试暴露“进程退出前 GLib source 仍持有 client/identity”会留下 gRPC auth 目录。测试现在显式释放 UI/client 并断言目录消失；production identity 创建还会在 mode 0700 私有父目录内清理 PID 已不存在的 session 目录，同时保留活 PID、当前 PID、无法识别项与符号链接。对应 hermetic 回归已通过。
- Emulator 收到 `SIGTERM` 后不会保证删除 share-vid shm，曾观察残留超过 45 秒；同端口重启可以覆盖复用该对象。因此 liteavd 的 exact managed stop、启动回滚和 session drop 必须在确认对应 engine 退出后按 console port 删除该 shm，而不能把协议端清理当作保证。

## gRPC

- 验证入口为 `tests/grpc_auth_spike.rs`，Android Emulator 37.1.11 / build 15917651，2026-08-09 复验通过。
- 上游语义交叉核对：[Android Emulator gRPC security](https://android.googlesource.com/platform/external/qemu/+/emu-master-dev/android/android-grpc/docs/README.md) 与 [qemu-setup.cpp](https://android.googlesource.com/platform/external/qemu/+/emu-master-dev/android-qemu2-glue/qemu-setup.cpp)；本页仍以固定二进制的真实矩阵为验收依据。
- 使用显式 `-port` 且省略 `-grpc` 时没有广告文件；正式发现路径不能依赖 emulator 的隐式 gRPC 默认值。
- 只传 `-grpc 8571` 时 listener 为 `*:8571`，没有 token，匿名 `getStatus` 成功。这是明确禁止的负向配置。
- `-grpc 8571 -grpc-use-jwt` 时 listener 为 `[::ffff:127.0.0.1]:8571`；广告包含用户私有 `grpc.jwks` 目录与 `grpc.jwk_active` 文件，匿名请求被拒绝。
- ES256 公钥文件必须是 JWKS，即 `{"keys":[...]}`，而不是裸 JWK；模拟器 active 文件出现对应 `kid` 后才可发请求。该版本 watcher 未在 5 秒内响应“临时文件 rename 为 `.jwk`”，直接 create/write `.jwk` 可用，因此 active 确认不能省略。JWT header 不能携带 `typ`，否则 Tink validator 返回 `InvalidArgument`。
- liteavd 自定义 issuer/allowlist、正确 ES256 JWT 已真实调用 `getStatus`、PNG `getScreenshot` 与 `listSnapshots` 成功；匿名 token 和由未注册密钥签名的 token 均被拒绝。所有正式 unary 请求携带 10 秒 `grpc-timeout`。
- `getStatus().booted`、`getScreenshot(Png)` 和 `listSnapshots` 在真实模拟器上调用成功；PNG magic 已验证。
- snapshot proto 生成到 crate root 的 `emulator_snapshot` module；嵌套 enum（如图片格式）位于 prost 生成的子 module。
- 当前成功证明 JWT 下的状态、PNG 截图、快照读写、`sendKey/sendMouse/sendTouch`、`streamAudio` 与 `get/setMicrophoneState`。`injectAudio` 已做负向实测并因固定版本崩溃/不达 guest 而明确排除在 allowlist/client 外。

## 音频输出

- 2026-08-10 在 CachyOS、Emulator 37.1.11 / build 15917651、Pixel 2 / android-35 google_apis x86_64 上，唯一隔离测试 AVD 保留 production `-no-audio`、不使用 `DISPLAY`/Xvfb/`share-vid`，认证 `/android.emulation.control.EmulatorController/streamAudio` 成功返回 48kHz/stereo/S16LE：首个有效 packet 132B，其中 95 个非零 byte。测试先通过 exact adb serial 安排 2 秒 guest timer；该版本在首个音频 packet 可用前不会完成 server-streaming response future，因此 5 秒建立 timeout 同时覆盖 response header 与首包等待。总测试 24.27 秒，结束后隔离 AVD、engine、端口与认证材料均清理。
- CPAL 0.18.1 的显式 PulseAudio backend 在 CachyOS 宿主 PipeWire-Pulse 上可创建 48kHz/stereo/i16 输出并由 callback 消费预缓冲 PCM。进入 Flatpak 沙箱且私有 HOME 没有 Pulse cookie 时，同一 backend 首次返回 `server error: Invalid`：socket 存在并可连接，但纯 Rust `pulseaudio` 0.3.1 把缺失 cookie 编码为零长度 blob。应用在 Flatpak 私有 HOME no-clobber 创建 mode 0600 的 256B 零值占位后，同一 GNOME 50 build sandbox 测试通过；现有 cookie 的长度、普通文件类型和权限会复验且绝不覆盖。该占位不授予新身份，PipeWire-Pulse 仍以本地 socket 的同 UID peer credential 鉴权。
- core 当前只接受单包不超过 64KiB 且按 4B stereo frame 对齐的 PCM，buffer 总容量固定为 120ms。固定 60ms 首缓冲在真实三设备轮换中暴露小首包边界：250ms 内已收到 1,222 或 1,700 samples，但尚未达到 5,760 samples，因而未起播。当前新 route 用 5ms 样本低延迟首响，第一次欠载后仍要求重新积累完整 60ms；route clear 先输出 5ms 旧尾淡出，再对新 PCM 做 5ms 淡入，二者不并发。同一隔离 AVD 门禁把 session 提交给产品 `DeviceRuntime`，由 focused `AudioController` 驱动 CPAL callback：收到 9,864 samples、播放 4,096、丢弃 0、callback contention 0，总测试 27.40 秒且 exact stop/清理通过。
- 2026-08-10 三台唯一隔离 managed AVD 的无 Xvfb 短门禁在最终双阶段缓冲下通过。测试让每台 guest 定时器产生音频，并在切换前用独立认证流确认目标至少有 60ms 连续且非零的 active PCM；headless-swangle 65 秒校准的 focused handoff 为 71ms/71ms、stop-phase fail-stop 恢复 52ms，三次 soak handoff p95 76ms，0 drop/0 callback contention。三台最终由一个 all-running exact operation 停止并清理 AVD/engine/端口/认证材料。该测试证明 active-source 条件下的 `<250ms` route handoff、stop-phase 清理和自动去爆音的数据路径，不是人工听觉上的无重叠或音画事件延迟测量。
- 三设备 1800 秒资源/焦点门禁在 `DesktopHost` 通过，全程未使用 Xvfb。60 次 active-source 轮换的 handoff p95 为 60ms；liteavd 进程 RSS 从 14,237,696B 起，峰值 20,287,488B，结束 18,903,040B，线程 19→峰值22→19，fd 31→峰值34→31。切换点累计观测 226,488 received / 192,376 played、0 dropped、25 callback contention；固定 buffer 始终不超过 120ms，竞争按设计填静音。总测试 1,859.04 秒，三台 exact stop 与隔离 AVD/engine/端口/认证材料清理通过。
- 同一 1800 秒门禁的 `HeadlessSwangle` 尝试两次，固定 Emulator 37.1.11 的 qemu engine 分别在 881.61 秒和 675.58 秒收到 SIGSEGV；systemd coredump 为约 943–953MiB，主机仍有约 20GiB available memory，没有 OOM 记录，崩溃进程包含 SwiftShader/gfxstream 线程。liteavd 在崩溃前已完成 26/20 次轮换，线程/fd 可回落且 RSS 分别约 18.8/20.3MB。短链和既有无音频三 viewport 30 分钟 swangle 门禁均通过，因此当前证据只把问题限定为该固定 Emulator 的重复 timer/stream/swangle 长压组合，不能宣称 headless 音频长稳已关闭。
- 后续听辨证明 Google Clock 会在不同铃声 URI 间缓存旧 Oxygen，Buzzer 本身也含不连续段，不能作为 exact route 的可闻来源。新增无权限、testOnly `AudioTrack` fixture 以每 guest 唯一的 440/660/880Hz 连续正弦替代；用户按低→中→高→低确认三个 focused route 无旧音残留，应用级音量与两个独立的播放/静音控制均正常。测试 APK 的源码、manifest、官方 Build Tools 35.0.1/Platform 35 构建说明和 SHA-256 均已纳入仓库。
- `streamAudio` proto 的默认 `MODE_UNSPECIFIED` 明确允许 client 落后；产品现固定请求 `MODE_REAL_TIME`，CPAL output 从默认约一秒 target buffer 先降到 960-frame（20ms），再以 480-frame（10ms）满足音画预算。20 个 fixture 同步事件通过 production `share-vid` 与产品 CPAL→隔离 Pulse sink monitor 实测；fixture 用 `AudioTrack.getPlaybackHeadPosition()` 对齐 guest 实际首个新音调 frame。DesktopHost 下音频相对视频为 115–172ms，p95 160ms、max 172ms，低于 180ms 目标；测试总计 46.34 秒并完成 exact cleanup，全程没有 Xvfb或宿主可闻输出。
- 最终 `MODE_REAL_TIME` + 10ms callback 的三设备 `HeadlessSwangle` 1800 秒门禁完成 60 次轮换，handoff p95 35ms；liteavd 测试进程 RSS 为 14,442,496B 起、17,973,248B 峰值、17,969,152B 结束，线程 19→峰值22，fd 31→峰值34，累计 74,764 received / 57,678 played、0 dropped、10 callback contention。总测试 1,869.79 秒，三台 exact stop，engine/端口/auth/fixture/临时 AVD/null sink 全部清理；它越过旧 675.58/881.61 秒窗口且没有 SIGSEGV。旧失败因此保留为固定 Emulator 的重复 Google Clock timer + swangle 上游特定限制，不再代表确定性产品音频长压。
- 最终 GNOME 50 Flatpak release commit `d84ed323c39a361d49ef682dea79b276a9ad1ed78c8e44eaecde26967a65645f` 已离线 build、AppStream compose、export 与 user reinstall；权限仍为 `shared=ipc;network; sockets=pulseaudio;wayland;x11; devices=dri;kvm;`。最终源码 locked 全 targets、strict Clippy、Xvfb 音频控件与 Rust 1.88 core-only 均通过。

## 虚拟麦克风

- Emulator 37.1.11 / build 15917651 的默认 Android 35 google_apis x86_64 image 开启 `VirtioSndCard`。该版本 `injectAudio` 只通过 HDA codec 注册 audio forwarder：默认配置调用认证 RPC 会在 `AudioStreamCapturer/QemuAudioInputStream` 路径解引用空 audio-forwarder 并使 qemu engine SIGSEGV。强制 `-feature -VirtioSndCard` 可避免崩溃，但 guest `AudioRecord` 仍只收到 Emulator 内建约 220Hz fake mic，注入的确定性波形没有进入 Android。因此这不是可通过 client pacing 修复的产品链。
- 官方 Linux headless Emulator build 不含 PulseAudio；`-no-window` 会选择该 build，设置 `QEMU_AUDIO_DRV=pa`/`-audio pa` 仍报 backend 初始化失败。普通 Emulator binary 使用 `-qt-hide-window` 可连接 Pulse；HeadlessSwangle 显式设 `QT_QPA_PLATFORM=offscreen` 后不需要 DISPLAY/Xvfb，DesktopHost 显式设 `QT_QPA_PLATFORM=xcb` 并继承现有 XWayland，因为固定 SDK 的 Qt bundle 没有 Wayland platform plugin。
- 当前 PipeWire-Pulse 1.6.8 可加载 `module-pipe-source source_name=<session-unique> file=<0600-fifo> format=s16le rate=48000 channels=1`，Emulator 37.1.11 通过非标准 `QEMU_PA_SERVER`、`QEMU_PA_SOURCE` 与 `QEMU_PA_SINK` 精确连接该 source 和私有 null sink。启动参数保留 `-allow-host-audio`，但该 source 本身不包含宿主输入；production launch 在返回 session 前通过认证 gRPC 强制设置并复验 `realAudioEnabled=false`。
- `tests/microphone_chain.rs` 在 CachyOS + KVM + 唯一隔离 AVD 中，把 5 秒 1kHz、48kHz mono S16 WAV 经 production share-vid、`MicrophoneCoordinator`、私有 FIFO 和认证状态 RPC 送入 guest fixture 的 `AudioRecord`。guest 取得 434,176B PCM；最佳 1kHz 幅值 23,958.8、RMS 16,941.4，700/1300Hz 最大旁带 0.1。测试随后销毁原 `DeviceRuntime`，由广告、JWT recovery lease、capture 和 Pulse metadata 恢复为 `Recovered` exact session，端点 identity 与关闭状态一致；总测试 26.32 秒，stop 后无 engine、端口、auth、shm、FIFO、Pulse module 或临时 AVD，且没有 DISPLAY 或 Xvfb。
- CPAL 0.18.1 的 PulseAudio input backend 在当前宿主和 GNOME 50 Flatpak finish-args 沙箱中均可创建并启动 48kHz/mono/i16 默认输入 stream。`BufferSize::Default` 在人工链中曾让 Pulse callback 晚于 120ms ring 消费窗口，guest 只能录到 peak -72.25dBFS、RMS -77.22dBFS 的近静音；改用固定 960-frame（20ms）callback 后，ignored 门禁在 500ms 内断言收到实际 frame，并由 2 秒首 callback watchdog 在设备异常时 fail closed。
- `gpu_host_production_xwayland` 在桌面 XWayland `DISPLAY=:1` 下把虚拟麦克风 endpoint 加入 production JWT/广告/share-vid/DesktopHost 链：20.36 秒内完成 boot、endpoint 默认关闭、capture、认证 screenshot 与 exact cleanup；engine 打开 5 个 `/dev/dri/renderD128` fd且没有已知软件 renderer。该普通 Emulator 使用显式 xcb，没有启动 Xvfb。
- 增强后的 `microphone_chain` 在同一真实 guest 中验证 control-revision reset 和 recovered stop-in-flight 都会在 3 秒界限内取消 pump、经认证 RPC 恢复默认关闭；注入的 stop 失败后 exact route 可重新 focus 并正常最终停止，总测试 27.35 秒。独立三设备 HeadlessSwangle 快速门禁为 68.96 秒：三路各持独立 endpoint/JWT，焦点 handoff 总计 9ms、新 source 状态等待 2ms，逐路认证 RPC 轮询未观察到同时启用；revision reset、显式取消、stop-in-flight、失败恢复与 survivor 再注入都通过。结束后没有 engine、Pulse module、FIFO、临时 AVD 或 shm 残留。
- 2026-08-10 同一三设备门禁以 `DesktopHost` 完成 1800 秒正式资源压测：60 次 exact source 轮换的 handoff p95 为 9ms；测试进程 RSS 为 12,009,472B 起、12,431,360B 峰值/结束，thread 31→31，fd 25→峰值28→25。总测试 1,857.37 秒，三台 engine 使用真实 host GPU 且没有 Xvfb；结束后 engine、测试端口、Pulse module、FIFO、临时 AVD/home 与 shm 全部无残留。
- 安装 commit `4b6ad4c9720d90fd1efffbe768b922a112d21e4d11bef06160766d1db2f75e44` 的 GTK chooser 通过 document portal 打开生成的 30 秒 PCM S16/48kHz/mono WAV，精确文件授权可见且没有宽泛 filesystem grant，guest 随后录到其 1kHz 波形。该试验同时暴露“WAV 播放中复用宿主 Toggle 会先停止 WAV”的歧义；最终 UI 让 Toggle 只表示 host source，WAV 按钮负责暂停/继续，并增加独立停止按钮。
- 同一最终安装包把 CPAL source-output 绑定当前默认物理输入 Razer Seiren Mini；维护者说话时 guest `AudioRecord` 得到 770,048B、约 8.02 秒 PCM，peak -8.064dBFS、RMS -26.156dBFS、RMS peak -14.934dBFS，直接回放由维护者确认可辨听。点击独立停止按钮后 CPAL source-output 完全消失，只剩 Emulator 对 session 私有 source 的 corked 连接；这验证停止会关闭真实宿主采集，且应用数据、日志和仓库没有持久化 PCM。

## 输入与旋转

验证入口：`tests/gui_viewport_real.rs`，Emulator 37.1.11 + Pixel 2 / android-35/google_apis/x86_64。

- 认证 gRPC 的 key、mouse、UTF-8 text、touch down/move/pressure=0 release 均已在真实 managed session 调用成功；GTK `GestureDrag` 经专用输入 worker 可改变 guest 画面。
- `adb emu rotate` 返回 `OK` 后，gRPC screenshot 报告 1920x1080、`Rotation.SkinRotation=3`；同一时刻 share-vid header 仍为 1080x1920 并继续递增 counter。
- 因此 share-vid 是当前输入物理坐标系；Android/设备姿态变化不保证交换 shm header 宽高。viewport 应按实际 buffer 做 contain 映射，不能根据 screenshot rotation 再旋转一次坐标。
- 快速 drag 的最后一个 lossy motion 必须在 release 前提升为可靠 FIFO 事件；否则 GTK 连续信号可能在 worker 消费前清掉 motion，使滑动退化为 down/up 点击。

## share-vid 协议

验证入口：`tests/share_vid_spike.rs`。2026-08-09 已从协议探针改为复用 production `emulator::launch`、`CaptureHandle` 与 managed stop。

- POSIX shm 名为 `videmulator<console-port>`，Linux 文件入口是 `/dev/shm/videmulator<port>`。
- 布局为 24 字节 header 加 BGRA 像素：

| offset | 类型 | 含义 |
|---:|---|---|
| 0 | `u32 LE` | width |
| 4 | `u32 LE` | height |
| 8 | `u32 LE` | fps（实测 60） |
| 12 | `u32 LE` | 单调递增 frame counter |
| 16 | `u64 LE` | 纳秒时间戳 |
| 24 | bytes | `width * height * 4` BGRA，alpha 实测 `0xff` |

- Pixel 2 的实测尺寸为 `24 + 1080 * 1920 * 4 = 8,294,424` 字节。
- idle 时像素区可保持稳定，counter 可以降到约 15fps，也观察过完全停止递增；因此 header 的 60 既不是交付帧率保证，延迟测量也必须注入肯定会改变画面的输入。
- 画面变化时可能改写大部分像素区，观察到单次约 6.2MB 变化。协议没有已知通知 fd；当前策略只能观察 counter。
- production reader 使用只读 mmap，复制前后双读完整 header；header 变化时丢弃并重试，尺寸/inode/长度变化时重新 attach。映射上限为 128 MiB 像素加 24B header，消费者只保留最新帧。
- Emulator 37.1.11 实测生产路径：Pixel 2 1080x1920 BGRA，21.3 秒完成 launch/boot；发布 61 帧、0 unstable、最近一次完整帧复制 688µs，停止后 engine、广告文件、端口与 shm 无残留。复制时间不是输入到显示延迟。

## GPU 与显示

验证入口：`tests/gpu_host_spike.rs`、`tests/headless_boot.rs`。

- 在该主机无 `DISPLAY` 时，`-gpu host` 的 GLX/EGL 初始化失败；设置 `EGL_PLATFORM=surfaceless` 不能改变 emulator 的 GLX 路径。
- `-gpu angle_indirect` 在该版本被判为无效并回退 auto。
- `-gpu swiftshader_indirect` 曾在 cold boot 中途 SIGSEGV（三次观察）。
- `-gpu swangle_indirect` 可在纯无头环境 boot，并会写广告文件；当前 GUI 因此使用它作为默认值，boot 约 30–50 秒。
- `Xvfb + DISPLAY + -gpu host` 成功选择 RX 7900 XTX/RADV，boot 约 22.7 秒。Xvfb 只满足 GLX display；Vulkan 渲染仍走 host GPU。
- 早期 `gpu_host_spike` 在 `-no-window` 与 `-qt-hide-window` 下都观察到 boot 完成但无广告；复核发现命令没有传显式 `-grpc`，而既有 gRPC 实验已经证明省略它本身就不会产生 liteavd 所需的广告。因此该结果不能归因于 host GPU，并已从架构阻断条件中撤销。
- 2026-08-10 CachyOS 桌面 Wayland/XWayland `DISPLAY=:1` 上，Emulator 37.1.11 的正式 `-grpc <port> -grpc-use-jwt -grpc-allowlist ... -no-window -gpu host -share-vid` 启动链通过：21.34 秒内完成 launch/boot、认证 screenshot 和 capture attach；已验证 SDK 内 qemu engine 打开 `/dev/dri/renderD128`，maps 中未发现 SwiftShader/llvmpipe/swrast，随后 exact stop 清理 engine、端口、广告、shm、认证材料和隔离 AVD。此测试没有启动 Xvfb。
- 产品的 `DesktopHost` 会在 spawn 前拒绝缺失/空白 `DISPLAY`，并在认证 gRPC 后要求已验证 engine 同时具备 `/dev/dri/*` fd 且没有已知软件 renderer；失败会停止实例，不回退到 swangle。默认 `HeadlessSwangle` 仍可在无 `DISPLAY`、无 Xvfb 下运行，Xvfb 不是 share-vid 或任一 production policy 的依赖。
- CachyOS 的 `xorg-server-xvfb` 21.1.24-1.1 安装体积 2.57 MiB、下载约 1.06 MiB。一次 `1280x1024x24` 空闲 Xvfb 实测 1 thread、RSS 55,192 KiB、PSS 24,096 KiB、CPU 约 0.1%；其依赖树会列出 Mesa/LLVM，但桌面 GTK/Mesa 环境通常已提供大部分。它本身体积较轻，项目只把它用于 synthetic GUI 门禁与历史实验，不作为产品运行时依赖。
- Fedora 44 的 GStreamer 1.28.5 VA 插件在该主机没有 H264 VAAPI decoder；可用备选是 `openh264dec` 或 `avdec_h264` 软件解码。项目当前尚未依赖 GStreamer crate。

## Flatpak

- 2026-08-10 在 CachyOS 使用 `flatpak-builder 1.4.10`、`org.gnome.Platform//50`、`org.gnome.Sdk//50` 和 `org.freedesktop.Sdk.Extension.rust-stable//25.08` 完成 release 构建、AppStream compose、export 与 user install。Cargo 源由官方 `flatpak-builder-tools` cargo generator commit `737c0085912f9f7dabf9341d4608e2a77a51a73a` 根据 `Cargo.lock` 生成；构建在 Cargo offline 模式下通过。
- 为支持显式 `DesktopHost`，历史安装 ref `fe255dfb842dc11042ecab04e3c472f23073f688b2cbd366c6a57124687d2cfb` 的静态权限为 `shared=ipc;network; sockets=wayland;x11; devices=dri;kvm;`，没有 home/host、host `/dev/shm`、通配 D-Bus、音频、USB 或 input-device grant；该包在沙箱内同时继承 `WAYLAND_DISPLAY=wayland-1` 与 `DISPLAY=:1`，并能访问 `/dev/dri/renderD128`、`/dev/kvm`，bundle SHA-256 复核通过，未安装或启动 Xvfb。
- WP-3.7 首次 Flatpak 实测发现 session auth 目录只存在于应用 mount namespace，宿主 PipeWire-Pulse daemon 对其中 FIFO 返回 `No such entity`。当前实现只在严格匹配应用 ID 的 Flatpak 中，把 mode 0600、PID/session-key 命名的 FIFO 放到宿主与沙箱共享且已复验为当前 UID/0700 的 `$XDG_RUNTIME_DIR/app/io.github.ydog12138.liteavd`；有效 metadata 保护 recovered session，dead-owner orphan 只按精确命名、FIFO 类型、UID、权限和 PID 条件清理。非 Flatpak 仍使用私有 auth 目录。
- 包含 WP-3.7 最终 UI/input 修复的 GNOME 50 release commit `4b6ad4c9720d90fd1efffbe768b922a112d21e4d11bef06160766d1db2f75e44` 已离线 build、AppStream compose、export、user reinstall 并复核权限；仍为 `shared=ipc;network; sockets=pulseaudio;wayland;x11; devices=dri;kvm;`，没有新 filesystem/D-Bus/device grant。实际 finish-args 沙箱内 endpoint recovery、固定 20ms CPAL host-input callback 和真实 KVM/WAV→guest 链均通过；后者取得 417,792B PCM，1kHz 幅值 23,958.8、RMS 16,941.4、旁带 0.1，总测试 30.38 秒并完成 exact cleanup。已安装 Wayland GUI 的 portal WAV、宿主说话→guest 录音→人工回放、独立隐私停止也已通过，全程没有 Xvfb。
- 同一 GNOME 50 finish-args 沙箱使用只读私有 SDK 和唯一临时 AVD，分别在 `HeadlessSwangle` 与 `DesktopHost` 跑通真实 `-no-audio`/JWT/guest timer/产品 `AudioController`/CPAL/exact stop 链；两次均未启动 Xvfb。swangle 为 9,608 received / 4,096 played / 0 dropped / 0 callback contention，总测试 24.34 秒；desktop-host 为 9,602 / 4,096 / 0 / 0，总测试 23.37 秒。测试通过 build sandbox 的精确 SDK grant 运行，并不等同于从已安装 GUI 手工点击后的可闻性记录。
- 默认托管 SDK/AVD 根为 Flatpak 私有 `$XDG_DATA_HOME/liteavd/android-sdk` 和 `$XDG_DATA_HOME/liteavd/avd`。使用只读精确 filesystem grant 时，沙箱内 Emulator 37.1.11.0 build 15917651 `-version`/`-accel-check` 与 adb 37.0.1 均通过，KVM version 12 可用。沙箱 localhost HTTP 精确 hash 校验通过，证明 network grant 同时覆盖下载和 loopback 控制面。
- 安装应用的 Wayland GUI 冒烟通过。Flatpak build sandbox 内以本地 HTTP/zip fixture 执行 UI→`PackageService`→下载校验→私有临时 SDK 安装通过，测试许可对话框的“拒绝”与“关闭”都返回中止；这两条是不含第三方条款的 hermetic 回归，不替代真实 Google 许可链。同一沙箱边界内使用只读隔离 SDK/system image 运行 `tests/gui_viewport_real.rs` 的生产链通过：真实 KVM engine、广告文件、JWT/JWK active、localhost adb/gRPC、私有 shm `share-vid`、1080x1920 `GdkMemoryTexture`、GTK drag/旋转后 touch 和 exact stop 均成功。10 秒采样为 222 published / 85 latest-frame overwrite / 0 attach retry / 0 unstable，RSS 154,677,248B、18 threads、43 fd 在采样窗内无增长；8 个短链路样本的 texture-commit p95 为 66.862ms，样本不足 500，不作 M1 `<50ms` KPI 结论。viewport root、JWT `Arc`、auth 目录、engine、端口、shm 和临时 AVD 都在测试进程退出前清理。
- 2026-08-10 从不存在私有 SDK/AVD 的已安装 Flatpak 开始，由用户在应用中显式接受真实 Google 许可，随后安装 Emulator 37.1.11、Platform Tools 37.0.1 与 Android 35/google_apis/x86_64，并创建 `liteavd_flatpak_wp42`。组件约占 4.4GB，安装后无 `.tmp-install-*`；SDK 与 AVD 均持久保存在 `~/.var/app/io.github.ydog12138.liteavd/data/liteavd/`。
- 同一空数据链启动 AVD 后确认 qemu 使用 console 5554、认证 gRPC 8554、`-grpc-use-jwt`、最小 allowlist、`-no-window -gpu swangle_indirect -share-vid`；adb 报告 `emulator-5554 device`，约 18.6 秒 boot complete。active JWK、0700 auth 目录、0600 recovery 文件和 8,294,424B mode 0600 的 1080x1920@60 share-vid 均确认，应用内交互触发认证控制请求。用户停止后 engine、5554/5555/8554、adb entry 与 auth session 都消失，AVD 保留。此链未安装或启动 Xvfb。
- 第一次空数据尝试发现 Flatpak 会继承宿主 `AVDM_SDK_ROOT`，但不可见的绝对路径会落到非持久沙箱视图。`sdk_override_allowed` 现在只允许私有 XDG data 子路径的新 SDK，或已获 filesystem grant、可见且包含 `emulator/emulator` 的外部 SDK；不可见宿主路径、私有根的 `..` 逃逸均有纯回归测试。
- 首次空数据实测使用修复前的安装包，产品 stop 后仍留下 `/dev/shm/videmulator5554`，但 app mmap 已关闭、engine 已死且端口已释放。确认归属后删除残留；exact stop/回滚/drop 现携带 console port，并在确认 engine 退出后清理精确 shm，core 回归验证升级到 SIGKILL 的路径也会删除。随后重建、校验并安装 0.1.0 bundle，复用私有 AVD 做产品启动/停止短链：engine 不存在、adb 为空、5554/5555/8554 释放、auth session 数为 0、shm 不存在，AVD 保留，实机关闭该缺陷。
- 上述产品复验还观察到 `begin_stop` 后、engine 退出到 session 完成之间的窗口中，viewport 鼠标事件会尝试重连已关闭的 gRPC 端口并产生密集警告。根因是 exact route 只比较 session identity，未排除 `Stopping` phase；当前 `InputRouteGuard` 在 stop 开始即失效，stop 失败后对保留 session 恢复有效，对应状态回归与输入队列回归通过。
- 上述 production swangle/share-vid 沙箱链没有安装或启动 Xvfb，再次证明 Xvfb 不是该路径依赖。宿主上已运行且不在同一沙箱的 Emulator 不能通过 liteavd 要求的 `/proc` 身份校验，因此当前不宣称该类 external session 可接管。
- 同一已安装 Flatpak 随后切换 `DesktopHost` 并重启应用，私有 `liteavd_flatpak_wp42` 使用 `-gpu host -no-window -share-vid` 启动；guest `boot=1`，产品 viewport/输入正常，engine 选择 AMD Radeon RX 7900 XTX/RADV，打开 5 个 `/dev/dri/renderD128` fd，maps 无 SwiftShader/llvmpipe/swrast。停止后 engine、5554/5555/8554、JWT/recovery 和 8,294,424B 私有 shm 全部消失，AVD 保留；Quick Boot snapshot 用 1.524 秒保存并移除锁。整个过程没有 Xvfb。
- 该产品链首次尝试因历史 `snapshot.lock.lock` 记录旧 Flatpak PID 而在 spawn 后被 Emulator 拒绝；现场确认无 engine、端口、认证或内核锁后曾手工隔离该 3B 文件。最终实现只在锁为当前用户的 0600 小型普通文件、记录 PID 已死且可同时取得 BSD/POSIX 非阻塞锁时自动清理；live/busy/symlink/异常内容均拒绝，managed engine 的 SIGTERM 宽限改为 20 秒。对应 hermetic 回归与最终 production host 复跑通过。
- 实测中曾因保存 GPU policy 后未重启而仍启动旧 swangle。最终 runtime 在 scheduler 完全空闲时会立即应用新 policy 并重建设备 demand；存在 queued/active/external allocation 时拒绝热切换，保持旧 reservation/backend，并把已保存设置留到下次应用启动。core 回归覆盖空闲成功与排队拒绝，Xvfb 设置页回归通过。
- `desktop-file-validate`、`appstreamcli validate --no-net` 和 manifest parse 通过。官方 Flathub manifest/builddir linter 的 URL/screenshot 报告属于已延后的 Flathub 渠道，不再阻塞当前 GitHub Releases 路径。
- GitHub Releases 的 `v*` tag workflow 会在 Ubuntu 24.04 构建单文件 `.flatpak`、验证安装和 `.sha256`，然后创建 draft prerelease；重复 tag run 只允许刷新仍为 draft 的 release。单文件 bundle 不内含 GNOME runtime 或完整 AppStream repository，构建时嵌入 Flathub runtime-repo hint。2026-08-10 已建立 public remote `https://github.com/ydog12138/liteavd`，annotated `v0.1.0` 指向 `32cc1fee724b362fa3757b0efcc58cf8ada589c1`。Ubuntu 24.04 workflow run `31403319009` 完成 build、AppStream compose、bundle、安装与 checksum 后创建 draft；公开前本机下载的 2,916,936B bundle SHA-256 为 `5cc4b76bab0bcbb4915d531b4abb49341953f44160ad292bf2ce8da724d684f3`，与 `.sha256` 和 GitHub asset digest 一致，user reinstall 得到 Flatpak commit `8878c00d83a4a5e87a451cef3e5029eb48ec30459e21de8fbe178367ef0a7cd1`，权限仍为 `shared=ipc;network; sockets=pulseaudio;wayland;x11; devices=dri;kvm;`。随后以 prerelease 公开于 `https://github.com/ydog12138/liteavd/releases/tag/v0.1.0`。

## GTK/线程与工具链

- Fedora 已验证 GTK 4.22.4、libadwaita 1.9.2；CachyOS 复验为 GTK 4.22.4、libadwaita 1.9.3、Rust/Cargo 1.97.1、protoc 35.1。仓库当前用 `mise.toml` 固定 Rust 组件并设置隔离 SDK 路径，不依赖 Nix flake。
- GTK widget 不是 `Send`。worker 侧使用 `glib::SendWeakRef`，`MainContext::invoke` 的闭包只捕获 `Send` 数据。
- 原先的 thread-local 跨线程任务队列不可行：worker 写的是 worker TLS，主线程 drain 的是另一份 TLS。当前代码已改为直接 `invoke`。
- 下载进度用原子数值加主线程 timer；timer 必须在任务结束时 `Break`。
- 模拟器自带 Qt 依赖 xcb；GUI 模式在 Fedora 需要 `libxcb-cursor.so.0`。liteavd 自身 GTK 窗口走 Wayland 不代表 emulator Qt 窗口也走 Wayland。
- 2026-08-09，Emulator 37.1.11 + Xvfb 下的 production capture → `GdkMemoryTexture(B8g8r8a8)` → `GtkPicture` 真实纵切运行 30 秒：1080x1920，capture 发布 269 帧、0 unstable，测试进程 RSS 增长约 24 MiB；结束后无 engine、端口、广告文件、shm 或临时 AVD 残留。该短测不替代 WP-1.5 的 30 分钟 soak。
- WP-1.4 输入/旋转复验的 1 秒快速 soak 发布 238 帧、丢弃 77 个旧 latest-frame、0 unstable、最近复制 763µs，测试进程 RSS 在采样区间无增长；该数字只证明链路和清理，不是延迟或长期稳定性结论。
- WP-1.5 用同进程单调时钟测量输入 RPC send→新 share-vid counter→`GtkPicture::set_paintable`；500 个 AppSwitch/GoBack 可观察交互样本为 p50 21.501ms、p95 38.441ms、p99 44.019ms，0 failure、0 pending、0 dropped pending。分段 p95：RPC 3.150ms、RPC completion→frame observation 31.854ms、frame copy 1.651ms、copy→GTK commit 6.041ms。该终点是 GTK texture property commit，不是显示器 scanout/photon。
- 2026-08-10 CachyOS 单设备正式 30 分钟 soak 通过：500 个样本 p50 13.582ms、p95 43.695ms、p99 50.404ms；queue/RPC/RPC→frame/copy/GTK 的 p95 分别为 2µs/3.075ms/35.957ms/1.445ms/6.212ms，最大 UI pump gap 21.711ms，0 failure/pending/drop。capture 发布 4050、覆盖旧帧 85、0 attach retry、0 unstable、无最后错误；RSS 从 474,488,832B、峰值 479,080,448B 到清理后 291,557,376B，thread 50、fd 35 均无增长。
- 2026-08-10 CachyOS 三设备正式 30 分钟 soak 通过：三路 GTK commit 为 7419/7363/7289，capture published 为 9667/9683/9626、latest-frame overwrite 为 2212/2297/2305，0 attach retry、0 unstable、无 capture error；最大 UI pump gap 为 126.208/126.191/126.189ms。测试进程 RSS 起点 666,927,104B、峰值 730,591,232B、结束前 559,144,960B，thread 62、fd 52 无增长。总测试时间 1834.31 秒；结束后 engine、端口、广告、shm、认证目录、临时 AVD 和 Xvfb 均无残留。该测试使用三个独立 viewport 验证底层并行边界，不代表产品多设备工作区已完成。
- 2026-08-10 WP-2.2 在独立 Xvfb `:98`（1600×1200×24）通过三 synthetic managed capture 的产品工作区门禁：三张 `GdkMemoryTexture` 同时进入各自 `GtkPicture`；同一 FlowBox 在 1180px 手工分配下为三列、420px 下为单列；焦点目标与 session route 一致。停止中间 session 只移除其 viewport，左右 `GtkPicture` 弱引用仍可升级，证明未重建另外两路。该测试不启动 Emulator，不能替代上一条三设备真实 soak。
- 2026-08-10 WP-2.3/2.4 在 CachyOS、Emulator 37.1.11、Pixel 2/android-35 google_apis x86_64 上通过独立 managed operation/restart 链：显式移除 `DISPLAY`/`WAYLAND_DISPLAY` 后以 swangle 启动，保存 focus/selection 并丢弃整个 `DeviceRuntime`，engine 未被终止；新 runtime 按广告完整身份与独占 recovery lease 恢复为可控 `Recovered` session，重新获得 capture/JWT 后认证截图返回有效 PNG；无效 `.apk` 得到逐设备失败且会话继续运行，随后 exact stop 成功。最终复验耗时 20.29 秒，结束后无 engine、恢复凭据、AVD/临时目录或 `videmulator*` shm 残留。跨设备部分失败/替换隔离另由 deterministic fake-adb 与 route 单测覆盖。
- 2026-08-10 WP-2.4 三设备 Xvfb 故障隔离短门禁运行 90 秒，总测试 118.43 秒；随机选择第 3 台并在中点由独立 worker 停止 engine。故障设备 capture 停在 312 帧，另外两台继续到 660/707 帧，GTK commit 为 570/578；三路最大 UI pump gap 为 19.181/19.283/19.288ms。测试进程 RSS 211,423,232B、峰值 236,511,232B、结束前 225,435,648B，thread 29→峰值30→25，fd 54→峰值58→51；三路 0 attach retry、0 unstable、无 capture error，结束后无 engine、auth/recovery、shm、临时 AVD 或 Xvfb 残留。首次实现把 stop 同步放在 GTK 循环，触发 2.219s pump gap 并被门禁正确拒绝；改为产品一致的 worker stop 后通过。该短门禁用于在正式长测前验证注入逻辑。
- 2026-08-10 WP-2.4 三设备带故障 Xvfb 正式门禁完成 1800 秒 soak，总测试 1828.18 秒；随机第 3 台在 900 秒由 worker 停止。三路 capture published 为 10289/10341/4860、latest-frame overwrite 为 1104/1169/1153，GTK commit 为 9157/9144/3701；两台 survivor 在故障后继续出帧，三路 0 attach retry、0 unstable、无 capture error。最大 UI pump gap 为 131.976/131.972/131.969ms。测试进程 RSS 起点 588,111,872B、峰值 695,668,736B、结束前 302,534,656B，thread 62→峰值63→58，fd 55→峰值59→52。结束后 engine、测试端口、auth/recovery、share-vid shm、临时 AVD home、测试 AVD 和 Xvfb 均无残留；该结果满足 WP-2.4 出口并关闭 M2。
- 2026-08-10 WP-3.0 本地 HTTP 门禁验证稳定 checksum/URL cache key、无父目录首次下载、206 与跨 service `.part` 续传、200 fallback、416、错误 `Content-Range`、无 `Content-Length` 的 16MiB 提前中止、损坏完整 cache 重下和 checksum 复验后零网络复用。Xvfb 下的真实按钮链从本地 HTTP 安装微型 platform-tools zip，完成后“本机已装/在线仓库”同时重建；许可拒绝与标题栏关闭均返回明确决定。测试 cache、假 SDK 和 Xvfb 均无残留。
- 2026-08-10 有 cache 的零 Java `install_chain` 在独立 `/data/Projects/liteavd-sdk` 用 38.77 秒复装并验证 Emulator 37.1.11.0、platform-tools 37.0.1 和 android-35/google_apis/x86_64；1,738,815,903B 系统镜像 zip 未重新下载，结束后无 `.tmp-install-*`/`.tmp-backup-*` 残留。该结果证明新组件 `flock` 不破坏现有事务安装，不代表对未授权许可的自动同意。
- 2026-08-10 WP-3.1 在 `/data/Projects/liteavd-sdk` 的 Emulator 37.1.11.0 上验证事务创建兼容性：测试使用唯一名称和临时 `ANDROID_AVD_HOME`，`emulator -list-avds` 能识别完成发布的 `.ini`/`.avd`，删除后不再列出；测试未启动虚拟机，临时 AVD home 无残留。独立 Xvfb 门禁同时验证创建向导暴露镜像管理入口，镜像列表重排后仍按 `api/tag/abi` 恢复原选择；Xvfb 与临时 SDK 已清理。
- 2026-08-10 WP-3.2 在 CachyOS、Emulator 37.1.11.0 的唯一隔离 managed AVD 上验证受 JWT 保护的 SnapshotService 写操作：exact route 顺序完成 save/list/load，load 后控制面重新可达，再 delete/list 确认移除；同一链继续通过 PNG screenshot、无效 APK 的可见失败隔离和 exact stop，总耗时 22.19 秒。结束后 engine、console/gRPC 端口、auth/recovery、share-vid、临时 AVD/output 与测试 session 日志均无残留。
- 2026-08-10 WP-3.2 的独立 Xvfb 门禁验证 operation bar 同时暴露 screenshot、APK、snapshot、session log 与 stop，APK 按钮持有 `GdkFileList` DropTarget；日志由 blocking worker 读取后回 GTK 主线程按 stdout 过滤，snapshot 结果行提供 load/delete。镜像页本地 HTTP 安装回归在改用共享长存 Tokio executor 后仍通过；各次 Xvfb 均已清理。
- 2026-08-10 WP-3.6 的版本化 fixture（普通 APK、`testOnly` base、French configuration split）均通过 Build Tools 35.0.1 的 APK v1/v2/v3 签名校验并固定 SHA-256。CachyOS、Emulator 37.1.11、Android 35/google_apis/x86_64 的唯一隔离 AVD 真实链在 29.39 秒内完成普通 APK 安装/重装/卸载、`testOnly` 安装/卸载、`install-multiple` base+split 安装/卸载、8MiB `PushFiles`、host/guest SHA-256 对照、`.part` 不存在、远端删除、无效 APK 失败隔离和 exact stop；结束后 adb 列表为空，engine、端口、auth/recovery、share-vid、临时 AVD/output 均无残留。开发时用于生成 fixture 的 Google build-tools/JDK 不属于产品构建或运行依赖。
- 同日 deterministic fake-adb 覆盖三设备稳定顺序的成功/失败/用户取消、64KiB 输出上限、超时/取消 kill+wait、route 被同端口新 PID 替换后的 stale 终止且不发送清理、远端已存在 no-clobber 和 staging 清理。Xvfb operation smoke 验证 APK 与普通文件两个 `GdkFileList` DropTarget。新增真实三设备门禁在唯一临时 AVD home 中让三台 guest 通过 all-running exact operation 安装普通 APK，并逐台接收 256MiB 不可压缩文件且 SHA-256 与宿主一致；第二台预置目标时第 1/3 台继续成功，第二台明确失败；下一次在第二台真实 adb push 运行中取消，捕获 PID 随后不存在，第二/三台为 canceled，三台无 `.part`。测试进程 RSS 10,682,368B 起、10,747,904B 峰值、10,944,512B 结束，thread 15→16→15、fd 22→27→22，总测试 74.35 秒并完成 exact cleanup。
- 最终安装 GNOME 50 Flatpak commit `d84ed323c39a361d49ef682dea79b276a9ad1ed78c8e44eaecde26967a65645f` 的真实 GTK chooser 选择 8,538B 签名 APK，确认后 guest package manager 返回 `io.github.ydog12138.liteavd.fixture.normal` versionCode 1；文件管理器把 1,147B `README.md` 拖到独立 push DropTarget，确认后得到 `/sdcard/Download/liteavd/README-op2-1.md`，guest/host SHA-256 同为 `b232bf0ab9427f4a8954c7a34c2d260b6419f67287cddd9b71a15a7181a9fecb`，没有 `.part`。两次 portal 都只导出精确源文件，静态权限仍为 `shared=ipc;network; sockets=pulseaudio;wayland;x11; devices=dri;kvm;` 且无 filesystem grant；验收后 package、远端文件和 document grant 均清理。产品 stop 后 engine、5554/5555/8554、adb、JWT/recovery、share-vid 与 Pulse 临时资源均无残留，私有 AVD 保留。
- 2026-08-10 WP-3.3 的 hermetic 门禁验证 settings v0→v1 迁移不会自动改写源文件，损坏/未来版本/超过 1MiB/符号链接均保留原文件并返回可见回退原因；rename 前故障保持旧文件且不留 temp。cache 配额测试验证最旧可回收项清理、活跃 `flock` lease 跳过以及未知 size/单归档超限在网络请求前失败。独立 Xvfb 同时确认 `AVDM_SDK_ROOT` 在设置页显示为只读覆盖，新增配额根 lease 后的本地 HTTP 镜像安装仍通过。
- 2026-08-10 WP-4.1 在 Rust 1.88.0 上通过 locked core-only check/test/strict Clippy（关闭时 149 passed / 1 ignored），并通过默认 GUI 全目标、全 feature check；`cargo tree --no-default-features` 不含 GTK/GDK/GLib/Pango/Cairo。加入 Flatpak、host GPU、部署、音频与虚拟麦克风回归后，当前 Rust 1.88 core-only 为 190 passed / 2 ignored，stable locked 全目标为 lib 216 passed / 12 ignored，全库 fmt 和无命令行豁免的 strict Clippy 通过。
- vendored 根 proto 与 Android Emulator 37.1.11.0 build 15917651 安装归档中的 `lib/*.proto` 逐字节一致；`proto/README.md` 记录来源/生成命令，`proto/SHA256SUMS` 校验当前所有输入。`google/protobuf` 是仓库既有的最小依赖快照，不来自该 Emulator 归档。
- tonic `Channel` 的 transport task 与创建它的 Tokio runtime 绑定；将 launch 的短命 runtime 中创建的 channel 直接交给另一个 worker runtime 会使输入 RPC 停滞到 deadline。当前 worker 在自己的长存 runtime 内按 endpoint/auth 重连；只在 transport 恢复时重放绝对 mouse/touch 状态，不重放结果未知的键盘事件。
