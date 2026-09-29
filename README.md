# OpenUUYC

![OpenUUYC](assets/banner.png)

OpenUUYC 是用 Rust 编写的 UU 远程第三方 Windows 客户端。使用已有的 UU 账号登录，连接和控制远端设备。

项目正在快速迭代，优先完善 Windows；**Windows 正式版完成后会适配 Linux 等其他平台。** 提前移植请注意接口和结构仍会变动，建议先通过 [Issue](https://github.com/djkcyl/openuuyc/issues) 沟通。

## 下载与使用

从 [Releases](https://github.com/djkcyl/openuuyc/releases) 下载 Windows x64 客户端，扫码或短信登录后连接设备，支持 UU 官方客户端。

最新稳定版为 [v0.7.0](https://github.com/djkcyl/openuuyc/releases/tag/v0.7.0)。[v1.0.0-alpha.7 预发布](https://github.com/djkcyl/openuuyc/releases/tag/v1.0.0-alpha.7)提供正在开发的本机被控能力。

**首次升级至 alpha.7 会重置旧版注册身份和登录信息，需要重新登录，设备 ID 会改变。** 旧设备条目请在列表中手动删除。设备资料使用本机真实硬件及当前 Windows 壁纸，上传的壁纸副本添加 OpenUUYC Logo，不修改系统原图。

## 能力表

以下为当前预发布版能力。主控端连接远端设备，被控端接受连接；功能受双方能力和权限限制。

| 功能 | 主控端 | 被控端 | 说明 |
| --- | --- | --- | --- |
| 同账号远程连接 | 支持 | 支持 | 本机在连接设置中开启“允许被控” |
| 设备 ID / 验证码协助 | 支持主动连接 | 待实现 | 主控端支持最近连接、收藏和保存验证码 |
| 画面传输 | 接收与播放 | 采集与编码 | H.264 / H.265 / AV1，支持画质、码率和帧率调整；AV1 需两端 OpenUUYC 及对应硬件能力 |
| 硬件编解码 | DXVA11 解码 | NVENC / AMF / QSV 编码 | 按显卡与驱动能力选择；AV1 支持 4:2:0、8/10 位 |
| 软件编解码 | Rust H.264 解码 | Rust H.264 编码 | 软件路径仅支持 H.264 |
| 真彩与 HDR | 接收与呈现 | 采集与编码 | 按双端能力协商；HDR 需系统及硬件支持 |
| 键盘与鼠标 | 支持 | 支持 | 相对/绝对鼠标、组合键、光标样式与大小同步 |
| 移动端原生触摸 | — | 支持接收 | 可接收移动端官方客户端的触摸输入 |
| 多显示器 | 切屏、多窗口观看 | 多屏采集与独立视频轨道 | 支持屏幕标签拖出窗口 |
| 分辨率与 DPI | 支持调整远端 | 支持接收调整 | 按目标屏幕支持的配置应用 |
| 虚拟屏与超级屏 | 支持操作远端 | 支持创建、删除与恢复 | OpenUUYC 被控端需安装虚拟显示驱动 |
| 无显示器与断屏恢复 | 支持观看 | 常驻虚拟屏接管 | 需安装显示驱动；无屏时断线仍保留，其他可用屏幕接入后回收，保留用户主动切屏选择 |
| 桌面声音 | 支持播放 | 采集与发送 | 支持跟随系统默认或指定播放设备，设备变化后自动恢复采集 |
| 仅音频连接 | 支持 | 支持 | 波形、电平、网络 RTT、精简模式及恢复画面 |
| 麦克风 / 虚拟麦克风 | 支持选择设备并发送 | 支持接收 | 本机接收需安装可选虚拟音频驱动，供录音、语音等应用使用 |
| 音质设置 | 接收音质、麦克风发送可调 | 桌面声音发送可调 | 64 / 128 / 192 / 256 kbps；远程切换需双方 alpha.6+，仅音频默认 256 kbps |
| 虚拟扬声器与麦克风 | 无需本地虚拟声卡 | 支持 | 驱动安装在被控端；默认音频设备可选保持原有、仅调整麦克风、仅调整扬声器或都调整 |
| 剪贴板同步 | 支持 | 待实现 | 主控端支持文字、富文本、图片及文件/文件夹复制粘贴 |
| 独立文件传输 | 支持 | 待实现 | 目录浏览、双向传输、冲突处理、暂停续传及后台任务 |
| TCP 端口转发 | 支持 | 待实现 | 自定义监听地址、连通性检查及流量统计 |
| 批注与指示工具 | 支持发送 | 待实现 | 画笔、形状、撤销重做、激光笔和鼠标指示 |
| 远程电源操作 | 支持 | 待实现 | 主控端按远端能力提供开机、关机和重启 |
| 锁屏 / PIN 页面控制 | 支持连接官方被控端 | 安装服务后支持 | OpenUUYC 被控端通过系统服务采集和处理输入 |
| 更新期间自动恢复 | 保留窗口并自动重连 | 更新前通知主控 | 两端新版 OpenUUYC，设备恢复可连接后重连 |

客户端另提供设备列表与详情、别名管理、自定义快捷键、本机编解码与设备上报诊断、性能监控，以及[插件和节点图](plugins/README.md)。

## 安装与服务

主页“安装服务”会将程序安装到 `Program Files\OpenUUYC`，安装后台服务、输入和显示驱动，创建快捷方式并配置托盘自启动。安装需管理员授权及驱动证书信任；关闭主窗口进入托盘，托盘“退出”结束运行。

虚拟声卡在“连接设置 → 服务管理”中选装。更新会检查已安装组件；卸载可选择保留虚拟显示驱动、虚拟声卡及用户数据。

### 虚拟音频驱动的使用条件

**虚拟音频驱动使用测试签名，尚未取得 Microsoft 正式签名。** 使用虚拟扬声器或接收远程麦克风，需要手动准备测试启动环境；普通画面、键鼠和桌面声音不需要此设置。

- 临时测试：Shift + 重启 → 疑难解答 → 高级选项 → 启动设置 → 重启 → **7 / F7 禁用驱动程序强制签名**，仅本次启动有效。[Windows 说明](https://support.microsoft.com/en-us/windows/experience/startup-boot/windows-startup-settings)
- 持续测试：管理员执行 `bcdedit /set testsigning on` 后重启；恢复时执行 `bcdedit /set testsigning off` 并重启。Secure Boot、BitLocker 或组织策略可能限制操作。[微软说明](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/the-testsigning-boot-configuration-option)

测试启动会降低驱动加载保护，程序不会自动修改这些设置。1.0.0 正式版将在 Windows 完整被控功能完成并验证后发布。

## 构建

需要 Rust stable（MSVC）、Visual Studio C++ 构建工具、Windows SDK、CMake 和 UPX。软件 H.264 编解码使用项目 Rust 核心。

```powershell
git clone https://github.com/djkcyl/openuuyc.git
cd openuuyc
cargo dist
```

产物在 `target/dist/`，默认 UPX 压缩并校验启动。未压缩构建用 `cargo dist --no-upx`；命令行参数见 `--help`。

源码已包含签名驱动包和公钥证书，构建主程序不需要签名私钥。修改驱动及发布前验证见 [驱动构建说明](drivers/README.md)。

## 反馈与许可

问题和建议请提交到 [Issues](https://github.com/djkcyl/openuuyc/issues)。报告问题时附上版本、操作步骤和相关日志，注意去掉账号、验证码等私人信息。

OpenUUYC 非网易官方项目。源码公开，但项目整体未采用开源许可证，使用与分发条件见 [LICENSE](LICENSE)。第三方及派生代码保留各自的许可，详见 [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES)。
