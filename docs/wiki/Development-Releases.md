# Testing development releases

> **Prerelease warning:** the rolling `dev` build contains changes intended for the next stable release. It may change again before release. Do not use it when you need stable production behavior.
>
> For stable-channel updates, Homebrew rules, and legacy-install migration, see [Updating](Updating).

## Install the rolling dev build

For an existing direct installation:

```bash
codex-switch self-update --dev
```

For a new macOS or Linux installation:

```bash
curl -fsSL https://github.com/xjoker/codex-switch/releases/download/dev/install.sh | bash -s -- --dev
```

Do not substitute a `raw.githubusercontent.com/.../master/scripts/install.sh` URL. That old branch can serve a retired installer which writes to `/usr/local/bin`. The dev Release asset above defaults to `$HOME/.local/bin`; it requests `sudo` only for the one-time removal of a root-owned legacy binary, or when you explicitly pass `--system`.

For a new Windows installation, run this in PowerShell:

```powershell
$env:CS_DEV="1"
irm https://github.com/xjoker/codex-switch/releases/download/dev/install.ps1 | iex
```

### Move from Homebrew to dev

Homebrew distributes stable releases only and owns its Cellar binary. Do not use `self-update --dev` against a Homebrew installation. Remove the package, then use the direct dev installer:

```bash
brew uninstall codex-switch
curl -fsSL https://github.com/xjoker/codex-switch/releases/download/dev/install.sh | bash -s -- --dev
```

Profiles and configuration remain under `~/.codex-switch`; changing binary ownership does not reset them.

## Verify the installation

Confirm that the reported version ends in `-dev`, then check the rolling channel without modifying the installation:

```bash
codex-switch --version
codex-switch self-update --check --dev
```

The installer and self-updater preserve profiles and configuration. On macOS and Linux, current direct installs default to the user-owned `$HOME/.local/bin`; Windows direct installs use `%LOCALAPPDATA%\Programs\codex-switch`.

## Run a focused smoke test

Use the smallest path that covers the behavior you want to test:

```bash
codex-switch list
codex-switch tui
```

If you are testing a specific command, run `codex-switch <command> --help` before exercising it. Do not use live reset cards, profile deletion, daemon installation, or account switching unless those actions are part of the intended test.

## Report a problem

Open an issue with:

- operating system, architecture, terminal, and installation method
- output from `codex-switch --version`
- the exact command and the expected and actual behavior
- the smallest reproducible sequence
- redacted diagnostic output when needed

```bash
codex-switch --debug <command>
```

Remove tokens, profile contents, email addresses, account IDs, workspace names, identifying filesystem paths, and proxy credentials before sharing output. Use the [GitHub issue tracker](https://github.com/xjoker/codex-switch/issues).

## Return to stable

For a direct installation:

```bash
codex-switch self-update --stable
```

To return to Homebrew ownership, remove the direct installation and reinstall `xjoker/tap/codex-switch` with Homebrew.

```bash
curl -fsSL https://github.com/xjoker/codex-switch/releases/download/dev/install.sh | bash -s -- --uninstall
brew install xjoker/tap/codex-switch
```

When the uninstaller asks whether to remove the data directory, answer `N` to preserve profiles and configuration.

## 中文摘要

`dev` 是下一正式版发布前的滚动测试通道，可能继续变化。直装用户可运行 `codex-switch self-update --dev`；macOS/Linux 新装使用安装脚本的 `--dev` 参数，Windows PowerShell 设置 `$env:CS_DEV="1"` 后运行安装脚本。安装后用 `codex-switch --version` 确认版本以 `-dev` 结尾（例如 `20260828.1.0-dev`），并用 `codex-switch self-update --check --dev` 检查通道。测试结束后运行 `codex-switch self-update --stable` 回到直装正式版。

Homebrew 只提供正式版。切换开发版前先运行 `brew uninstall codex-switch`，再使用带 `--dev` 的直装脚本。若测试后希望恢复 Homebrew 管理，运行直装卸载脚本、在删除数据目录的询问中选择 `N`，然后执行 `brew install xjoker/tap/codex-switch`。

测试自定义提供方或多模型 TUI 时，可先阅读[中文指南](Chinese-Guide)中的操作摘要，再对照英文 [Providers](Providers) 核对边界行为。

提交问题前请删除 Token、profile 内容、提供方密钥、邮箱、account ID、工作区名称、可识别身份的路径和代理凭据。更多中文入口见[中文指南](Chinese-Guide)。

## Next steps

- Learn the supported workflows in the [Feature guide](Feature-Guide).
- Review stable-channel and migration details in [Updating](Updating).
- Diagnose a failed install or update with [Troubleshooting](Troubleshooting).
