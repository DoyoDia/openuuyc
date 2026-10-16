//! 协助邀请链接（`uuremote://external/assistance?...`）在进程内的投递。
//!
//! 命令行和 Windows WM_COPYDATA 都只校验并入队，控制中心等登录、
//! 凭据提示及已有操作结束后再处理，复用手动输入的连接流程。
//! 队列不绑定窗口；等待期间保留最新邀请，避免积压过期连接请求。
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
            tracing::warn!("无法处理协助链接：{error:#}");
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
