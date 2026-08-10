# liteavd 用户指南

[English](USER_GUIDE.md) | **简体中文**

## 1. 基本模型

liteavd 把 AVD 显示为单窗口工作区中的 session 卡片。卡片不只由 AVD 名称标识；操作会绑定精确 session ID、generation、console port 和经过验证的进程。设备停止并由新进程复用名称或端口后，旧操作不能跨入替换 session。

session 来源分为：

- **Managed**：由当前 liteavd 启动，可完整控制；
- **Recovered**：此前由 liteavd 启动，应用重启后用私有身份恢复；
- **Adopted**：从外部观察到；仍可显示，但需要 liteavd 私钥的能力明确不可用。

## 2. 镜像、组件与许可

在“镜像与组件”中查看本机和在线 package。托管模式直接从 Google repository archive 安装 Emulator、Platform Tools 和系统镜像，不调用 Java 工具。

安装受许可约束的组件前，liteavd 会展示许可全文。接受记录同时绑定 license ID 和规范化文本 hash；许可变化后会重新展示。拒绝、关闭窗口或持久化失败都会中止操作。

下载使用稳定 cache、断点续传、checksum 复验、配额核算和事务安装。不要手工修改活动中的 `.part` 或临时安装目录。

## 3. 创建与管理 AVD

创建向导只接受结构完整、经过复验的本地系统镜像。选择设备 profile、内存/数据分区、名称和 GPU policy 后，`.ini` 与 `.avd` 会事务发布，同名对象不会被覆盖。

对应 Emulator 仍运行时不能删除 AVD。

## 4. 启动、焦点与选择

启动请求进入 FIFO scheduler，可能等待：

- `5554..=5586` 范围内的空闲偶数 console port；
- 并发启动上限；
- 内存预算；
- DesktopHost 所需的 host-GPU slot。

UI 会解释排队原因，并允许在 spawn 前取消。

点击设备卡片切换焦点；`Ctrl+1`、`Ctrl+2`、`Ctrl+3` 可聚焦可见卡片，额外 modifier 不会被接受。选择框独立于焦点，用于批量操作。

操作作用域明确分为：焦点设备、已选设备、全部运行设备。

## 5. 视口与输入

Managed/Recovered session 把最新的完整 `-share-vid` BGRA 帧显示在 GTK Picture 中；未读取的旧帧会被覆盖，而不是积压。

视口转发主键触摸/拖动/释放、鼠标 hover/右键、导航键和输入法 UTF-8 commit。坐标根据真实共享视频 buffer 和 letterbox 映射；图像外 press 被忽略，活动 drag 会 clamp，并在失焦或 detach 时可靠 release。

## 6. 卡片快捷控制

每张 managed/recovered 卡片提供响应式的 Android 导航、电源、guest 音量、截图、宿主麦克风与 WAV 等快捷入口。触发时固化 exact route，延迟完成的动作不会误发给替换 session。

## 7. 截图、APK 与普通文件

操作工具栏作用于选定 scope，并按稳定顺序报告每个目标。

### 截图

截图使用认证 gRPC，复验 PNG，写入同目录临时文件，并以 no-clobber 方式发布。

### APK 安装

- 单 APK 使用 `adb install -r -t`；
- 用户明确选择的全 APK 集合使用 `adb install-multiple -r -t`；
- `-d`（允许降级）和 `-g`（授予运行时权限）只在确认页显式开启；
- 混合类型、`.aab`、`.apks`、XAPK、符号链接和非普通文件会被拒绝。

可以通过 GTK portal 选择文件，或拖到 APK 按钮。确认前检查 exact session 和 flags。

### 普通文件推送

选择普通文件或拖到文件推送按钮，目标固定为：

```text
/sdcard/Download/liteavd/
```

远端名称包含清理后的 basename、operation ID 和 item index。文件先写唯一 `.part`，再以 no-clobber 方式发布。失败或取消时，只要原 route 仍有效，就会尽力清理 staging。

## 8. guest 扬声器输出

只有焦点 managed/recovered session 通过 liteavd 播放。链路使用认证实时 `streamAudio`、120ms 有界 buffer 和 10ms 宿主 callback。焦点变化会清除旧 route，并用短淡入淡出避免爆音或重叠。

顶部控件分别控制播放/静音和应用音量；Android guest 的媒体音量仍是独立控制。

没有私钥的 adopted session 会明确显示不可用。由旧 allowlist 启动后恢复的 session 可能需要重启设备。

## 9. 虚拟麦克风

麦克风默认关闭、不持久化，并且同一时刻只允许一个 exact focused managed/recovered session 使用。来源互斥：

- 默认 PulseAudio 兼容输入设备的实时宿主麦克风；
- 用户选择或拖放的 PCM WAV 文件。

WAV 支持 PCM U8/S16、mono/stereo、最高 48kHz，并流式转换为 48kHz mono S16。UI 提供暂停/继续和独立停止。MP3、AAC、FLAC 与压缩 WAV 不支持。

停止、切换焦点、替换 route、控制面 reset 或退出应用都会取消旧来源；PCM 不持久化。

## 10. Snapshot 与日志

Snapshot 对话框只作用于 focused exact session，支持 list/save/load/delete。加载后会重置长存控制流，UI 在 Emulator 控制面恢复后重连。

Managed session 日志有界轮转。查看器在 GTK 线程外加载，可过滤 stdout/stderr，并以 no-clobber 方式导出。Adopted session 不假定拥有可恢复的 managed log pipe。

## 11. 设置与 GPU policy

设置写入 versioned、0600、原子文件，可配置并发启动、内存预算、host-GPU slot、cache 配额、日志级别和 managed GPU policy。

- **HeadlessSwangle**：默认，不需要 display，不占 host-GPU slot；
- **DesktopHost**：需要桌面 XWayland `DISPLAY`，占一个 host-GPU slot，并复验 Emulator 已打开 `/dev/dri` 且未使用已知软件 renderer；绝不静默回退。

只有 scheduler 没有 queued/active/external allocation 时，GPU policy 才会立即作用于后续启动；否则在重启 liteavd 后生效。

## 12. 停止与恢复

停止首先使输入和长存流失效，复验 exact 进程，再请求 Emulator 退出并等待；只有同一已验证进程超时才升级终止。停止完成前 port 和资源 reservation 仍归该 session 所有。

关闭 liteavd 不会自动杀死健康的 managed Emulator。下次启动会结合私有 recovery lease、广告文件、进程身份、JWT、capture、焦点和选择进行恢复。真正外部实例保持 adopted，不会被静默授予控制。

## 13. 已知限制

- Android Emulator 37.1.11 在 HeadlessSwangle + 音频流下反复触发 Google Clock timer 可能 SIGSEGV；确定性连续音频通过最终 30 分钟门禁，该刺激仍作为上游特定限制保留。
- 首版不解析 AAB/APKS/XAPK，也不调用 bundletool。
- 虚拟麦克风只支持 PCM WAV。
- 多 display、远程 Emulator/WebRTC、ARM host、Windows 和 macOS 不在范围内。
- DesktopHost 不是真无头策略；缺少 XWayland 或硬件 renderer 证据时会失败。

安装问题见[安装指南](INSTALLATION.zh-CN.md)。报告缺陷时请提供精确 AVD 名称、session 状态、GPU policy、有界日志和复现步骤，不要公开凭据或 guest 私有数据。
