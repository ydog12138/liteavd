# liteavd 文档

[English](README.md)

本目录把用户指南、贡献指南、设计事实与带日期的验证证据分开维护。项目概览中的产品声明必须与架构和验证记录一致。

## 从这里开始

| 文档 | 用途 |
|---|---|
| [安装说明](INSTALLATION.zh-CN.md) | 安装 GitHub Release bundle、源码构建、更新、卸载与故障排查 |
| [用户指南](USER_GUIDE.zh-CN.md) | 准备 SDK、创建与操作设备、配置音频和 GPU 策略 |
| [架构](ARCHITECTURE.md) | 组件、状态所有权、数据路径、安全边界与限制 |
| [开发指南](DEVELOPMENT.zh-CN.md) | 工具链、构建命令、测试、仓库结构和按改动追加的门禁 |
| [贡献指南](../CONTRIBUTING.zh-CN.md) | 贡献流程与评审要求 |
| [安全策略](../SECURITY.zh-CN.md) | 支持版本、威胁边界与私密报告方式 |

## 设计与验证记录

- [产品定义](PRODUCT.md)
- [架构事实源](ARCHITECTURE.md)
- [验证事实](VALIDATED_FACTS.md)

机器、版本和日期相关的结果应写入 `VALIDATED_FACTS.md`。不得把目标、设计意图或 ignored 集成测试写成无条件的已完成功能。
