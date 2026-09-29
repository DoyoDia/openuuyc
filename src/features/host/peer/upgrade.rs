//! Local installer notification over the existing, reliable TEXT channel.
use super::ReportTarget;
use anyhow::{Context, Result, ensure};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;

pub(crate) struct UpdateNotice {
    routes: ReportTarget,
    cancel: CancellationToken,
    sent: tokio::sync::Mutex<bool>,
}
impl UpdateNotice {
    pub(super) fn new(routes: ReportTarget, cancel: CancellationToken) -> Arc<Self> {
        Arc::new(Self {
            routes,
            cancel,
            sent: tokio::sync::Mutex::new(false),
        })
    }
    pub(crate) async fn send(&self) -> Result<()> {
        let mut sent = self.sent.lock().await;
        if *sent {
            return Ok(());
        }
        let operation = async {
            ensure!(!self.cancel.is_cancelled(), "被控连接已结束");
            let channel = self
                .routes
                .borrow()
                .text
                .as_ref()
                .and_then(std::sync::Weak::upgrade)
                .context("更新通知通道尚未就绪")?;
            ensure!(
                channel.ready_state() == RTCDataChannelState::Open
                    && channel.ordered()
                    && channel.max_packet_lifetime().is_none()
                    && channel.max_retransmits().is_none(),
                "更新通知需要已就绪的可靠有序通道"
            );
            let bytes = crate::features::stream_control::publisher::update_started();
            channel.send_text_bytes(&bytes::Bytes::from(bytes)).await?;
            // In our SCTP implementation this counter is released by SACK, not
            // merely when send() queues the message. Keep the connection alive
            // until delivery drains; do not replay this notice over another carrier.
            loop {
                ensure!(
                    channel.ready_state() == RTCDataChannelState::Open,
                    "更新通知通道已关闭"
                );
                if channel.buffered_amount().await == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            anyhow::Ok(())
        };
        tokio::select! {
            _=self.cancel.cancelled()=>anyhow::bail!("被控连接已结束"),
            result=tokio::time::timeout(Duration::from_secs(3),operation)=>result.context("更新通知发送超时，尚未退出旧版本")??,
        }
        *sent = true;
        tracing::info!("local update start delivered to controller");
        Ok(())
    }
}
