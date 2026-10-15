//! The unattended host service, as far as Linux has one: a per-user systemd
//! unit that follows the graphical session.
//!
//! Windows installs a system service that keeps the account online and owns
//! controlled sessions without a signed-in desktop, with its credentials in a
//! machine vault. Linux has no equivalent and cannot get one cheaply: capture,
//! clipboard and audio all come from the compositor session, so a host with
//! nobody signed in has no picture to send. What Linux does have is
//! [`startup`]: a systemd user unit, `WantedBy=graphical-session.target`, that
//! runs the control center with `--background` the way Sunshine runs, so the
//! desktop client still owns its own account and presence, and every answer
//! below is still the portable mode's. A machine-wide service would only be
//! worth it together with a capture path that does not need the session (KMS),
//! or a virtual display, and would replace these answers.
use anyhow::{Context, Result, bail};
use std::path::PathBuf;

const NO_SERVICE: &str = "Linux 尚未提供后台被控服务";

/// Whether an install, update or uninstall is replacing the service now.
pub(crate) fn maintaining() -> bool {
    false
}

pub(crate) mod install {
    use super::*;

    /// No service is ever installed, so the portable client does its own
    /// background upkeep.
    pub(crate) fn running() -> Result<bool> {
        Ok(false)
    }
}

pub(crate) mod vault {
    use super::*;

    /// Credentials stay in the user's Secret Service keyring.
    pub(crate) fn applies() -> Result<bool> {
        Ok(false)
    }
    pub(crate) fn root() -> Result<PathBuf> {
        bail!(NO_SERVICE)
    }
    pub(crate) fn owner() -> Result<Option<String>> {
        Ok(None)
    }
    pub(crate) fn sid(_pid: u32) -> Result<String> {
        bail!(NO_SERVICE)
    }
    pub(crate) fn key(service: &str, account: &str) -> String {
        use sha2::{Digest, Sha256};
        format!(
            "{:x}.secret",
            Sha256::digest(format!("{service}\0{account}"))
        )
    }
    pub(crate) fn read(_key: &str) -> Result<Option<Vec<u8>>> {
        bail!(NO_SERVICE)
    }
    pub(crate) fn write(_key: &str, _value: Option<&[u8]>) -> Result<()> {
        bail!(NO_SERVICE)
    }
}

pub(crate) mod resident {
    use super::*;
    pub(crate) use crate::session::resident::{Reply, Request, Snapshot};

    /// This process is never the service's resident account owner.
    pub(crate) fn is_owner() -> bool {
        false
    }
    /// The desktop client always owns the account itself.
    pub(crate) fn managed() -> bool {
        false
    }
    pub(crate) async fn request(_request: Request) -> Result<Reply> {
        bail!(NO_SERVICE)
    }
    pub(crate) fn call(_request: Request) -> Result<Reply> {
        bail!(NO_SERVICE)
    }
    /// Only asked of a managed client; with no service there is nothing to retire.
    pub(crate) async fn pause_for_exit() -> Result<()> {
        Ok(())
    }
}

/// Work the resident owner waits for before it reports itself paused. With no
/// service there is no owner waiting, so nothing is counted.
pub(crate) mod activity {
    pub(crate) struct Work;
    impl Work {
        pub fn new() -> Self {
            Self
        }
    }
}

/// 登录自启：一个 systemd 用户单元，等价于 Windows 的每用户 Run 键。
///
/// 单元跟着 `graphical-session.target` 走，理由是被控要有画面可采——会话结束服务
/// 就该停，也因此在没人登录时不会硬撑（Linux 上那本来也没有画面可采）。代价是
/// 桌面得自己接进这个 target（GNOME、KDE、uwsm 会，XFCE、MATE、Cinnamon 与直接
/// 启动的 sway、Hyprland 不会），见 [`startup::unavailable`]。
pub(crate) mod startup {
    use super::*;
    use std::path::Path;
    use std::sync::OnceLock;

