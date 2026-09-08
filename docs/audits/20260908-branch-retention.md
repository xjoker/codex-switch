# 分支收敛记录

目标保留 `master` 与 `dev`。`master` 不改名，不合并，不触发正式发布。

远端引用已通过 Git HTTPS OpenSSL 后端核实；本地 `dev` 从 `origin/dev` 的 `be6a0add0652a1e8ea34f93852d74908c9e81497` 开始开发。

## 可恢复归档

22 个远端引用及其完整可达历史已保存到本机 `target/branch-retention/pre-cleanup.bundle`，大小 14,208,908 字节；`git bundle verify` 成功。对应完整 SHA 清单在同目录 `remote-heads.json`。这两个文件位于忽略目录，尚未作为远端资产上传。

## 需要保留的差异信息

| 分支组 | 结论 |
|---|---|
| 12 个 cursor 分支 | 分支头已是原 dev 的祖先 |
| `cursor/tui-launch-feature-1ab3` | 非祖先提交存在等价补丁，`git cherry` 为 `-` |
| `cursor/filter-vector-models-dd00` | 1 个非等价提交，增加向量模型过滤；未自动合入 |
| `cursor/tui-account-launch-dd00` | 1 个非等价提交，增加账号启动选项界面；未自动合入 |
| 5 个 dependabot 分支 | 均基于旧主线；不能直接按分支存在判断仍需升级 |

归档可恢复独有代码，不代表已经接纳其产品行为。删除前必须重新核对远端 SHA，避免删掉盘点后新推送的提交。

## 执行状态

尚未删除任何远端分支。完成本次本地验收后，集中提供 dev 推送、dev 发布触发和远端分支裁剪清单，等待明确授权。
