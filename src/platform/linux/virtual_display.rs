//! 被控端的虚拟显示器：借 root 的辅助程序接一路 VKMS 连接器出来，再让合成器认下它。
//!
//! 为什么必须是「辅助程序造、合成器认」这两步：凭空多一块显示器要写 configfs，只有
//! root 能写（见 `crates/vkms`）；而分辨率由合成器决定，因为 VKMS 的连接器只有一份
//! 固定的无 EDID 模式表，首选还是 1024x768。两步都成了，采集那边什么都不用改——
//! 它是按合成器报出来的输出枚举的，新屏自然就在里面。
use anyhow::{Context, Result};

/// 辅助程序装没装。查文件，不连它——握手时连一次会顺手把那个 root 服务拉起来。
pub(crate) fn helper_installed() -> bool {
    openuuyc_vkms::installed()
}

/// 一次被控会话里的虚拟显示器。持有一条到辅助程序的连接。
///
/// 连接本身就是租约：辅助程序在我们断开时把所有连接器还回去，所以进程被杀掉也不会
/// 在用户屏幕上留下收不回的虚拟屏。
pub(crate) struct Session {
    /// 连接按需建立：没租到屏的连接会被辅助程序按空闲超时收掉，所以这里是
    /// `Option`，下次要用再连。
    client: Option<openuuyc_vkms::Client>,
    leased: Vec<String>,
}

impl Session {
    pub(crate) fn open() -> Result<Self> {
        let mut session = Self {
            client: None,
            leased: Vec::new(),
        };
        session.client()?;
        Ok(session)
    }

    /// 拿一条能用的连接。连上先问一句：套接字在、但服务拒绝我们（比如没有活动的
    /// 图形会话）时，这里就要报出来，而不是等主控点了「添加虚拟屏」才发现。
    fn client(&mut self) -> Result<&mut openuuyc_vkms::Client> {
        if self.client.is_none() {
            let mut client = openuuyc_vkms::Client::connect()?;
            let status = client.status().context("虚拟显示器辅助程序不可用")?;
            tracing::debug!(capacity = status.capacity, "虚拟显示器辅助程序已就绪");
            self.client = Some(client);
        }
        self.client.as_mut().context("虚拟显示器辅助程序不可用")
    }

    /// 跑一次请求。连接已经不能用时（辅助程序空闲超时收掉了、或者它重启过）丢掉它
    /// 重连一次再试。
    ///
    /// 重试是安全的：断开连接本身就等于归还这一条连接租的所有连接器，所以死在半路的
    /// 那次请求不会在屏幕上留下收拾不掉的东西。
    fn call<T>(&mut self, run: impl Fn(&mut openuuyc_vkms::Client) -> Result<T>) -> Result<T> {
        let first = run(self.client()?);
        match first {
            Ok(value) => Ok(value),
            Err(error)
                if error
                    .chain()
                    .any(|cause| cause.is::<openuuyc_vkms::Disconnected>()) =>
            {
                tracing::debug!(%error, "虚拟显示器辅助程序的连接已失效，重连一次");
                self.client = None;
                run(self.client()?)
            }
            Err(error) => Err(error),
        }
    }

    /// 要一块 `width`x`height`@`hz` 的屏，返回内核给它的连接器名。
    pub(crate) fn create(&mut self, width: u32, height: u32, hz: u32) -> Result<String> {
        let connector = self.call(|client| client.create(width, height, hz))?;
        // 合成器没接住就立刻还回去：一块没有合成器接管、也没人能用的连接器留在那里，
        // 只会让下一次「添加虚拟屏」挑到它然后失败。
        if let Err(error) = super::display::compositor::adopt(&connector, width, height, hz) {
            if let Err(cleanup) = self.call(|client| client.destroy(&connector)) {
                tracing::error!(%cleanup, connector, "虚拟显示器回滚失败");
            }
            return Err(error);
        }
        self.leased.push(connector.clone());
        Ok(connector)
    }

    pub(crate) fn remove(&mut self, connector: &str) -> Result<()> {
        self.call(|client| client.destroy(connector))?;
        self.leased.retain(|name| name != connector);
        Ok(())
    }

