# 独立安卓原生接口回归探针

本目录是独立构建工作区，包名为 `app.concat.jniregression`（独立测试应用），不引用编辑器、助手、模型、网络或用户文件。它使用真实安卓虚拟机调用验证 9 类标量真假值接口，并覆盖生成绑定及返回寄存器边界，共 41 项记录。没有应用界面；系统原生活动完成后退出。

## 依赖与补丁

- `android-activity = 0.6.1`（安卓活动库）。
- `jni = 0.22.4`（原生接口库）。
- `jni-sys = 0.4.1`（原生接口底层类型库）。
- `android-build = 0.1.4`（安卓字节码构建助手）。
- 路径补丁固定为 `../../../src/patches/jni`（当前仓库中的接口边界补丁）和 `../../../src/patches/jni-macros`（生成绑定补丁），两者版本均为 0.22.4。
- 开发配置关闭增量；所有依赖的优化级别为 2，与主仓库一致。调用方设置 `CARGO_PROFILE_DEV_DEBUG=0`（关闭调试信息）。
- 开发配置明确使用 `panic=unwind`（恐慌可展开），以捕获测试中故意写错的类型兼容声明。若实际配置改成立即终止，探针会在编译阶段明确拒绝，不能把未执行的预期断言算作通过。

本探针在仓库中的位置为：

```text
scripts/ci/android-jni-probe
```

清单到仓库根目录共有三级目录；原生源码中的共享辅助模块引用多一级源码目录。两个补丁路径及辅助模块路径都依赖这一结构。不要把探针单独搬离仓库后直接使用原清单。

## 覆盖范围

| 原生接口 | 真实验证 |
| --- | --- |
| `CallBooleanMethodA`（实例真假方法） | 假、真各一次；另验证子类确实覆盖了父类方法。 |
| `CallNonvirtualBooleanMethodA`（非虚真假方法） | 在子类实例上直接调用父类实现，结果与虚调用相反。 |
| `CallStaticBooleanMethodA`（静态真假方法） | 假、真各一次。 |
| `GetBooleanField`（实例真假字段） | 写入假和真后分别读取。 |
| `GetStaticBooleanField`（静态真假字段） | 写入假和真后分别读取。 |
| `IsSameObject`（对象相同性） | 同一对象的另一局部引用、不同对象。 |
| `IsInstanceOf`（对象类型判断） | 子类实例与父类、测试实例与字符串类。 |
| `IsAssignableFrom`（类型可赋值判断） | 子类转父类为真、父类转子类为假；参数顺序遵循原生接口。 |
| `ExceptionCheck`（异常检查） | 初始无异常为假；真实抛出异常后为真且异常对象非空；清除后为假且异常对象为空。 |

测试对象主动抛出 `IllegalStateException`（非法状态异常）。探针清除待处理状态后，继续核对保留下来的真实异常对象类型与专用消息。没有替换异常检查函数，也没有伪造异常对象。

以上动态环境调用产生 26 项记录。另有两组必要检查：

- `bind_java_type!`（生成类型绑定）产生的实例方法、静态方法、实例字段、静态字段分别读回假和真，共 8 项。类缓存明确使用真实内存类加载器。合法的子类到父类兼容声明实际初始化；故意写错的反向声明必须触发生成器专用断言并被捕获，再确认没有遗留虚拟机异常，共 2 项。捕获范围只接受预期的类型断言消息，其他错误均失败。
- 安卓 64 位汇编函数复现实际观察到的返回寄存器 `0x00007b8978663500`（高位残留且低字节为零）。两个入口共享同一函数体：整寄存器入口先核对四个输入的完整寄存器值；字节入口随后直接调用 `src/patches/jni/src/ffi_bool.rs`（生产补丁共享辅助源码）的 `call1`（单参数调用适配）验证 0 为假、1／2／255 为真，共 5 项。探针通过相对路径包含原辅助源码，不复制实现，也不把外部函数先当成布尔返回再转换。

汇编检查在本测试安装包内部执行，适用当前模拟器通过原生桥加载手机架构动态库的路径，不依赖运行手机架构的命令行程序。预期类型断言会经过默认恐慌记录器；是否通过仍以独立标签下的完整 41 项记录和汇总为准。

## 构建步骤

以下示例使用 `PowerShell`（命令行外壳）。命令应在仓库内、固定编译器版本 1.93.1 的现有安卓开发环境中执行。调用方准备已安装的安卓开发工具和打包插件，复用已有任务目标目录。不要清理主构建缓存，不需要媒体库、推理库或界面代码生成。

