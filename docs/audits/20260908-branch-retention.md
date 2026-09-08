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
| `cursor/filter-vector-models-dd00` | 非等价提交在目录组装时过滤向量模型；当前 dev 已在 `chat_slugs_from_gateway` 导入阶段用同类规则过滤，并有 `fetch_drops_embedding_and_reranker_slugs` 测试。保留历史，不重复合入旧实现。 |
| `cursor/tui-account-launch-dd00` | 当前 dev 未包含该 ChatGPT 账号启动选择器；它增加模型/思考等级/额外参数选择，属于独立功能，未自动纳入本次既定整改。完整补丁已归档。 |
| 5 个 dependabot 分支 | 具体版本对照见下表；不把旧基线锁文件直接合入当前候选。 |

归档可恢复独有代码，不代表已经接纳其产品行为。删除前必须重新核对远端 SHA，避免删掉盘点后新推送的提交。

## 执行状态

尚未删除任何远端分支。完成本次本地验收后，集中提供 dev 推送、dev 发布触发和远端分支裁剪清单，等待明确授权。

## Dependabot 逐项核对

| 分支目标 | 当前 dev | 处置 |
|---|---|---|
| thiserror 2.0.20 | 已为 2.0.20，syn 3.0.4 也已存在 | 目标版本已包含，无需重合旧补丁 |
| webbrowser 1.2.4 | 已为 1.2.4 | 目标版本已包含，无需重合旧补丁 |
| flate2 1.1.10 | 1.1.9；分支还将 miniz_oxide 0.8.9 升至 0.9.1 | 归档可选升级，当前锁文件依赖审计无报告；本次未升级 |
| owo-colors 4.4.0 | 4.3.0 | 归档可选升级，本次未升级 |
| action-gh-release 3.0.3 | 固定 SHA 的 3.0.1 | 归档可选升级；本次发布流程修复基于已审查版本，不以旧分支覆盖新门禁 |

以上以当前源码、锁文件及分支补丁为依据，不代表对未采纳的新依赖版本作兼容性认证。归档于 2026-09-09 再次运行 `git bundle verify`，确认包含全部 22 个引用及完整历史。
