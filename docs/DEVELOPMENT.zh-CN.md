# 开发 liteavd

[English](DEVELOPMENT.md) | **简体中文**

## 仓库事实约定

- `docs/PRODUCT.md`：产品定位与 MVP 边界；
- `docs/ARCHITECTURE.md`：实现与信任边界；
- `docs/VALIDATED_FACTS.md`：与机器、版本、日期相关的证据。

英文架构文档为国际贡献者解释同一当前设计。没有可重复证据时，不得把目标态写成完成态。

## 工具链

- Rust 2024 edition；
- MSRV 为 Rust 1.88；
- `mise.toml` 固定当前开发工具链；
- 默认 `gui` feature 使用 GTK4/libadwaita；
- `protoc-bin-vendored` 提供构建期 protoc；
- Emulator proto 固定在 `proto/`。

项目保持单 crate。`--no-default-features` 的 core-only 正常依赖图不能泄漏 GTK/GDK/GLib/Pango/Cairo。

## 构建与质量门禁

```bash
cargo fmt --all -- --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo +1.88.0 test --locked --no-default-features --lib
cargo +1.88.0 clippy --locked --no-default-features --lib -- -D warnings
```

GUI 修改追加最小相关 Xvfb smoke。真实 SDK/Emulator 测试默认 `#[ignore]`，并在源码中写明环境变量、副作用和清理要求。

## 模块边界

- `src/core/`：repo、下载/安装、AVD/runtime、调度、adb、认证 gRPC、capture、operation、音频和麦克风，不引用 GTK 类型；
- `src/ui/`：GTK/libadwaita 组合和 core 状态投影；GTK 对象只在主线程访问，worker 只持纯 `Send` 数据或 `glib::SendWeakRef`；
- `proto/`：固定 Emulator 版本的协议与 checksum；
- `tests/`：默认自包含，真实系统门禁显式 ignored。

新增跨模块行为时优先建立显式状态模型和接口，不继续扩大 thread-local 全局容器，也不为每个 UI 操作新建 Tokio runtime；应用持有共享长存 executor。

## 真实 Android 测试

不得破坏性使用个人 SDK/AVD。必须采用：

- 专用 `AVDM_SDK_ROOT`；
- 唯一临时 `ANDROID_AVD_HOME`；
- 唯一 AVD 名称；
- signal 前复验进程身份；
- RAII 清理进程、端口、auth、共享内存、Pulse module/FIFO、文件和 AVD。

修改进程级环境变量的真实测试应串行运行：

```bash
AVDM_SDK_ROOT=/path/to/test-sdk \
  cargo test --test operation_real -- --ignored --nocapture --test-threads=1
```

按改动边界选择最小相关真实系统测试。生成 prost 代码只在 include 边界定向配置 lint，不能降低项目源码 Clippy 标准。

## Android fixture 规则

APK/WAV fixture 必须小型、版本化、确定性并有文档；binary 旁保留源码、manifest、生成说明和 SHA-256。生成 fixture 所需 Build Tools/JDK 只属于开发环境，不得成为运行时依赖。

不要把非确定性的 Clock ringtone 当作 exact 音频 route 证据。音频 fixture 使用 Android `AudioTrack`；麦克风通过确定性 recorder 和波形分析验证。

## Cargo 与 Flatpak sources

`Cargo.lock` 属于应用发布边界。修改后使用官方 `flatpak-builder-tools` Cargo generator 重新生成 `flatpak/cargo-sources.json`，并复验离线 manifest 构建。

`Cargo.toml`、最新 AppStream release、changelog 和 tag 版本必须一致。

```bash
flatpak/build-bundle.sh 0.1.0
```

脚本在 `dist/` 生成版本化 bundle 与 checksum；构建目录和产物均已忽略。

## Vendored Emulator proto

当前快照来自 Android Emulator 37.1.11.0 build 15917651。升级必须：

1. 只替换需要的 proto；
2. 在 `proto/README.md` 记录精确版本和生成命令；
3. 更新 `proto/SHA256SUMS`；
4. 重跑认证 gRPC 集成测试；
5. 审查 allowlist method 和生成 API 变化。

禁止引入裸 `-grpc` fallback。

## 文档与发布证据

“已实现”必须能指向当前代码或可重复测试；机器/版本/日期事实写入 `VALIDATED_FACTS.md`。解释保留限制所需的历史失败不能删除。

打 tag 前：

1. 运行完整 hermetic 门禁和相关硬件测试；
2. 校验 desktop/AppStream/Flatpak 元数据；
3. 审计 tracked 文件中的凭据和构建产物；
4. 确认版本一致；
5. 推送 release commit 并等待 GitHub CI；
6. 创建 annotated `v<version>` tag；
7. 检查 draft bundle/checksum 后再公开 prerelease。
