# 手机助手布局预览与验收证据

本目录包含独立网页预览、网页交互检查，以及实际安卓安装包的验收记录。网页使用示例数据；网页检查通过不能证明原生界面、模型调用、工具审批或工程保存通过。

| 文件 | 用途 |
| --- | --- |
| [网页预览](bk-cat-mobile-sheet-20261121.html) | 可直接在浏览器打开的单文件布局与交互示例。 |
| [网页检查脚本](check-mobile-sheet-preview.cjs) | 对上述静态网页执行 11 项交互与布局检查。 |
| [原生验收记录](mobile-sheet-verification-20261121.md) | 按安装包版本区分构建、设备操作、模拟模型及原生工具执行结果。 |
| [原图来源清单](native-mobile-sheet-20261121/manifest.json) | 五张真实安卓原图的来源、字节数、尺寸、进程号及校验散列。 |

## 固定工具与路径

准备 Node.js（脚本运行环境）18 或以上版本、npm（包管理工具）及 Git（版本管理工具）。以下命令从当前仓库内任意目录执行，不要求预先安装网页检查工具。

本轮使用 **Playwright（浏览器自动化工具）1.56.1**。其[固定版本发布信息](https://registry.npmjs.org/playwright/1.56.1)声明运行环境版本至少为 18，命令入口为 `cli.js`（工具命令入口）。下面将精确版本安装到 `.build/mobile-sheet-preview-tools`（忽略提交的临时工具目录），通过 `NODE_PATH`（模块搜索路径）让检查脚本加载它。不会修改产品的依赖清单或锁文件。

检查脚本的浏览器选择顺序为：

1. 已设置 `BKCAT_PREVIEW_BROWSER`（浏览器可执行文件路径）时，使用指定浏览器。
2. 否则，Linux（开源操作系统）上已有 `/usr/lib/chromium/chromium`（系统浏览器路径）时，使用该路径。
3. 否则，使用固定工具版本管理的 Chromium（开源浏览器）；必须先执行该版本的浏览器安装命令。

`BKCAT_PREVIEW_EVIDENCE`（网页检查证据目录）指定截图和报告位置。未设置时，脚本使用仓库下的 `.build/mobile-sheet-preview-20261121`（默认证据目录）。以下示例每次指定一个新目录，避免覆盖以前的证据。

## Windows PowerShell（视窗系统命令行）

以下完整示例使用固定工具版本管理的浏览器。`npm.cmd`（包管理工具的命令入口）用于避免依赖命令行脚本执行策略。

```powershell
$ErrorActionPreference = 'Stop'
$previewRepoRoot = & git rev-parse --show-toplevel
if ($LASTEXITCODE -ne 0) { throw '请从当前仓库内执行。' }
$previewRepoRoot = $previewRepoRoot.Trim()
$previewTools = Join-Path $previewRepoRoot '.build/mobile-sheet-preview-tools'
New-Item -ItemType Directory -Force -Path $previewTools | Out-Null

& npm.cmd --prefix $previewTools install --no-save --package-lock=false playwright@1.56.1
if ($LASTEXITCODE -ne 0) { throw '固定版本工具安装失败。' }
$env:NODE_PATH = Join-Path $previewTools 'node_modules'
$previewCli = Join-Path $previewTools 'node_modules/playwright/cli.js'

# 下载与固定工具版本匹配的浏览器。
& node $previewCli install chromium
if ($LASTEXITCODE -ne 0) { throw '浏览器安装失败。' }
Remove-Item Env:\BKCAT_PREVIEW_BROWSER -ErrorAction SilentlyContinue

$previewStamp = Get-Date -Format 'yyyyMMdd-HHmmss-fff'
$previewEvidence = Join-Path $previewRepoRoot ".build/mobile-sheet-preview-evidence/$previewStamp"
if (Test-Path -LiteralPath $previewEvidence) { throw '请使用尚不存在的新证据目录。' }
$env:BKCAT_PREVIEW_EVIDENCE = $previewEvidence
$previewCheck = Join-Path $previewRepoRoot 'docs/design/check-mobile-sheet-preview.cjs'
& node $previewCheck
$previewExit = $LASTEXITCODE
Write-Output "网页检查退出码：$previewExit"
if ($previewExit -ne 0) { throw '网页检查未通过，请检查终端错误。' }
Get-Content -Raw -LiteralPath (Join-Path $previewEvidence 'preview-check-results.json')
```

使用已有浏览器时，把上面“下载浏览器”和清空浏览器变量的部分替换为下列设置；将示例值改成实际存在的可执行文件路径：

```powershell
$env:BKCAT_PREVIEW_BROWSER = 'C:\Program Files\Google\Chrome\Application\chrome.exe'
if (-not (Test-Path -LiteralPath $env:BKCAT_PREVIEW_BROWSER -PathType Leaf)) {
    throw '指定的浏览器可执行文件不存在。'
}
```

## Linux shell（开源操作系统命令行）

以下示例在工具支持的系统环境中安装浏览器及其系统运行依赖。`--with-deps`（同时安装系统依赖）需要系统包管理器的安装权限；如果使用已配置好的浏览器，可按下方说明替换这一步。

```bash
set -eu
preview_repo_root=$(git rev-parse --show-toplevel)
preview_tools="$preview_repo_root/.build/mobile-sheet-preview-tools"
mkdir -p "$preview_tools"

npm --prefix "$preview_tools" install --no-save --package-lock=false playwright@1.56.1
export NODE_PATH="$preview_tools/node_modules"
preview_cli="$preview_tools/node_modules/playwright/cli.js"

# 下载匹配的浏览器，并补齐它的系统运行依赖。
node "$preview_cli" install --with-deps chromium
unset BKCAT_PREVIEW_BROWSER

preview_stamp="$(date -u +%Y%m%dT%H%M%SZ)-$$"
preview_evidence="$preview_repo_root/.build/mobile-sheet-preview-evidence/$preview_stamp"
test ! -e "$preview_evidence"
export BKCAT_PREVIEW_EVIDENCE="$preview_evidence"
if node "$preview_repo_root/docs/design/check-mobile-sheet-preview.cjs"; then
    preview_exit=0
else
    preview_exit=$?
fi
printf '网页检查退出码：%s\n' "$preview_exit"
test "$preview_exit" -eq 0
cat "$preview_evidence/preview-check-results.json"
```

使用已有浏览器时，把安装浏览器和清空浏览器变量的部分替换为：

```bash
export BKCAT_PREVIEW_BROWSER=/usr/bin/chromium
test -x "$BKCAT_PREVIEW_BROWSER"
```

该路径只是示例，应替换为实际浏览器路径。若系统已有默认优先选择的浏览器，但本次要明确使用刚下载的配套浏览器，在安装完成后设置：

```bash
export BKCAT_PREVIEW_BROWSER="$(node -e 'process.stdout.write(require("playwright").chromium.executablePath())')"
```

## 判定范围与原生证据

网页通过需同时确认检查进程退出码为 **0**，以及本次终端输出和 `preview-check-results.json`（网页检查结果文件）中 `passed`（通过项数）为 **11**、各项 `result`（结果）为 `passed`（通过）、`errors`（错误列表）为空。旧目录中的通过报告不能证明本次执行通过；示例已使用新的证据目录。截图供人工复核，不能单凭截图存在判定通过。

这 11 项仅检查独立静态网页。实际安卓结果以[原生验收记录](mobile-sheet-verification-20261121.md)和以下五张原图为准；原图保持原始字节，未重绘、裁切、缩放或合成：

- [原编辑器](native-mobile-sheet-20261121/manual-editor.png)
- [助手半屏](native-mobile-sheet-20261121/assistant-half.png)
- [助手全屏](native-mobile-sheet-20261121/assistant-full.png)
- [审批与键盘](native-mobile-sheet-20261121/approval-keyboard.png)
- [修改保存后重开工程](native-mobile-sheet-20261121/saved-project.png)

原图对应第五版安装包及生产源码 `8fe27a20326afac88a5fd5859ac5c5b3a3177b6d`；安装包散列、每张原图的远端路径与校验散列见[来源清单](native-mobile-sheet-20261121/manifest.json)。清单中的远端绝对路径用于追溯原证据，不是网页检查脚本的运行依赖。

## 工作流触发边界

本地网页复跑无需更改现有 CI（持续集成）工作流。当前[通用工作流](../../.github/workflows/ci.yml)与[手机工作流](../../.github/workflows/mobile.yml)的分支推送入口限定为 `main`（主分支）和 `dev`（开发分支）；[发布工作流](../../.github/workflows/release.yml)的标签推送入口为 `v*`（版本标签匹配模式）。它们另有合并请求、手动或复用调用入口，不能概括为只有分支和标签触发。

没有打开合并请求的普通 `fix`（修复）分支推送，不会匹配上述分支推送入口。本轮交付已核实当前修复分支没有打开的合并请求。之后创建或更新合并请求会按现有配置触发检查；本地安装工具和执行本文命令不会触发这些远端工作流。
