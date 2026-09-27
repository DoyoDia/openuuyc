# Windows 驱动

OpenUUYC 包含输入驱动 `ROOT\OPENUUYC_INPUT`、虚拟显示驱动 `ROOT\OPENUUYC_DISPLAY`，以及可选的内核虚拟音频驱动 `ROOT\OPENUUYC_AUDIO`。源码在本目录，签名后的 INF、DLL/SYS、CAT 和共用公钥证书在 `assets/drivers/`，由主程序内嵌。

构建主程序直接使用仓库中的驱动包，不需要私钥或安装 WDK。安装驱动时，由程序请求管理员授权，并将 `CN=OpenUUYC Drivers` 证书加入本机 Root 和 TrustedPublisher。这是本项目自签名方案，不是 Microsoft/WHQL 签名；受组织策略限制的设备可能拒绝安装。

**虚拟音频驱动目前只有测试签名。** 用户需要手动启用测试签名模式，或在 Windows 启动设置中为本次启动禁用驱动程序强制签名，使用方法及限制见 [README](../README.md#虚拟音频驱动的使用条件)。程序不自动更改启动安全策略；信任项目证书不能替代内核驱动加载许可。输入和显示驱动保持现有部署方式。

## 修改驱动

需要 Visual Studio C++ 工具、WDK 10.0.26100.0，以及当前 Windows 用户证书库中可用的代码签名私钥。

```powershell
./drivers/display/build.ps1 -SigningThumbprint <证书指纹> -Stage
./drivers/input/build.ps1 -SigningThumbprint <同一证书指纹> -Stage
./drivers/audio/build.ps1 -TestSigningThumbprint <同一证书指纹> -Analyze -Stage
```

修改驱动内容时，增加对应 INF 的 `DriverVer`。更换签名身份时，各驱动包、公钥证书和安装器中固定的证书指纹必须一起更新。私钥、PFX/P12、密码及本机证书库内容不提交到仓库。音频驱动调试包默认输出到 `target/audio-driver`，可通过 `OPENUUYC_AUDIO_DRIVER_DIR` 临时指定主程序内嵌的包目录；未设置时使用仓库内的包。

## 发布检查

在已信任项目证书的发布机器上执行：

```powershell
./drivers/verify.ps1
cargo dist
```

验证脚本检查公钥、安装器指纹、DLL/SYS/CAT 签名、目录文件成员和源码 INF 一致性，不安装驱动、不修改证书信任。音频 INF 以 UTF-16LE 打包。此处的 Authenticode 检查不表示已通过 Microsoft 内核签名或 WHQL；当前包未加时间戳，证书续期时需要重新签名并更新内嵌包。

Release 附件使用 `target/dist/` 中经 UPX 压缩并通过完整性、实际启动检查的单个 EXE，驱动和许可证已内嵌；未加壳的开发构建使用 `cargo dist --no-upx`。源码仓库保留驱动源码、构建脚本和已签名包。输入驱动使用 Windows 的 VHF/UMDF 组件；虚拟音频使用 KMDF/ACX，不同 Windows 版本和安全策略仍需分别验证。

显示驱动基于 SudoVDA，原始来源及声明见 [UPSTREAM.md](display/UPSTREAM.md)，Microsoft 示例代码许可见 [LICENSE](display/LICENSE)。

虚拟音频参考 Microsoft ACX 示例，来源见 [UPSTREAM.md](audio/UPSTREAM.md)，许可见 [LICENSE](audio/LICENSE)。
