# OpenUUYC

![OpenUUYC](assets/banner.png)

OpenUUYC 是用 Rust 编写的 UU 远程第三方 Windows 客户端。使用已有的 UU 账号登录，连接和控制远端设备。

## 下载与使用

从 [Releases](https://github.com/djkcyl/openuuyc/releases) 下载 Windows x64 客户端，双击运行，扫码或短信登录后选择设备连接。主控端支持连接 UU 官方客户端；本机被控功能见下文。

最新稳定版为 [v0.7.0](https://github.com/djkcyl/openuuyc/releases/tag/v0.7.0)。[v1.0.0-alpha.2 预发布](https://github.com/djkcyl/openuuyc/releases/tag/v1.0.0-alpha.2)提供正在开发的本机被控能力。

## 功能

- **设备管理**：设备列表与详情、修改别名、远程开关机和重启；支持通过设备 ID 连接，以及最近连接和收藏。
- **远程控制**：键鼠操作、多种鼠标模式、自定义快捷键和设备快速切换。
- **麦克风**：将本机麦克风发送到远端 Windows 设备；支持选择输入设备或跟随系统默认，在播放器标题栏开启或关闭。
- **文件传输**：双栏浏览、文件和文件夹双向传输、目录管理、暂停续传及同名文件处理；关闭窗口后可继续传输。
- **剪贴板同步**：文字、图片及文件复制粘贴。
- **端口转发**：TCP 映射、自定义监听地址、连通性探测、网速与流量统计；支持后台运行。
- **多显示器**：切屏、拖出独立窗口、调整分辨率与 DPI，支持虚拟屏和超级屏。
- **画质设置**：画质档位、自定义码率、帧率、YUV 4:4:4 和 HDR；支持硬件解码、音频播放与性能监控。
- **批注**：画笔、形状、擦除、撤销重做，以及激光笔和鼠标指示；笔迹在远端可见。
- **插件**：用节点图组合画面处理效果，提供[滤镜示例与插件 SDK](plugins/README.md)。

上述功能主要面向主控端。目前仅支持 Windows x64。

当前源码中的本机被控支持同账号连接后的画面采集与编码、键鼠输入、物理多屏、分辨率/DPI、虚拟屏和超级屏及退出恢复，在连接设置中开启“允许被控”。

主页“安装服务”将程序安装到 `Program Files\OpenUUYC`，统一安装后台服务、自有输入驱动和 OpenUUYC 虚拟显示驱动，并创建桌面和开始菜单快捷方式。两类驱动共用签名证书，安装需确认机器证书信任及管理员授权。安装完成后自动打开安装目录中的程序，之后不再依赖下载文件。登录自动启动托盘，关闭窗口隐藏到托盘，托盘“退出”停止本次运行。连接设置提供服务卸载；Windows 应用列表提供完整卸载，可分别选择是否卸载虚拟显示驱动、是否删除本机登录信息、设置、插件、缓存和日志，默认保留数据。

本机被控的桌面音频、剪贴板、文件等能力仍在开发，主控端已有对应功能不代表本机已能接收这些操作。1.0.0 正式版留待约定范围内的完整被控能力完成并验证后发布。

## 构建

需要 Rust stable（MSVC）、Visual Studio C++ 构建工具、Windows SDK、CMake 和 UPX，确保 `upx` 在 PATH 中。软件 H.264 编解码使用项目 Rust 核心。

```powershell
git clone https://github.com/djkcyl/openuuyc.git
cd openuuyc
cargo dist
```

程序位于 `target/dist/upx/`，打包时自动进行 UPX 压缩、完整性和启动检查。命令行用法可通过程序的 `--help` 查看。

命令行支持 `gui`（默认）、`login`、`devices`、`connect` 和 `uninstall`。`devices` 会列出设备 ID；连接时使用完整设备名或 `connect --device-id <ID>`，二者选一。`gui --background` 启动到托盘；观看的帧率、编码、硬件解码和链路选项见对应命令的 `--help`。`--log-level`、`--log-file` 可用于指定诊断日志。

源码已包含签名驱动包和公钥证书，构建主程序不需要签名私钥。修改驱动及发布前验证见 [驱动构建说明](drivers/README.md)。

## 反馈与许可

问题和建议请提交到 [Issues](https://github.com/djkcyl/openuuyc/issues)。报告问题时附上版本、操作步骤和相关日志，注意去掉账号、验证码等私人信息。

OpenUUYC 非网易官方项目。源码公开，但项目整体未采用开源许可证，使用与分发条件见 [LICENSE](LICENSE)。第三方及派生代码保留各自的许可，详见 [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES)。
