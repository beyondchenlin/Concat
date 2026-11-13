# 第四组系统凭据实现与验证记录

日期：2026-11-13。工作树：`E:\BK-CAT\.build\worktrees\m4-model-settings`。分支：`feature/bk-cat-m4-model-settings`。起点：`7872f3a207cf01be596d7f85abcd6132e9e03801`。

## 已完成范围

公共凭据接口按渠道实例与完整规范化服务来源隔离。来源包含协议、主机、有效端口及完整基路径，长度编码后计算散列；作用域对象只保留散列。秘密值使用可清零的内存包装，不提供序列化、显示或复制接口，调试输出固定脱敏。读、写、删除、状态接口区分缺失、损坏、不可用及不支持，不回退普通明文文件。

微软桌面系统使用当前用户的系统凭据管理器，限定产品命名空间及精确作用域；写入不使用企业漫游。读取仅把系统明确的不存在错误解释为缺失，返回的系统秘密缓冲在释放前擦除。没有枚举、迁移或借用其他项目凭据。

安卓使用系统密钥存储生成每个精确作用域的加密密钥，采用带认证的加密、随机初始向量及作用域认证数据。应用不备份目录只保存密文，临时文件同步后原子替换并同步目录；私有文件加锁，错误不转成缺失或明文后备。原生接口擦除跨语言读写字节数组，没有网页、工具或编辑协议入口。凭据安装与配置保存均不发起模型调用，也不签发联网或费用许可。

安卓适配最终只保留全局应用上下文及固定类引用，不保留活动或活动句柄。后续调用直接附着同一虚拟机；同进程重复安装返回既有安全适配，不替换正在使用的存储。

## 实际验证

| 项目 | 实际结果 | 原始证据 |
| --- | --- | --- |
| 公共凭据集成测试 | 3 项通过；专用系统测试与子进程入口默认忽略，另行显式运行 | `.build/m4-credentials-tests-r2.log` |
| 微软桌面真实系统凭据 | 2 项显式运行通过，包含独立子进程读回、替换、精确来源隔离、删除、损坏值分类 | `.build/m4-windows-vault-r1.log` |
| 修补前完整核心回归 | 63 项通过、0 项失败；涵盖配置、凭据、操作日志、所有权及数据库 | `.build/m4-core-tests-r1.log` |
| 最新严格核心静态检查 | 全目标警告即错误检查通过 | `.build/m4-core-clippy-r3.log` |
| 最新受影响单元组 | 9 项通过，包含重复安装不替换及配置落盘读回错配拒绝 | `.build/m4-affected-lib-tests-r1.log` |
| 安卓源码真实编译 | 5 个实际类使用安卓接口 35 编译通过 | `.build/android-credentials-java-r3/javac.log` |
| 安卓运行组件原策略 | 6 组通过 | `.build/android-credentials-java-r3/policy-tests.log` |
| 凭据加密封装策略 | 7 组通过；使用新生成的桌面测试密钥，覆盖实际认证加密、随机性、作用域绑定、篡改、版本及长度拒绝 | `.build/android-credentials-java-r3/credential-policy-tests.log` |
| 安卓字节码 | 最低接口 26 的字节码生成退出码 0 | `.build/android-credentials-java-r3/dex/classes.dex` |
| 安卓目标限定类型检查 | 实际原生接口库及本次产品源码通过目标类型检查；应用初始化接口使用限定夹具 | `.build/android-credentials-jni-check/check-r3.log` |

字节码的 SHA-256（安全散列算法）为 `ce5794fcc74ecee124edad5c6c2c6edfac2da0cd5cd201ee12fc9ed6bb58f7ca`。字节码只记录校验信息，不作为安装包交付。

完整核心回归和最后 9 项受影响验证分属两个明确源码快照，没有把两份结果合并称为重新运行完整套件。系统凭据测试只创建产品专用随机测试命名空间和值，未读取其他命名空间；本次新建的测试项已逐一删除并确认缺失，测试秘密没有写入证据文件。

## 两轮独立审查及修补

