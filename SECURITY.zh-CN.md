# 安全策略

[English](SECURITY.md) | **简体中文**

## 支持版本

liteavd 仍是 pre-alpha。安全修复面向最新发布的 0.1.x prerelease 和当前 `master`，不支持更旧 snapshot。

## 报告漏洞

请使用 `ydog12138/liteavd` GitHub 仓库中的 **Report a vulnerability** 私有漏洞报告入口。在修复可用前，不要把漏洞公开到 issue、discussion、日志粘贴站或社交平台。

尽量提供：

- 受影响 commit/tag 与 Flatpak 版本；
- 宿主发行版、架构和沙箱状态；
- Android Emulator/系统镜像版本；
- managed/recovered/adopted session 来源；
- 复现步骤、预期与实际安全边界；
- 是否暴露凭据、宿主文件、guest 数据、端口或麦克风数据；
- 已移除 secret 的最小 PoC。

维护者会通过 GitHub 确认有效报告、评估严重性、协调修复与披露，并在报告者需要时署名。

## 应私下报告的边界

- managed gRPC 无需 session JWT 或可从 loopback 外访问；
- 输入/operation 跨 session 或跨复用端口投递；
- 停止或 signal 无关进程；
- Flatpak 超出声明权限或 exact portal grant 访问文件；
- archive traversal、组件覆盖或许可绕过；
- UI 已显示停止后仍持续采集宿主麦克风；
- JWT 私钥、guest PCM 或敏感 session 数据泄漏；
- 攻击者可控制的无界内存、磁盘或进程增长。

单纯的上游 Android Emulator 崩溃若没有越过 liteavd 安全边界，可以作为普通缺陷；若可能暴露宿主或 guest 数据，请仍先私下报告。

## 防御设计摘要

Managed session 使用独立 ES256/JWT 和最小 allowlist；operation 固化 exact route；进程终止复验 executable identity/port；下载复验 hash，archive 拒绝 traversal；私有文件使用严格权限和原子发布；Flatpak 不申请宽泛 filesystem grant；麦克风采集必须显式开启且不持久化。
