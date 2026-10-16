//! 协助邀请链接（`uuremote://external/assistance?...`）在进程内的投递。
//!
//! 链接可能从三个方向进来：命令行参数、Windows 的协议激活转发（WM_COPYDATA）、
//! Linux 的单实例控制通道。它们都只把链接放进这里的队列，由控制中心在下一帧
//! 统一处理：切到「开始协助」页、填好设备 ID 和验证码，再走与手动输入完全一样
//! 的连接流程。
//!
//! 队列是进程级的静态量，不绑定某一个窗口：控制中心可能还没建出来、可能收在
//! 托盘里，链接都得等着。同一时刻只留最新的一条，和官方「同一批请求只保留最新
//! 一条」的取舍一致。
use crate::account::assist::AssistLink;
use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// 一次外部激活：有效的邀请链接，或一条给用户看的失败原因。
#[derive(Debug)]
pub(super) enum Activation {
    Link(AssistLink),
    Failed(String),
}

/// 校验失败的链接最多排这么多条，避免被反复拉起的坏链接刷满内存。
const MAX_FAILED: usize = 4;

#[derive(Default)]
struct Queue {
    link: Option<AssistLink>,
    failed: VecDeque<String>,
}

fn queue() -> &'static Mutex<Queue> {
    static QUEUE: OnceLock<Mutex<Queue>> = OnceLock::new();
    QUEUE.get_or_init(Mutex::default)
}

fn lock() -> MutexGuard<'static, Queue> {
    queue().lock().unwrap_or_else(|poison| poison.into_inner())
}

/// 把一条 `uuremote://` 链接交给控制中心。链接来自外部（系统按协议注册拉起、
/// 另一个进程转发），所以这里只做校验和入队，不假装知道界面现在能不能用。
pub(crate) fn receive(uri: &str) {
    let mut queue = lock();
    match AssistLink::parse(uri) {
        Ok(link) => {
            // 被协助端的登录状态只在这里留个痕迹：连接方式由设备 ID 和验证码
            // 决定，不因为这个标记改道。验证码不记，日志会外带。
            tracing::debug!(
                connect_id = %link.connect_id,
                controlled_login = link.controlled_login,
                "收到协助邀请链接"
            );
            queue.link = Some(link);
        }
        Err(error) => {
            tracing::warn!(uri, "无法处理的协助链接：{error:#}");
            if queue.failed.len() >= MAX_FAILED {
                queue.failed.pop_front();
            }
            queue.failed.push_back(format!("{error:#}"));
        }
    }
}

/// 本帧要处理的激活。界面还在登录、登出或别的写操作里（`settled` 为假）时，
/// 链接先留在队列里等状态稳定；失败原因不占时间，随时可以交给用户。
pub(super) fn take(settled: bool) -> Option<Activation> {
    let mut queue = lock();
    if let Some(message) = queue.failed.pop_front() {
        return Some(Activation::Failed(message));
    }
    settled
        .then(|| queue.link.take())
        .flatten()
        .map(Activation::Link)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 队列是进程级的静态量，只用一个用例跑完两种路径，免得用例之间互相取走
    /// 对方的投递。
    #[test]
    fn queues_the_newest_link_and_reports_failures() {
        receive("uuremote://external/assistance?deviceId=12345");
        receive("uuremote://external/assistance?deviceId=111111111&code=AAAAAAAA");
        receive("uuremote://external/assistance?deviceId=222222222&code=BBBBBBBB");
        // 界面忙的时候先不取链接，但坏链接的理由要立刻送到。
        let Some(Activation::Failed(message)) = take(false) else {
            panic!("校验失败的链接应该直接交给界面");
        };
        assert!(message.contains("9位设备 ID"), "{message}");
        // 界面没稳定之前，排队的链接不该被取走。
        assert!(take(false).is_none());
        let Some(Activation::Link(link)) = take(true) else {
            panic!("队列里应该留着最新的一条链接");
        };
        assert_eq!(link.connect_id, "222222222");
        assert_eq!(link.code, "BBBBBBBB");
        assert!(take(true).is_none(), "被取代的旧链接不该再出现");
    }
}