    const UNIT: &str = "openuuyc.service";
    /// systemd 的用户单元搜索路径里由发行版安装的那几处。包装的单元是权威版本，
    /// 便携运行时系统里没有，才按当前可执行文件另写一份到用户配置目录。
    const PACKAGED_UNITS: [&str; 3] = [
        "/usr/lib/systemd/user/openuuyc.service",
        "/usr/local/lib/systemd/user/openuuyc.service",
        "/etc/systemd/user/openuuyc.service",
    ];
    /// 这条自启要求桌面自己接进 `graphical-session.target`：GNOME、KDE、uwsm 会接，
    /// XFCE、MATE、Cinnamon 与直接启动的 sway、Hyprland 不会，那些桌面上单元 enable
    /// 了也没人拉它；没有 systemd 的发行版连 systemctl 都没有。
    const NO_SESSION_TARGET: &str = "当前桌面没有接入 systemd 的 graphical-session.target（XFCE、MATE、Cinnamon，以及直接启动的 sway、Hyprland 等），systemd 用户单元不会被拉起；请改用桌面的自启动设置";
    /// 本进程就在这个图形会话里跑，答案在运行期间不会变，所以只查一次。
    static REACHABLE: OnceLock<bool> = OnceLock::new();

    fn config_dir() -> Result<PathBuf> {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .context("无法确定用户配置目录")
    }

    /// 我们自己写的那份单元，与包装的同名单元不同路径。
    fn generated_unit() -> Result<PathBuf> {
        Ok(config_dir()?.join("systemd/user").join(UNIT))
    }

    /// systemd 现在能不能看到这个单元，包装的或我们自己写的都算。
    fn unit_installed() -> Result<bool> {
        Ok(
            generated_unit()?.exists()
                || PACKAGED_UNITS.iter().any(|unit| Path::new(unit).exists()),
        )
    }

    /// 早先版本写的是 XDG 自启动条目，两份同时在会让登录时起两次（第二份被实例锁
    /// 挡掉，只是白跑一趟）。所以启用单元时把它清掉，关掉时也一样。
    fn legacy_autostart() -> Result<PathBuf> {
        Ok(config_dir()?.join("autostart/openuuyc.desktop"))
    }