调用方明确设置以下环境变量；工具位置和设备编号不在本目录内硬编码：

| 环境变量 | 内容 |
| --- | --- |
| `BKCAT_PROBE_CARGO`（探针构建工具） | 固定版本构建工具的绝对路径。 |
| `CARGO_TARGET_DIR`（构建目标目录） | 当前任务已有的目标目录绝对路径。 |
| `BKCAT_PROBE_ADB`（探针安卓调试桥） | 调用方实际使用的安卓调试桥可执行文件绝对路径。 |
| `BKCAT_PROBE_DEVICE`（探针设备编号） | 本次设备的明确序列号。 |
| `BKCAT_PROBE_APK`（探针安装包） | 本次构建实际产出的独立探针安装包绝对路径；运行前设置。 |
| `BKCAT_PROBE_EVIDENCE_DIR`（探针证据目录） | 调用方已创建的本次独立证据目录绝对路径。 |

运行前必须存在本目录的 `Cargo.lock`（独立依赖锁文件）。锁文件缺失时停止，由维护者导入对应验证构建的锁文件；常规验证不执行重新生成，不修改主仓库锁文件。

```powershell
$probeRepoRoot = (& git rev-parse --show-toplevel).Trim()
if ($LASTEXITCODE -ne 0) { throw 'Run from inside the repository' }
$probeRoot = Join-Path $probeRepoRoot 'scripts/ci/android-jni-probe'
$probeManifest = Join-Path $probeRoot 'Cargo.toml'
$probeCargo = $env:BKCAT_PROBE_CARGO
if (-not $probeCargo -or -not (Test-Path -LiteralPath $probeCargo -PathType Leaf)) { throw 'Set BKCAT_PROBE_CARGO to the installed fixed-version tool' }
if (-not $env:CARGO_TARGET_DIR -or -not [IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) { throw 'Set CARGO_TARGET_DIR to the existing task target directory' }
if (-not (Test-Path -LiteralPath (Join-Path $probeRoot 'Cargo.lock') -PathType Leaf)) { throw 'The verified standalone Cargo.lock is required' }
$env:CARGO_BUILD_JOBS = '1'
$env:CARGO_INCREMENTAL = '0'
$env:CARGO_NET_OFFLINE = 'true'
$env:CARGO_PROFILE_DEV_DEBUG = '0'
$env:CARGO_PROFILE_DEV_PANIC = 'unwind'
```

先用以下元数据检查确认两个补丁确实进入依赖图；这一步只解析依赖：

```powershell
$probeMetaText = & $probeCargo metadata --manifest-path $probeManifest --format-version 1 --locked --offline --filter-platform aarch64-linux-android
if ($LASTEXITCODE -ne 0) { throw 'Probe metadata failed' }
$probeMeta = $probeMetaText | ConvertFrom-Json
foreach ($probePatchName in @('jni', 'jni-macros')) {
    $probePackage = @($probeMeta.packages | Where-Object { $_.name -eq $probePatchName })
    $probeExpectedPatch = [IO.Path]::GetFullPath((Join-Path $probeRoot "..\..\..\src\patches\$probePatchName\Cargo.toml"))
    if ($probePackage.Count -ne 1 -or $probePackage[0].version -ne '0.22.4' -or $null -ne $probePackage[0].source -or [IO.Path]::GetFullPath($probePackage[0].manifest_path) -ne $probeExpectedPatch) {
        throw "Probe must use the local patch: $probePatchName"
    }
}
```

检查通过后，在调用方现有资源保护外壳内执行，沿用内存保留阈值与本任务进程限制：

```powershell
$probeLockBefore = (Get-FileHash (Join-Path $probeRoot 'Cargo.lock')).Hash
& $probeCargo apk build --manifest-path $probeManifest --lib --target aarch64-linux-android
if ($LASTEXITCODE -ne 0) { throw 'Probe build failed' }
if ((Get-FileHash (Join-Path $probeRoot 'Cargo.lock')).Hash -ne $probeLockBefore) { throw 'Probe lock changed during build' }
```

上述命令使用现有安卓打包插件和调试签名方式；构建脚本将极小测试对象编为内存类加载器使用的安卓字节码，并要求链接阶段没有未定义符号。只使用开发配置，不传入发布配置。锁文件、两个补丁源码散列、共享辅助源码散列、编译退出码与安装包散列应随本次原生验证证据保存。

已验证的安卓打包插件不接受 `--locked`（锁定依赖）、`--offline`（离线）和 `-j`（任务数）的打包命令行参数，因此示例没有把它们传给打包插件。离线与单任务由上面明确设置的环境变量控制；构建工具本身的元数据命令支持锁定参数，负责先检查锁定解析，编译前后再比较锁文件散列。平台过滤避免解析本次不编译的其他平台依赖。不能因插件不支持这些参数就取消锁文件检查。

