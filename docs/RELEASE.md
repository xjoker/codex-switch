# Release 流程

CI 由 `.github/workflows/release.yml` 驱动，监听 tag 事件 `v*` 与 `dev`。

## 版本号策略

| 推送的 tag | CI 输出版本号 | GitHub Release 名 | self-update 通道 | 触发 homebrew |
|---|---|---|---|---|
| `dev`（rolling，每次覆盖） | `<Cargo.toml base>-dev.<UTC时间戳>` | `dev` | `--dev` | 否 |
| `vX.Y.Z-<suffix>`（永久 prerelease） | `X.Y.Z-<suffix>` | `vX.Y.Z-<suffix>` | 拿不到（客户端硬编码 tag=`dev`） | 否 |
| `vX.Y.Z`（stable） | `X.Y.Z` | `vX.Y.Z` | 默认通道 | 是 |

> Cargo.toml `version` 字段不带 `-dev` 后缀，由 CI 在 inject 步骤统一加。
>
> 客户端 `src/update.rs` 的 `--dev` 通道 `fetch_release(Some("dev"))` 是写死的 tag=`dev`，所以"独立 prerelease tag"对 self-update 不可见。

## ⚠ `dev` 是分支也是 tag（refname 歧义）

本仓库 `dev` 同时是开发分支与 rolling tag。**所有 push/delete/lookup 必须使用完整 refspec**，否则报：

```
error: src refspec dev matches more than one
```

或意外推到错误的目标。

## 发布 dev 测试版（标准流程）

前置：`dev` 分支已合入待发布的所有 commit；本地工作树干净。

```bash
# 1) 跑全量测试（最低保证）
cargo test --all

# 2) 本地 release 构建一次，验证可编译且 --version 正确
cargo build --release
./target/release/codex-switch --version

# 3) 推送 dev 分支到远端（必须完整 refspec）
git push origin refs/heads/dev:refs/heads/dev

# 4) 删除远端旧 dev tag（指向旧 commit，需要先删才能"移动"）
git push origin :refs/tags/dev

# 5) 在本地把 dev tag 重打到 HEAD
git tag -d dev && git tag dev

# 6) 推送新 dev tag —— 触发 CI 构建 6 平台二进制 + 覆盖 GitHub Release `dev`
git push origin refs/tags/dev:refs/tags/dev
```

> 第 6 步**不能写 `git push origin dev`**：歧义错误（分支 + tag 同名）。必须用 `refs/tags/dev:refs/tags/dev`。
>
> 第 3 步同理：必须 `refs/heads/dev:refs/heads/dev`。

CI 完成后产物：
- 6 平台 tarball：`cs-{linux,darwin,windows}-{amd64,arm64}.tar.gz` + `.sha256`
- `install.sh` / `install.ps1`
- 用户侧：`codex-switch self-update --dev` 立即可拉取

## 发布 stable

```bash
# 1) 在 dev 分支充分验证后，合并到 master
git checkout master && git merge --ff-only dev && git push origin master

# 2) 在 master 上打版本 tag（注意：不要带 -dev / -rc 后缀，否则成 prerelease）
git tag vX.Y.Z && git push origin refs/tags/vX.Y.Z:refs/tags/vX.Y.Z

# 3) CI 会自动：构建 6 平台 + 创建 GitHub Release vX.Y.Z + 触发 homebrew job
```

发布前别忘了：
- `Cargo.toml` 版本号 bump 到 `X.Y.Z`
- `docs/CHANGELOG.md` 顶部新增 `## vX.Y.Z — YYYY-MM-DD` 段

## 排错

**`error: src refspec dev matches more than one`**
完整 refspec：`refs/heads/dev:refs/heads/dev`（分支） / `refs/tags/dev:refs/tags/dev`（tag）。

**dev tag 推上去但 CI 没跑**
检查 GitHub Actions 页面有没有 `Release` workflow 触发；workflow 文件 `on.push.tags` 是否仍包含 `"dev"`。

**`self-update --dev` 取不到新版**
GitHub Release 名必须是字面 `dev`（小写）。如果误推成 `v0.0.15-dev` 这类带 `v` 前缀的独立 tag，会创建独立 prerelease，客户端通道看不到。

**Cargo.toml 版本号该带 `-dev` 吗？**
不带。CI 的 dev 路径会自动追加 `-dev.<timestamp>`，本地 `Cargo.toml` 始终保持 `X.Y.Z` 干净版本号。
