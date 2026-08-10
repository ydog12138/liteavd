# 为 liteavd 贡献

[English](CONTRIBUTING.md) | **简体中文**

欢迎提交范围清晰的缺陷报告、文档修正、可重复性能证据，以及保持产品/安全边界的代码修改。

## 提交 issue 前

- 提交新报告前先搜索已有 issue；
- 安全漏洞不要公开提交，按[安全策略](SECURITY.zh-CN.md)报告；
- Emulator 特定故障请记录 Emulator build、系统镜像、GPU policy、宿主 compositor/GPU，以及是否使用 Xvfb；
- 删除 JWT、SDK 许可记录、guest 私有数据和不应公开的宿主路径。

## 开发环境

依赖安装见[安装指南](docs/INSTALLATION.zh-CN.md)，模块边界与测试策略见[开发指南](docs/DEVELOPMENT.zh-CN.md)。

从 `master` 创建 topic branch。每次修改只解决一个问题，不把无关清理混入行为修复。

## 必须遵守的工程规则

- 行为缺陷先用测试或最小复现证明，再修实现；
- core 模块不得引用 GTK 类型；
- GTK 对象只在主线程访问；
- 下载、hash、解压不能把完整大文件读入内存，也不能阻塞 GTK 主线程；
- 不得降低 managed gRPC 认证或回退裸 `-grpc`；
- 不得只凭 PID 存活停止 Emulator，必须复验端口和进程身份；
- `-share-vid` 保持 latest-frame 语义；
- 真实/破坏性测试使用隔离 SDK/AVD 根；
- 许可接受必须显式，并绑定当前文本 hash。

## 本地门禁

至少运行：

```bash
cargo fmt --all -- --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets --all-features -- -D warnings
```

core 修改还要通过 Rust 1.88 core-only test/Clippy；UI 修改运行相关 Xvfb smoke；进程、adb、gRPC、capture、调度修改应在独立测试 SDK 上运行对应 ignored integration gate。

## 文档

行为、安装、权限或控件变化时同步更新英文和简体中文用户文档。机器/版本/日期证据写入 `docs/VALIDATED_FACTS.md`，不得把计划目标写成已完成事实。

## Pull request 检查表

- [ ] 修改只有一个明确目标，没有无关编辑；
- [ ] 可观察行为有回归覆盖；
- [ ] fmt、locked tests 和 strict Clippy 通过；
- [ ] 列出所需 Xvfb/ignored integration test 结果；
- [ ] 已考虑失败、取消和清理路径；
- [ ] 安全、权限和许可边界没有静默扩大；
- [ ] 中英文文档一致；
- [ ] 没有凭据、用户 SDK 状态、构建产物或 AI assistant 文件。

提交贡献即表示同意按项目 MIT License 提供该贡献。