## 运行与日志判定

安装包名称为 `BKCAT-JniRegression.apk`（独立探针安装包）；以打包工具实际输出的位置为准。只安装、停止这个独立测试应用，不修改编辑器应用数据。

安装与采集窗口均从调用方提供的环境中初始化以下变量。证据目录必须专用于本次验证；日志文件不得追加到历史完整记录：

```powershell
$probeAdb = $env:BKCAT_PROBE_ADB
$probeDevice = $env:BKCAT_PROBE_DEVICE
$probeApk = $env:BKCAT_PROBE_APK
$probeEvidenceDir = $env:BKCAT_PROBE_EVIDENCE_DIR
if (-not $probeAdb -or -not (Test-Path -LiteralPath $probeAdb -PathType Leaf)) { throw 'Set BKCAT_PROBE_ADB to the actual adb executable' }
if (-not $probeDevice) { throw 'Set BKCAT_PROBE_DEVICE to the target serial' }
if (-not $probeApk -or -not (Test-Path -LiteralPath $probeApk -PathType Leaf)) { throw 'Set BKCAT_PROBE_APK to this build output' }
if (-not $probeEvidenceDir -or -not (Test-Path -LiteralPath $probeEvidenceDir -PathType Container)) { throw 'Set BKCAT_PROBE_EVIDENCE_DIR to a fresh evidence directory' }
$probeLog = Join-Path $probeEvidenceDir 'runtime-log.txt'
```

先安装并停止独立测试应用，核对安装退出码：

```powershell
& $probeAdb -s $probeDevice install -r $probeApk
if ($LASTEXITCODE -ne 0) { throw 'Probe installation failed' }
& $probeAdb -s $probeDevice shell am force-stop app.concat.jniregression
if ($LASTEXITCODE -ne 0) { throw 'Probe stop failed' }
```

启动前在采集窗口执行以下命令，覆盖本次日志文件并开始采集。保留一条已有记录作为日志缓冲起点，不清空设备日志；单条历史记录无法满足完整起始、全部项目及最终汇总要求：

```powershell
& $probeAdb -s $probeDevice logcat -v threadtime -T 1 'BKCAT-JNI-PROBE:I' '*:S' |
    Tee-Object -FilePath $probeLog
```

确认采集进程已经运行后，在安装窗口启动应用：

```powershell
& $probeAdb -s $probeDevice shell am start -n app.concat.jniregression/android.app.NativeActivity
if ($LASTEXITCODE -ne 0) { throw 'Probe launch failed' }
```

观察到最终结果后结束日志采集。调用方把本次启动的 `BEGIN`（起始记录）中的运行编号保存到 `BKCAT_PROBE_RUN_ID`（本次运行编号）环境变量，再显式核验该编号；没有本次起始记录时判为失败：

```powershell
$probeRunId = $env:BKCAT_PROBE_RUN_ID
if (-not $probeRunId) { throw 'A run ID from this launch is required' }
python (Join-Path $probeRoot 'verify-log.py') $probeLog --run-id $probeRunId
if ($LASTEXITCODE -ne 0) { throw 'Probe runtime verification failed' }
```

每个测试项目必须输出 `PASS`（通过），同一次运行最后必须输出完整汇总。判定程序要求 41 个唯一项目全部齐全，并且只检查最新一次运行；也可用 `--run-id`（指定运行编号）选择某一次。它不会把多次运行的部分结果拼成通过。准确汇总为：

```text
RESULT PASS families=9 generated=10 dirty_register=5 cases=41
```

对选定运行编号，缺少起始记录、任意失败、项目遗漏、重复记录、没有最终汇总，均返回非零退出码。应用内部出现断言失败或异常时记录失败并以退出码 1 退出；安卓启动命令自身返回 0 不能证明探针通过。

判定程序只检查传入的日志，不验证设备、已安装包散列或日志新旧。若传入旧的完整成功日志，默认选择的“最新一次”仍是旧日志中的运行；它不能自行发现另一场在应用入口之前退出、没有探针起始记录的启动。因此必须保留本次安装包散列、设备编号、安装和启动结果、新采集日志及明确的本次运行编号。入口前退出且没有本次探针起始记录时，调用方直接判为失败，不能拿历史记录补齐。

此探针通过仅证明列出的真实原生接口回归完成，不替代编辑器、助手、审批或项目保存的产品验收。
