//! Desktop media preparation within an already authorized controlled connection.
use super::{VideoConfig, capture, encoder, format};
use crate::features::stream_control::publisher;
use crate::protocol::capability::DeviceCapability;
use anyhow::{Context, Result, ensure};
use std::sync::Arc;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Capabilities {
    pub screen: capture::Screen,
    pub codecs: Vec<format::Capability>,
    pub adapters: Vec<capture::EncodingAdapter>,
}

#[derive(Default)]
pub(super) struct Cache(tokio::sync::Mutex<Option<Arc<Capabilities>>>);

fn same_probe_source(a: &capture::Screen, b: &capture::Screen) -> bool {
    // Placement, label, protocol ID, DPI and display refresh rate do not change
    // this 720p/30fps encoder check. Keep actual source/graphics/size/HDR bounds.
    a.identity == b.identity
        && a.device_name == b.device_name
        && a.adapter == b.adapter
        && a.render_adapter == b.render_adapter
        && a.width == b.width
        && a.height == b.height
        && a.hdr == b.hdr
}
impl Cache {
    pub fn snapshot(&self) -> Option<Arc<Capabilities>> {
        self.0.try_lock().ok()?.clone()
    }
    pub async fn prepare(
        &self,
        screen: capture::Screen,
        active: impl Fn() -> bool + Send + 'static,
    ) -> Result<Arc<Capabilities>> {
        let mut cached = self.0.lock().await;
        ensure!(active(), "媒体准备已取消");
        let adapters = tokio::task::spawn_blocking(capture::encoding_adapters).await??;
        ensure!(active(), "媒体准备已取消");
        if let Some(capabilities) = cached.as_ref()
            && same_probe_source(&capabilities.screen, &screen)
            && capabilities.adapters == adapters
        {
            let capabilities = Arc::new(Capabilities {
                screen,
                codecs: capabilities.codecs.clone(),
                adapters,
            });
            *cached = Some(capabilities.clone());
            tracing::info!("host media capabilities reused");
            return Ok(capabilities);
        }
        let capabilities = Arc::new(probe(screen, adapters, active).await?);
        *cached = Some(capabilities.clone());
        Ok(capabilities)
    }
    pub async fn replace(&self, value: Option<Capabilities>) {
        *self.0.lock().await = value.map(Arc::new);
    }
    pub async fn invalidate(&self) {
        self.0.lock().await.take();
    }
}

async fn probe(
    screen: capture::Screen,
    adapters: Vec<capture::EncodingAdapter>,
    active: impl Fn() -> bool + Send + 'static,
) -> Result<Capabilities> {
    tokio::task::spawn_blocking(move || {
        let started = std::time::Instant::now();
        ensure!(active(), "媒体准备已取消");
        let _runtime = encoder::Runtime::new()?;
        let mut desktop = capture::Desktop::open_selected(&screen)?;
        let codecs = encoder::probe(&mut desktop, &active)?;
        ensure!(active(), "媒体准备已取消");
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis(),
            screen_id = screen.id,
            capabilities = codecs.len(),
            "host media capabilities prepared"
        );
        Ok(Capabilities {
            screen,
            codecs,
            adapters,
        })
    })
    .await
    .context("被控能力检查任务中断")?
}

pub(crate) struct Prepared {
    pub screen: capture::Screen,
    pub negotiated: Arc<format::Negotiated>,
    pub config: VideoConfig,
}
impl Prepared {
    pub fn new(
        options: &publisher::ConnectOptions,
        screen: capture::Screen,
        capabilities: &[format::Capability],
        remote: &DeviceCapability,
        settings: super::EncodingSettings,
    ) -> Result<Self> {
        let negotiated = Arc::new(format::Negotiated::new(
            capabilities,
            remote,
            &options.decoders,
            settings,
            screen.render_adapter.unwrap_or(screen.adapter),
        )?);
        let mut config = publisher::config(options.params.as_ref());
        let chroma = options
            .params
            .as_ref()
            .map_or(1, |p| if p.chroma == 3 { 3 } else { 1 });
        let hdr = options.params.as_ref().is_some_and(|p| p.hdr);
        negotiated.apply(
            &mut config,
            None,
            chroma,
            hdr,
            (screen.width, screen.height),
        )?;
        tracing::info!(
            parameters = ?options.params,
            requested_fps = config.requested_fps,
            fps_limit = config.fps_limit,
            maximum_fps = config.maximum_fps,
            effective_fps = config.fps,
            "host initial capture settings negotiated"
        );
        Ok(Self {
            screen,
            negotiated,
            config,
        })
    }
}
