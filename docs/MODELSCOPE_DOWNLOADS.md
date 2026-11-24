# BK CUT（智能剪辑软件）模型文件下载来源

## 默认值与兼容性

没有明确保存来源选择的配置默认使用魔搭社区。明确保存的自动、上游镜像、模型镜像站、原始来源、自定义来源继续保留原含义。设置菜单新增第六项，不改变前五项索引；配置保存稳定值 modelscope（魔搭来源标识）。

魔搭来源仅使用已核对的文件映射。下载先写临时文件，完整接收后核对既有 SHA-256（文件校验值），验证成功才成为可用模型。未收录的模型继续尝试原有可用来源。来源切换不迁移、不删除已下载模型，也不改变应用内部目录标识。

## 经核验的映射

2026-11-23 在正确主机 DESKTOP-C0GEQDQ（开发主机）核实下列魔搭公开仓库；通过 Git LFS（大文件存储）对象标识及长度逐项与 models/manifest.toml（现有模型清单）比较。

| 模型组 | 公开仓库 | 固定提交 | 匹配文件数 |
| --- | --- | --- | --- |
| 转写 | [whisper.cpp（转写模型）](https://modelscope.cn/models/cjc1887415157/whisper.cpp) | ac12dbec310c2fd6e67398e808c40d80210ce4d0 | 6 |
| 笔刷分割 | [slimsam-77-uniform（轻量分割模型）](https://modelscope.cn/models/Xenova/slimsam-77-uniform) | 4c8131c2e433e9fa683a1bd0ee40521774996126 | 2 |
| 语音合成 | [chatterbox-turbo-ONNX（语音合成模型）](https://modelscope.cn/models/ResembleAI/chatterbox-turbo-ONNX) | ccb873afb4591f6b0a52cf9e52ef938e05e8f8f2 | 9 |

共 17 / 22 项具有已核验的精确映射。人物抠像、物体抠像、超分辨率和两个人声分离文件共 5 项尚未建立经核验的魔搭映射，保持已有来源。仓库元数据匹配不代表全部文件已实下载或全部推理功能已实际验收。

地址格式依据魔搭官方 [file_download.py（文件下载实现）](https://github.com/modelscope/modelscope/blob/master/modelscope/hub/file_download.py) 的 get_file_download_url（下载地址构造函数）；每一条映射使用固定提交及真实仓库文件路径，不使用猜测地址或替换模型。

## 实际保存位置与验证

安装版 Windows（桌面系统）沿用系统本地应用数据目录：

- 模型根目录：C:\Users\AI\AppData\Local\app.concat.editor
- 转写模型目录：C:\Users\AI\AppData\Local\app.concat.editor\whisper-models
- 本轮实下载模型：ggml-tiny.bin（多语言小型转写模型），77,691,713 字节。
- 文件校验值：be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21。

现有用户配置未保存下载来源，本轮未从外部改写设置文件。安装后的新代码按照缺省规则显示魔搭来源；明确选择后由原有设置保存路径写入，重启再读取。

下载与加载证据保存在主开发目录下 .build/bk-cut-20261123/modelscope-verification（验证目录）。其中：

- repository-metadata.json（转写仓库元数据）记录 6 个模型的对象及长度。
- additional-repository-metadata.json（补充仓库元数据）记录 11 个额外匹配文件。
- download-result.json（文件及原生库加载回执）确认真实文件校验与原生转写库加载。
- 首次直接库调用未注册处理器后端而触发断言，日志保留；按生产代码初始化后端后加载复核通过。该验证属于模型文件和已安装原生库的可用性，不替代新版应用界面验收。

当前源码生产下载器另由 src/crates/concat-host/examples/modelscope_probe.rs（原生验证程序）验证。它读取真实配置来源，下载到用户指定的空测试目录，调用与产品相同的 models::fetch（模型下载函数），记录最终成功来源主机、文件校验值及保存位置；回退成功不能被记作魔搭下载成功。此程序必须主动运行，不属于普通测试中的隐式网络访问。

检查与运行命令：

~~~text
python scripts/models.py --check
cargo test --locked --offline -p concat-host models::tests
cargo run --locked --offline -p concat-host --example modelscope_probe -- <空测试目录>
~~~

前一条核对模型清单与原生映射一致；测试覆盖缺省值、旧选择、固定来源、未映射回退、受控网络失败回退、断点续传与校验；最后一条执行真实生产下载路径。本轮原生生产下载验证已实际通过：缺省配置解析为魔搭，生产下载器从 modelscope.cn（魔搭官方主机）接收 77,691,713 字节并完成既有文件校验，下载耗时 7.68 秒。回执位于 .build/bk-cut-20261123/modelscope-native-probe-2/native-download-result.json（原生下载回执），日志位于 .build/bk-cut-20261123/modelscope-native-probe-2.log（原生下载日志）。第一次探针因历史环境助手留下空构建目标而未开始下载；保留失败日志，第二次明确桌面目标后通过。此结果仍不替代新版应用界面的点击下载与安装验收。

## 回滚

回滚对应源码提交即可恢复上一版默认来源和映射规则。模型文件与现有内部目录保持不变；已下载且校验一致的模型仍可被上一版本使用。不得为了回滚清空用户模型目录。