1. 首轮完整审查公共接口与微软桌面适配，发现公开接口缺少文档说明，会阻断警告即错误门禁。已补齐公开常量、类型、变体和方法说明；独立复查关闭该发现，严格核心检查通过。
2. 首轮安卓审查读取实际加密策略、存储实现、原生接口、初始化及构建接线，没有发现来源绑定、错误分类或明文降级问题。该审查不代表系统密钥或设备验证。
3. 第二轮完整源码审查发现安卓进程级存储保留首个活动句柄，活动重建后仍会引用旧活动。已改为初始化捕获全局应用上下文及固定类，后续直接附着虚拟机，重复安装幂等返回既有适配；新增生产安装辅助函数的重复安装测试已运行。主审重新完整读取三份修补差异，确认不再保存旧活动、上下文签名一致、异常清理保持、初始化不会替换已使用的适配，关闭该发现。
4. 两轮审查结论限于实际读取的公共、桌面、安卓凭据源码；真实安卓活动重建与系统存储仍未执行。

## 保留的失败与修复证据

- 首轮锁定依赖测试因统一锁文件尚未更新而拒绝构建；由配置作者统一更新依赖锁后运行通过，没有绕过锁定检查。原日志保留为 `.build/m4-credentials-tests-r1.log`。
- 首轮安卓源码编译发现公共安卓接口没有目录打开标志常量；改为现有公共只读打开方式后通过。原日志保留为 `.build/android-credentials-java-r1/javac.log`。
- 生命周期修补的第二轮限定类型检查发现全局类引用不能用普通克隆得到新全局引用。改为显式创建全局引用后第三轮通过。失败原件 `.build/android-credentials-jni-check/check-r2.log` 与成功原件 `.build/android-credentials-jni-check/check-r3.log` 均保留。
- 原命令行日志及早期差异中的控制台中文解码问题没有人工改写。证据目录额外保留直接从版本工具输出字节写入的 UTF-8（统一字符编码）差异及实际源码散列，便于复核真实内容。

## 未执行与边界

安卓系统密钥提供器、应用私有目录真实写入、原生接口运行、同进程活动重建、安装包安装和真实设备测试均未执行。限定目标类型检查没有完成整应用原生链接。没有模型请求、收费调用、真实渠道验收或网络费用授权。本文不把桌面系统凭据测试、桌面生成的加密测试密钥、源码审查或类编译记为安卓产品验收通过。

本次没有提交或推送，未修改主工作树。平台凭据改动随配置及原生设置集成后，仍须由主流程完成窗口构建、界面验证和提交。

## 原始证据与源码身份

原始日志、三份未经命令行解码的生命周期差异、既有命令行差异原件及校验清单归档在 `docs/development/evidence/m4-credentials-20261113/`。归档保留原始字节，校验清单记录来源路径、长度及散列；不复制测试秘密或字节码产物。源码身份表对应归档时实际内容。

| 实际源文件 | SHA-256（安全散列算法） |
| --- | --- |
| `src/crates/concat-agent/src/credentials.rs` | `155021157e7ce7f11097c8789ec4b1084aa17cb2d2613a7c27336bdb115943bc` |
| `src/crates/concat-agent/src/credentials/windows.rs` | `3933a4dbfc6a755265478233830a78bce11d17c944740a59d99f652df4690561` |
| `src/crates/concat-agent/tests/credentials.rs` | `1ddde539aacacd6044e6e07db6da33e5101a4b42a47d8c16a1716f76fbce78ec` |
| `src/crates/concat-android/java/BKCatCredentials.java` | `dcf3a00077bc1c462514c870c53ee8972fd050daeef77c5ef6bcc0ce2c8f07d6` |
| `src/crates/concat-android/java/BKCatCredentialPolicy.java` | `58907a9298fc38163ae39bc29e6ad287a50784a83b292382b9493c0986c94218` |
| `src/crates/concat-android/src/credentials.rs` | `4538fe953452744f7b8d4b69e38efc940aedf9bfd507f9bcace71c458e3d3741` |
| `src/crates/concat-android/tests/BKCatCredentialPolicyTest.java` | `0cf0ddb3b3168b87b06c0c32bdd72d786eb166c90f1fedf6f938b3ee0dbb3e28` |
| `src/crates/concat-android/Cargo.toml` | `56f45e43c3fb2e634d826ebf3550fc9707cead27b05ac2618238c6b65bca31a7` |
| `src/crates/concat-android/build.rs` | `32be5d8d60e0395fa954cdada4a7d0fc44aa360ebf41b86a29376ab267b983dd` |
| `src/crates/concat-android/src/lib.rs` | `21c8576d822599cd548915f9a952f10c71dc7e71bbe4e68f7a7efdc3468a2921` |
| `src/crates/concat-android/scripts/verify-runtime-java.ps1` | `a81c1cbce1d0b766c72119746b2365e9ae0722b223aa5580627c0e12af76da2d` |
