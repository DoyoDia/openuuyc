# Windows 驱动

OpenUUYC 包含两个用户态驱动：输入驱动 `ROOT\OPENUUYC_INPUT` 和虚拟显示驱动 `ROOT\OPENUUYC_DISPLAY`。源码在本目录，签名后的 INF、DLL、CAT 和共用公钥证书在 `assets/drivers/`，由主程序内嵌。

构建主程序直接使用仓库中的驱动包，不需要私钥或安装 WDK。安装驱动时，由程序请求管理员授权，并将 `CN=OpenUUYC Drivers` 证书加入本机 Root 和 TrustedPublisher。这是本项目自签名方案，不是 Microsoft/WHQL 签名；受组织策略限制的设备可能拒绝安装。

## 修改驱动

需要 Visual Studio C++ 工具、WDK 10.0.26100.0，以及当前 Windows 用户证书库中可用的代码签名私钥。

```powershell
./drivers/display/build.ps1 -SigningThumbprint <证书指纹> -Stage
./drivers/input/build.ps1 -SigningThumbprint <同一证书指纹> -Stage
```

修改驱动内容时，增加对应 INF 的 `DriverVer`。更换签名身份时，两种驱动包、公钥证书和安装器中固定的证书指纹必须一起更新。私钥、PFX/P12、密码及本机证书库内容不提交到仓库。

## 发布检查

在已信任项目证书的发布机器上执行：

```powershell
./drivers/verify.ps1
cargo dist
```

验证脚本检查公钥、安装器指纹、DLL/CAT 签名、目录文件成员和源码 INF 一致性，不安装驱动、不修改证书信任。当前包未加时间戳；证书续期时需要重新签名并更新内嵌包。

Release 附件使用 `target/dist/upx/` 中的单个 EXE，驱动和许可证已内嵌；源码仓库保留驱动源码、构建脚本和已签名包。输入驱动使用 Windows 的 VHF/UMDF 组件；不同 Windows 版本和安全策略仍需分别验证。

显示驱动基于 SudoVDA，原始来源及声明见 [UPSTREAM.md](display/UPSTREAM.md)，Microsoft 示例代码许可见 [LICENSE](display/LICENSE)。
