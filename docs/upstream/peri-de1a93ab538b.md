# Peri de1a93ab538b 人工同步

上游：`dcd4d116a12dacdc8db41ec5d1387c6bbe874fdd` → `de1a93ab538b2a695c548b5460ced7371c8e7cc4`。

[上游差异](https://github.com/KonghaYao/peri/compare/dcd4d116a12dacdc8db41ec5d1387c6bbe874fdd...de1a93ab538b2a695c548b5460ced7371c8e7cc4)；源码范围变化 119 项。

## 适配

- 保留云端手工压缩入口、无工具摘要请求和提示词；合入上游 provider replay 边界保护。
- 保留原生发行版的 peri-resources 依赖补丁，不引入 Emscripten 专用 wasm-fetch 依赖。
- 合入压缩净收益、证据投影、子会话交接与恢复、模型协议及相关上游回归。
- 上游新 SDK LICENSE 与旧根 LICENSE blob 相同，原许可证与第三方归属保留。
- 云端账户、OAuth 人工授权、设备权限与现有模型接入策略由本发行版继续管理；SDK 新模型能力不会自动注册到 Cloud。

## 验证

本地 Windows、模拟模型及本地 HTTP：

- 配置、公共协议、模型与凭据库 1121 项，压缩 244 项、子会话 209 项、模型桥接 28 项、Responses wire 12 项、Agent terminal 9 项，共 1623 项通过；一项原有忽略保持忽略。
- 全量 Cloud 目标编译通过；Cargo.lock 仅调整已有包的依赖边，第三方包名称、版本与来源集合不变。
- 改动 crate 的严格 Clippy：八个 crate 检查全部目标；peri-middlewares 检查运行时 lib。它原有的 Unix 专用测试辅助代码在 Windows 全目标严格检查下仍有 dead_code/unused_imports 告警，本次不扩大清理范围；现有 CI 仍运行完整核心回归。
- 保留 Unix 测试辅助 OAuth 构造器的平台条件，记录为一个新的生产补丁；所有 Rust 源码/测试均少于 1000 行。
- 来源校验、格式和精确提交 CI 另以关联 PR 记录为准。未调用真实模型或部署生产。

SDK 新增的 ChatGPT 计划登录不是 Cloud 新入口；其凭据持久存储目前仅支持 Unix。现有 Cloud OAuth 人工授权及 Windows 执行器登录流程不变。