    /// 会话结束时把所有屏还回去。失败也不拦着退出：辅助程序那边看到连接断开一样会还。
    pub(crate) fn release_all(&mut self) {
        for connector in std::mem::take(&mut self.leased) {
            if let Err(error) = self.call(|client| client.destroy(&connector)) {
                tracing::warn!(%error, connector, "虚拟显示器归还失败");
            }
        }
    }
}

/// 虚拟屏辅助程序现在的状态，直接就是界面上要显示的那几句话。
///
/// 每一态都是用户能采取不同行动的分界：没装要去装，服务没启用要去启用，桌面接不住
/// 要换桌面，内核太老要升级，能用就没事。
#[derive(Clone)]
pub(crate) struct Availability {
    label: String,
    detail: Option<String>,
}

impl Availability {
    /// 界面上的短标签。
    pub(crate) fn label(&self) -> &str {
        &self.label
    }
    /// 需要解释时的原因，能用时为 `None`。
    pub(crate) fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

/// 本机现在能不能加虚拟屏。诊断页拿它显示这一行。
///
/// 每一态都对应一件用户能去做的事：装辅助程序、开服务、换桌面、升级内核——所以
/// 「内核不支持」不能和「服务连不上」混着说。
pub(crate) fn availability() -> Availability {
    if !openuuyc_vkms::installed() {
        return Availability {
            label: "未安装".into(),
            detail: Some(
                "安装 openuuyc-vkms 后，被控端才能给主控添加虚拟显示器（需要 systemd 与内核的 VKMS 模块）。"
                    .into(),
            ),
        };
    }
    if !std::path::Path::new(openuuyc_vkms::SOCKET).exists() {
        return Availability {
            label: "已安装，服务未启用".into(),
            detail: Some("运行 systemctl enable --now openuuyc-vkms.socket 之后即可使用。".into()),
        };
    }
    // 服务能起来不等于现在能用：合成器接不住的话，屏建出来也没人把它摆进桌面。
    if !super::display::compositor::available() {
        return Availability {
            label: "已安装，当前桌面不支持".into(),
            detail: Some(super::display::compositor::unavailable_reason()),
        };
    }
    match openuuyc_vkms::probe() {
        Ok(status) => Availability {
            label: match status.capacity {
                0 => "可用".into(),
                capacity => format!("可用（最多 {capacity} 块）"),
            },
            detail: (!status.in_use.is_empty())
                .then(|| format!("已在使用：{}", status.in_use.join("、"))),
        },
        // 「内核不支持 configfs」是这台机器做不了，和「服务没起来」不是一回事，
        // 混在一起说会让人去重装服务，白费功夫。
        Err(error)
            if error
                .chain()
                .any(|cause| cause.is::<openuuyc_vkms::Unsupported>()) =>
        {
            Availability {
                label: "内核不支持".into(),
                detail: Some(format!("{error:#}")),
            }
        }
        Err(error) => Availability {
            label: "已安装，当前不可用".into(),
            detail: Some(format!("{error:#}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机端到端：要装好并启用辅助程序，还要合成器能接管新输出。
    ///
    /// 它会在桌面上真的多出一块屏再撤掉，所以不放进默认测试集：
    /// `cargo test --lib virtual_display -- --ignored --nocapture`
    #[test]
    #[ignore = "需要本机装好并启用 openuuyc-vkms，且合成器能接管新输出"]
    fn leases_a_screen_from_the_helper_and_gives_it_back() {
        let mut session = Session::open().expect("辅助程序不可用");
        let connector = session.create(1920, 1080, 60).expect("无法创建虚拟屏");
        assert!(
            connector.starts_with("Virtual-"),
            "连接器名异常：{connector}"
        );
        // 采集那边必须能按连接器名算出屏幕清单里的 identity 看见它——主控点「添加虚拟屏」
        // 就是在等这一步，等不到会报 -1（屏幕上明明多了一块）。
        let identity = crate::platform::display::topology::identity(&connector);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let screen = loop {
            if let Ok(screens) = crate::platform::capture::screens()
                && let Some(screen) = screens
                    .into_iter()
                    .find(|s| s.identity.as_deref() == Some(identity.as_str()))
            {
                break screen;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{connector}（identity {identity}）没进屏幕清单"
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        };
        println!(
            "{connector}: identity={:?} device_name={} {}x{}",
            screen.identity, screen.device_name, screen.width, screen.height
        );
        session.release_all();
    }
}
