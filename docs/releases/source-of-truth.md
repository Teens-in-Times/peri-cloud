# 发布真源

## 规则

1. 唯一源码真源是 GitHub `Teens-in-Times/peri-cloud`。
2. 所有提交必须先推送到 `origin`；发布只从已推送的 commit 或 tag 构建。
3. 部署机不保留 Git 仓库、源码分支或本地补丁；只解包 release 资产和保留运行状态。
4. 禁止在服务器上 `git pull` 后直接构建，禁止用服务器本地修改生成发布物。
5. release 必须包含 `BUILD.json`（commit）、`SHA256SUMS` 和源码 zip；部署前核对三者一致。
6. 任何热修复先提交并推送到 GitHub，再重新打包；不允许只替换二进制而不留源码记录。

## 发布流程

1. 在分支完成改动并通过测试。
2. 推送分支 → PR/CI → 合入 `main`。
3. 打 `vX.Y.Z` tag 并推送。
4. 从该 tag 的 CI 产物生成 release，上传 GitHub Releases。
5. 部署机下载 release，校验 SHA256 与 `BUILD.json` 的 commit，再切换 `current` 指针。
6. 回滚使用上一个 release 指针和迁移前备份；不使用服务器本地构建。