    /// 旧条目会不会真的在登录时启动程序：桌面被明确告知跳过它就不算。
    fn legacy_autostart_runs() -> Result<bool> {
        let entry = legacy_autostart()?;
        let text = match std::fs::read_to_string(&entry) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            result => result.with_context(|| format!("无法读取自启动设置 {}", entry.display()))?,
        };
        Ok(!text.lines().map(str::trim).any(|line| {
            line.eq_ignore_ascii_case("Hidden=true")
                || line.eq_ignore_ascii_case("X-GNOME-Autostart-enabled=false")
        }))
    }

    fn systemctl(args: &[&str]) -> Result<String> {
        let output = std::process::Command::new("systemctl")
            .arg("--user")
            .args(args)
            .output()
            .context("无法执行 systemctl")?;
        // 有的子命令（is-enabled）把状态写在标准输出上、错误时标准错误是空的。
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let reason = if stderr.is_empty() {
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        } else {
            stderr
        };
        anyhow::ensure!(
            output.status.success(),
            "systemctl --user {} 失败：{reason}",
            args.join(" ")
        );
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// 桌面有没有把自己接进 `graphical-session.target`。
    fn session_reachable() -> bool {
        *REACHABLE.get_or_init(|| {
            std::process::Command::new("systemctl")
                .arg("--user")
                .args(["is-active", "graphical-session.target"])
                .output()
                .is_ok_and(|output| output.status.success())
        })
    }

    /// 这条自启在当前桌面上不可用时的原因，界面据此把开关置灰并说明。
    pub(crate) fn unavailable() -> Option<String> {
        (!session_reachable()).then(|| NO_SESSION_TARGET.to_owned())
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum State {
        Disabled,
        Enabled,
        /// Windows keeps a separate Startup Apps approval the program cannot
        /// override. A desktop's disable switch is the unit itself, which the
        /// settings switch rewrites, so it reads as plain `Disabled`.
        #[allow(dead_code)]
        DisabledByWindows,
    }

    /// Whether login starts OpenUUYC. The answer is the unit's own enablement,
    /// so the settings switch and `systemctl --user enable/disable` are the
    /// same thing.
    pub(crate) fn state() -> Result<State> {
        Ok(if enabled()? {
            State::Enabled
        } else {
            State::Disabled
        })
    }

    /// The settings switch. There is no installed copy apart from the running
    /// one, so the unit starts this program, as enabling remote wake does.
    pub(crate) fn configure(enabled: bool) -> Result<()> {
        if !enabled {
            return set_image(false, Path::new(""));
        }
        set_image(true, &std::env::current_exe()?)
    }

    pub(crate) fn set_image(enabled: bool, path: &Path) -> Result<()> {
        let generated = generated_unit()?;
        let legacy = legacy_autostart()?;
        if !enabled {
            // 单元本来就不在时不必让 systemctl 报一次「单元不存在」，那就是我们要的结果。
            if unit_installed()? {
                systemctl(&["disable", UNIT])?;
            }
            // 只删我们自己写的那份；包装的单元归包管。
            if generated.exists() {
                std::fs::remove_file(&generated).context("删除自启动单元失败")?;
                systemctl(&["daemon-reload"])?;
            }
            return match std::fs::remove_file(&legacy) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    Err(error).context("无法删除本程序自启动设置")
                }
                _ => Ok(()),
            };
        }
        // 桌面不参与 `graphical-session.target` 时，enable 会成功但登录时没人拉起
        // 这个单元，界面却显示已开启——在这里就说清楚，别让它静默失效。
        anyhow::ensure!(session_reachable(), NO_SESSION_TARGET);
        anyhow::ensure!(path.is_file(), "尚未部署自启动程序");
        let image = path.to_str().context("程序路径不是 UTF-8")?;
        check_image(image)?;
        // 包装的单元已经把可执行文件位置写死成安装路径；只有在没有包装单元、或者
        // 之前那份本来就是我们自己写的时候，才按当前可执行文件重写一份，免得便携
        // 运行时换了个位置后单元指向一个已经不存在的文件。
        if generated.exists() || !unit_installed()? {
            if let Some(directory) = generated.parent() {
                std::fs::create_dir_all(directory)?;
            }
            std::fs::write(&generated, unit_body(image))
                .with_context(|| format!("无法写入自启动单元 {}", generated.display()))?;
            systemctl(&["daemon-reload"])?;
        }
        // 这里只 enable 不 --now：正在跑的就是本进程，再起一份会被实例锁挡住。
        systemctl(&["enable", UNIT])?;
        match std::fs::remove_file(&legacy) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                tracing::warn!(error = %error, "旧的 XDG 自启动条目没能删除");
            }
            _ => {}
        }
        Ok(())
    }

    /// 单元现在是否已启用。查到的是 systemd 的实际状态，所以界面上的「开机自启」
    /// 与 `systemctl --user enable/disable` 永远是同一件事。升级前写入的 XDG 条目
    /// 登录时照样会启动程序，所以也算已启用：否则升级用户的开关会显示「关」，而
    /// 实际上每次登录都在起。
    pub(crate) fn enabled() -> Result<bool> {
        Ok(unit_enabled()? || legacy_autostart_runs()?)
    }

    fn unit_enabled() -> Result<bool> {
        // 单元没装时不必问 systemctl：v253 起 `is-enabled` 才输出 `not-found`，
        // systemd 249（Ubuntu 22.04，deb 只投这个）对不存在的单元标准输出什么都不
        // 打、只往标准错误写一行就非零退出。
        if !unit_installed()? {
            return Ok(false);
        }
        let output = std::process::Command::new("systemctl")
            .arg("--user")
            .args(["is-enabled", UNIT])
            .output()
            .context("无法执行 systemctl")?;
        // 未启用、被 mask 都是非零退出，状态只写在标准输出上。
        let state = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        match state.as_str() {
            "enabled" | "enabled-runtime" | "alias" | "static" | "indirect" | "generated" => {
                Ok(true)
            }
            "disabled" | "linked" | "linked-runtime" | "masked" | "not-found" => Ok(false),
            // 状态查不到时标准输出是空的，退出码非零同样只是「没启用」。
            "" if !output.status.success() => Ok(false),
            "" => bail!(
                "systemctl --user is-enabled 没有给出状态：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            other => bail!("无法识别的单元状态：{other}"),
        }
    }

    /// 单元内容。包里的 `packaging/linux/openuuyc.service` 是同一份定义，改这里时
    /// 要一起改。
    fn unit_body(image: &str) -> String {
        let specifier = image.replace('%', "%%");
        format!(
            "[Unit]\n\
             Description=OpenUUYC 远程被控\n\
             # 被控要有画面可采，所以跟着图形会话走：会话结束就停，也不在没人登录时硬撑。\n\
             PartOf=graphical-session.target\n\
             After=graphical-session.target xdg-desktop-portal.service\n\
             # 程序不在时（例如已卸载，只剩这份写在 ~/.config 里的单元）跳过这次启动：\n\
             # 条件不满足是 skipped 而不是 failed，Restart= 不会每 3 秒再试一次。\n\
             ConditionFileIsExecutable={}\n\
             # 崩了要一直回来，所以不要默认的启动限速。\n\
             StartLimitIntervalSec=0\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart={} gui --background\n\
             # 崩了自动回来；从托盘退出是正常退出，不会又被拉起来。\n\
             Restart=on-failure\n\
             RestartSec=3\n\
             # 默认的 control-group 会把子进程一起杀掉：显示恢复守护（`process_group(0)`）\n\
             # 本该跨崩溃活着，`xdg-open` 打开的浏览器也不该跟着退。\n\
             KillMode=process\n\
             \n\
             [Install]\n\
             WantedBy=graphical-session.target\n",
            specifier,
            exec_quote(image)
        )
    }

    /// 单元里表达不了的可执行路径：换行会把一行写成两行，`"` 与 `\` 在 systemd 的
    /// 命令行解析里是特殊字符（`Executable path contains special characters`）。
    fn check_image(image: &str) -> Result<()> {
        anyhow::ensure!(!image.contains(['\n', '\r']), "程序路径含换行");
        anyhow::ensure!(
            !image.contains(['"', '\\']),
            "程序路径含引号或反斜杠，无法写入自启动单元"
        );
        Ok(())
    }

    /// `ExecStart` 里 `%` 是说明符，字面量要写两遍；`$` 在可执行路径上既不展开也不
    /// 折叠（`ExecStart="/x/a$$b"` 找的就是 `a$$b`），写两遍反而找不到程序。`"` 与
    /// `\` 由 [`check_image`] 拒绝。
    fn exec_quote(image: &str) -> String {
        format!("\"{}\"", image.replace('%', "%%"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn quotes_and_escapes_the_executable_path() {
            assert_eq!(exec_quote("/usr/bin/OpenUUYC"), r#""/usr/bin/OpenUUYC""#);
            // 说明符要写成字面量；变量展开在可执行路径上不存在，`$` 不能双写
            assert_eq!(exec_quote("/opt/a%b/c$d"), r#""/opt/a%%b/c$d""#);
            // 引号与反斜杠在单元里表达不了，只能拒绝
            assert!(check_image("/usr/bin/OpenUUYC").is_ok());
            assert!(check_image(r#"/opt/a"b\c"#).is_err());
            assert!(check_image("/opt/a\nb").is_err());
        }

        /// 包里的单元与程序自己写的必须说同一件事：包装时用前者，便携运行时用后者。
        #[test]
        fn the_shipped_unit_agrees_with_the_generated_one() {
            let shipped = include_str!("../../../packaging/linux/openuuyc.service");
            let generated = unit_body("/usr/bin/OpenUUYC");
            for line in [
                "PartOf=graphical-session.target",
                "After=graphical-session.target",
                "ConditionFileIsExecutable=/usr/bin/OpenUUYC",
                "StartLimitIntervalSec=0",
                "Type=simple",
                "Restart=on-failure",
                "RestartSec=3",
                "KillMode=process",
                "WantedBy=graphical-session.target",
            ] {
                assert!(shipped.contains(line), "单元文件缺少 {line}");
                assert!(generated.contains(line), "生成的单元缺少 {line}");
            }
            assert!(shipped.contains("ExecStart=/usr/bin/OpenUUYC gui --background"));
        }

        #[test]
        fn unit_follows_the_graphical_session() {
            let body = unit_body("/usr/bin/OpenUUYC");
            assert!(body.contains("ExecStart=\"/usr/bin/OpenUUYC\" gui --background"));
            assert!(body.contains("WantedBy=graphical-session.target"));
            assert!(body.contains("PartOf=graphical-session.target"));
            assert!(body.contains("Restart=on-failure"));
        }
    }
}

/// Holds the per-user lock that lets only one process keep a device's
/// presence room; two owners would keep kicking each other off the server.
pub(crate) struct PresenceReservation(#[allow(dead_code)] std::fs::File);

/// Reserve the device's presence room, or `None` while another process of
/// this user holds it.
pub(crate) fn reserve_presence(device: &str) -> Result<Option<PresenceReservation>> {
    anyhow::ensure!(
        !device.is_empty()
            && device
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "设备标识无效"
    );
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map_or_else(crate::platform::paths::require_local_app_data, Ok)?;
    let directory = base.join("openuuyc");
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!("presence.{device}.lock"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("无法打开在线状态锁 {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(PresenceReservation(file))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

/// Refuse a path that is a symbolic link, the Linux counterpart of the
/// Windows reparse-point check on service-owned directories.
pub(crate) fn reject_reparse(path: &std::path::Path) -> Result<()> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        anyhow::ensure!(!metadata.file_type().is_symlink(), "组件目录不可为符号链接");
    }
    Ok(())
}
