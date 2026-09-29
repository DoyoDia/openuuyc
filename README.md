# OpenUUYC

![OpenUUYC](assets/banner.png)

OpenUUYC 是用 Rust 编写的 UU 远程第三方客户端，支持 Windows 和 Linux。使用已有的 UU 账号登录，连接和控制远端设备。

项目正在快速迭代，优先完善 Windows；**Windows 正式版完成后会适配 Linux 等其他平台。** 提前移植请注意接口和结构仍会变动，建议先通过 [Issue](https://github.com/djkcyl/openuuyc/issues) 沟通。

## 下载与使用

从 [Releases](https://github.com/djkcyl/openuuyc/releases) 下载 Windows x64 客户端，扫码或短信登录后连接设备，支持 UU 官方客户端。

最新稳定版为 [v0.7.0](https://github.com/djkcyl/openuuyc/releases/tag/v0.7.0)。[v1.0.0-alpha.6 预发布](https://github.com/djkcyl/openuuyc/releases/tag/v1.0.0-alpha.6)提供正在开发的本机被控能力。

## 能力表

以下为当前预发布版在 **Windows x64** 上的能力；Linux x64 需自行构建，差异见[下文](#linux-版差异)。主控端连接远端设备，被控端接受连接；功能受双方能力和权限限制。

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
| 无显示器与断屏恢复 | 支持观看 | 临时虚拟屏接管 | 需安装显示驱动；原屏恢复后切回，保留用户主动切屏选择 |
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

客户端另提供设备列表与详情、别名管理、自定义快捷键、本机编解码诊断、性能监控，以及[插件和节点图](plugins/README.md)。

## 安装与服务

主页“安装服务”会将程序安装到 `Program Files\OpenUUYC`，安装后台服务、输入和显示驱动，创建快捷方式并配置托盘自启动。安装需管理员授权及驱动证书信任；关闭主窗口进入托盘，托盘“退出”结束运行。

虚拟声卡在“连接设置 → 服务管理”中选装。更新会检查已安装组件；卸载可选择保留虚拟显示驱动、虚拟声卡及用户数据。

### 虚拟音频驱动的使用条件

**虚拟音频驱动使用测试签名，尚未取得 Microsoft 正式签名。** 使用虚拟扬声器或接收远程麦克风，需要手动准备测试启动环境；普通画面、键鼠和桌面声音不需要此设置。

- 临时测试：Shift + 重启 → 疑难解答 → 高级选项 → 启动设置 → 重启 → **7 / F7 禁用驱动程序强制签名**，仅本次启动有效。[Windows 说明](https://support.microsoft.com/en-us/windows/experience/startup-boot/windows-startup-settings)
- 持续测试：管理员执行 `bcdedit /set testsigning on` 后重启；恢复时执行 `bcdedit /set testsigning off` 并重启。Secure Boot、BitLocker 或组织策略可能限制操作。[微软说明](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/the-testsigning-boot-configuration-option)

测试启动会降低驱动加载保护，程序不会自动修改这些设置。1.0.0 正式版将在 Windows 完整被控功能完成并验证后发布。

## Linux 版差异

Linux 版可以登录、管理设备、观看和控制远端桌面，界面走 wgpu（Vulkan，缺失时回退 OpenGL），X11 与 Wayland 均可运行。本机被控在 Xorg 会话中可用。与 Windows 版相比：

- **解码**：H.264 通过 VA-API 硬件解码（Constrained Baseline、Main、High，8 位 4:2:0），其余 H.264 格式使用 Rust 软件解码；H.265 与 AV1 暂不支持（不会向对端声明，对端会改用 H.264）。硬件解码需要对应的 VA-API 驱动。
- **剪贴板**：文字、图片与文件双向同步。粘贴远端复制的文件时，文件通过挂载在 `$XDG_RUNTIME_DIR` 下的只读 FUSE 文件系统按需读取，需要安装 `fuse3`。
- **本机被控**：在已登录的桌面内运行。画面采集按 Sunshine 的优先级选择：Xorg 下先用 NVIDIA NvFBC（画面直接抓进显存交给 NVENC，仅在 NVENC 可用时选用），再到 X11 MIT-SHM，最后是 XDG 屏幕共享门户（PipeWire）；Wayland 下只用门户。门户第一次使用时需要在本机屏幕上点“共享”，授权会被记住（`~/.local/share/OpenUUYC/screencast-restore-token`，在系统隐私设置中可撤销）；可用环境变量 `OPENUUYC_CAPTURE`（如 `nvfbc`、`x11,portal`）指定顺序。编码优先用 NVENC（H.264/H.265，4:2:0 与 4:4:4，8 位；AV1 为 4:2:0 8 位，需要带 AV1 编码单元的显卡，尚未在这类显卡上实测），不可用时回退 Rust H.264 软件编码；桌面声音参照 Sunshine 经 PulseAudio 客户端接口录制所选播放设备的监听源（PulseAudio 与 PipeWire 均可）；该设备静音或音量为 0 时可能录不到声音；键鼠经 XTest 注入（按物理键位映射，文字输入不依赖键盘布局，移动端触摸按单指指针模拟），因此键鼠目前只在 Xorg 会话可用；支持物理多屏与通过 RandR 切换分辨率，退出时恢复。暂无 AMD/Intel 硬件编码、10 位与 HDR、KMS 采集、虚拟屏、超级屏、按显示器 DPI、远端麦克风与调整默认音频设备（OpenUUYC Audio 虚拟声卡是 Windows 驱动；Linux 上开启时会提示暂不支持），也没有后台服务，因此无法在登录界面或锁屏时被控。
- **安装与托盘**：没有“安装服务”，直接运行构建出的程序。关闭窗口隐藏到托盘（StatusNotifierItem；GNOME 需 AppIndicator 扩展，Ubuntu 默认启用），桌面没有托盘时关闭即退出。
- **未接入**：批注与白板、插件节点图、多显示器独立窗口、HDR、全局快捷键（快捷键仅在播放窗口获得焦点时生效），以及诊断页中的解码检查。
- **凭据**：登录态保存在系统密钥环（Secret Service），需要运行 gnome-keyring、KWallet 等服务；没有明文回退。

Wayland 下窗口的拖动与缩放由合成器接管，因此不支持窗口吸附等 Windows 专有行为。

## 构建

### Windows

需要 Rust stable（MSVC）、Visual Studio C++ 构建工具、Windows SDK、CMake 和 UPX。软件 H.264 编解码使用项目 Rust 核心。

```powershell
git clone https://github.com/djkcyl/openuuyc.git
cd openuuyc
cargo dist
```

产物在 `target/dist/`，默认 UPX 压缩并校验启动。未压缩构建用 `cargo dist --no-upx`；命令行参数见 `--help`。

源码已包含签名驱动包和公钥证书，构建主程序不需要签名私钥。修改驱动及发布前验证见 [驱动构建说明](drivers/README.md)。

### Linux

需要 Rust stable（edition 2024）与 C/C++ 工具链。Ubuntu 22.04 及以上：

```bash
sudo apt install build-essential cmake clang pkg-config libva-dev \
    libasound2-dev libdbus-1-dev libxkbcommon-dev libxkbcommon-x11-dev \
    libwayland-dev libx11-dev libxcb1-dev libxrandr-dev libxi-dev libxcursor-dev \
    libgl1-mesa-dev libvulkan-dev libudev-dev libssl-dev fonts-noto-cjk fuse3
git clone https://github.com/djkcyl/openuuyc.git
cd openuuyc
cargo build --release
./target/release/OpenUUYC gui
```

`cargo dist` 只用于 Windows 打包，Linux 直接用 `cargo build`。

## 反馈与许可

问题和建议请提交到 [Issues](https://github.com/djkcyl/openuuyc/issues)。报告问题时附上版本、操作步骤和相关日志，注意去掉账号、验证码等私人信息。

OpenUUYC 非网易官方项目。源码公开，但项目整体未采用开源许可证，使用与分发条件见 [LICENSE](LICENSE)。第三方及派生代码保留各自的许可，详见 [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES)。
