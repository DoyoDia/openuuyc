//! Player window for X11 and Wayland.
//!
//! The Windows player composes DXVA surfaces through DirectComposition and
//! takes input from a raw-input hook. Neither exists here, so this presenter is
//! built the other way round: frames land on the CPU (software decode), the
//! wgpu shell draws them under the egui chrome, and input comes from the winit
//! events the compositor delivers to the focused window.
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{
    DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowAttributes, WindowId};

use crate::decoder::RenderSurface;
use crate::remote_input::MouseMode;
use crate::ui::chrome::{
    WindowMoveState, WindowResizeState, cancel_pointer_operation, configure_dwm_window,
    resize_regions, title_bar_height, title_bar_panel, update_nonmodal_window_resize,
    window_title_bar,
};
use crate::ui::gfx::{UiPresenter, UiTimingAudit, split_output};
use crate::ui::wgpu_video::VideoPlacement;
use crate::ui::window_manager::{Event as UiEvent, Repaint as UiRepaintEvent};

use super::{
    ConnectionProgress, ConnectionProgressApp, DecodedVideoFrame, NativeViewerSession,
    PerformancePanelMode, StreamControlUi, ViewerDisplayHandle, ViewerWindowEvent,
    configure_viewer_visuals, install_system_cjk_font, mutex_lock, show_stream_control_window,
    take_next_frame,
};

pub(super) use crate::viewer_shortcuts::Action as ViewerShortcut;

/// Space the caption keeps for the brand logo and the device alias.
const TITLE_CAPTION_WIDTH: f32 = 220.0;

pub(crate) struct ConnectingWindowsRunConfig {
    pub alias: String,
    pub progress: std_mpsc::Receiver<ConnectionProgress>,
    pub session: std_mpsc::Receiver<ViewerWindowEvent>,
    pub display_sender: tokio::sync::oneshot::Sender<ViewerDisplayHandle>,
}

pub(super) fn run(session: NativeViewerSession) -> Result<()> {
    let (sender, receiver) = std_mpsc::channel();
    let alias = session
        .title
        .strip_prefix(crate::VIEWER_TITLE_PREFIX)
        .unwrap_or(&session.title)
        .to_owned();
    sender
        .send(ViewerWindowEvent::Playing(Box::new(session)))
        .map_err(|_| anyhow!("queue existing viewer session"))?;
    run_player(
        ConnectingWindowsRunConfig {
            alias,
            progress: std_mpsc::channel().1,
            session: receiver,
            display_sender: tokio::sync::oneshot::channel().0,
        },
        false,
    )
}

pub(super) fn run_connecting(config: ConnectingWindowsRunConfig) -> Result<()> {
    run_player(config, true)
}

fn run_player(config: ConnectingWindowsRunConfig, needs_display: bool) -> Result<()> {
    let event_loop = EventLoop::<UiEvent>::with_user_event()
        .build()
        .context("create player event loop")?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut runner =
        ConnectingWindowsRunner::new(config, needs_display, event_loop.create_proxy(), false);
    event_loop
        .run_app(&mut runner)
        .map_err(|error| anyhow!("run connection/player event loop: {error}"))?;
    if let Some(error) = runner.fatal_error.take() {
        bail!(error);
    }
    Ok(())
}

fn ui_frame_interval(window: &Window) -> Duration {
    let millihertz = window
        .current_monitor()
        .and_then(|monitor| monitor.refresh_rate_millihertz())
        .filter(|rate| *rate != 0)
        .unwrap_or(60_000);
    Duration::from_secs_f64(1000.0 / f64::from(millihertz))
}

pub(crate) struct ConnectingWindowsRunner {
    embedded: bool,
    alias: String,
    attributes: WindowAttributes,
    progress: Option<std_mpsc::Receiver<ConnectionProgress>>,
    session: std_mpsc::Receiver<ViewerWindowEvent>,
    display_sender: Option<tokio::sync::oneshot::Sender<ViewerDisplayHandle>>,
    window: Option<Arc<Window>>,
    shell: Option<Shell>,
    stage: Stage,
    close_requested: bool,
    fatal_error: Option<String>,
    next_repaint: Option<Instant>,
    last_frame: Option<Instant>,
    ui_frame_interval: Duration,
    proxy: EventLoopProxy<UiEvent>,
    generation: u64,
}

enum Stage {
    Connecting(Box<ConnectionProgressApp>),
    Playing(Box<Player>),
    Finished,
}

/// egui plus the wgpu surface for the one player window.
struct Shell {
    context: egui::Context,
    input: egui_winit::State,
    presenter: UiPresenter,
    move_state: WindowMoveState,
    resize_state: WindowResizeState,
    timing_audit: Option<UiTimingAudit>,
}

impl ConnectingWindowsRunner {
    pub(crate) fn new(
        config: ConnectingWindowsRunConfig,
        needs_display: bool,
        proxy: EventLoopProxy<UiEvent>,
        embedded: bool,
    ) -> Self {
        Self {
            attributes: WindowAttributes::default()
                .with_visible(false)
                .with_title(format!("{}{}", crate::VIEWER_TITLE_PREFIX, config.alias))
                .with_window_icon(Some(crate::ui::branding::window_icon()))
                .with_decorations(false)
                .with_inner_size(LogicalSize::new(1280.0, 760.0))
                .with_min_inner_size(LogicalSize::new(760.0, 520.0)),
            alias: config.alias.clone(),
            progress: Some(config.progress),
            session: config.session,
            display_sender: needs_display.then_some(config.display_sender),
            window: None,
            shell: None,
            stage: Stage::Connecting(Box::new(ConnectionProgressApp::new(
                config.alias,
                std_mpsc::channel().1,
            ))),
            close_requested: false,
            fatal_error: None,
            next_repaint: Some(Instant::now()),
            last_frame: None,
            ui_frame_interval: Duration::from_secs_f64(1.0 / 60.0),
            proxy,
            embedded,
            generation: 1,
        }
    }

    pub(crate) fn closed(&self) -> bool {
        self.close_requested
    }

    pub(crate) fn error(&self) -> Option<String> {
        self.fatal_error.clone()
    }

    pub(crate) fn owns(&self, id: WindowId) -> bool {
        self.window.as_ref().is_some_and(|window| window.id() == id)
    }

    pub(crate) fn focus(&self) {
        if let Some(window) = &self.window {
            window.set_minimized(false);
            window.set_visible(true);
            window.focus_window();
        }
    }

    fn exit(&mut self, event_loop: &ActiveEventLoop) {
        self.close_requested = true;
        self.stage = Stage::Finished;
        if !self.embedded {
            event_loop.exit();
        }
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: &anyhow::Error) {
        self.fatal_error = Some(format!("{error:#}"));
        self.exit(event_loop);
    }

    fn create(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        if self.window.is_some() {
            return Ok(());
        }
        let window = Arc::new(
            event_loop
                .create_window(self.attributes.clone())
                .context("create player window")?,
        );
        crate::ui::branding::set_taskbar_icon(&window);
        configure_dwm_window(&window);
        let context = egui::Context::default();
        context.set_embed_viewports(true);
        install_system_cjk_font(&context);
        configure_viewer_visuals(&context);
        let window_id = window.id();
        let proxy = self.proxy.clone();
        let generation = self.generation;
        context.set_request_repaint_callback(move |info| {
            if info.viewport_id == egui::ViewportId::ROOT
                && let Some(when) = Instant::now().checked_add(info.delay)
            {
                let _ = proxy.send_event(UiEvent::Repaint(UiRepaintEvent {
                    window: window_id,
                    generation,
                    pass: info.current_cumulative_pass_nr,
                    when,
                }));
            }
        });
        let input = egui_winit::State::new(
            context.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            window.theme(),
            None,
        );
        let graphics = crate::ui::gfx::create_device()?;
        let presenter = UiPresenter::new(window.clone(), &graphics)?;
        self.ui_frame_interval = ui_frame_interval(&window);
        self.shell = Some(Shell {
            context,
            input,
            presenter,
            move_state: WindowMoveState::default(),
            resize_state: WindowResizeState::default(),
            timing_audit: None,
        });
        // Only the D3D11 path can hand a decoder its own surfaces; a Linux
        // session always decodes into CPU frames, so the handle stays empty.
        if let Some(sender) = self.display_sender.take() {
            let _ = sender.send(ViewerDisplayHandle::default());
        }
        if let Some(receiver) = self.progress.take() {
            self.stage = Stage::Connecting(Box::new(ConnectionProgressApp::new(
                self.alias.clone(),
                receiver,
            )));
        }
        self.window = Some(window);
        Ok(())
    }

    /// Session hand-offs arrive on a channel: play, close, or reconnect.
    fn poll_session(&mut self, event_loop: &ActiveEventLoop) {
        loop {
            let event = match self.session.try_recv() {
                Ok(event) => event,
                Err(std_mpsc::TryRecvError::Empty) => return,
                Err(std_mpsc::TryRecvError::Disconnected) => return,
            };
            match event {
                ViewerWindowEvent::Close => {
                    self.exit(event_loop);
                    return;
                }
                ViewerWindowEvent::Playing(session) => {
                    let Some(window) = self.window.clone() else {
                        return;
                    };
                    match Player::new(*session, &window, self.proxy.clone()) {
                        Ok(player) => {
                            window.set_visible(true);
                            self.stage = Stage::Playing(Box::new(player));
                        }
                        Err(error) => {
                            self.fail(event_loop, &error);
                            return;
                        }
                    }
                }
                ViewerWindowEvent::Reconnect {
                    alias,
                    window: _,
                    progress,
                    display,
                } => {
                    self.alias = alias.clone();
                    self.generation += 1;
                    let _ = display.send(ViewerDisplayHandle::default());
                    if let Some(shell) = self.shell.as_mut() {
                        shell.presenter.clear_video();
                    }
                    self.stage =
                        Stage::Connecting(Box::new(ConnectionProgressApp::new(alias, progress)));
                }
            }
        }
    }

    fn schedule(&mut self, when: Instant) {
        let when = self
            .last_frame
            .map_or(when, |last| when.max(last + self.ui_frame_interval));
        self.next_repaint = Some(self.next_repaint.map_or(when, |old| old.min(when)));
    }

    fn render(&mut self) -> Result<()> {
        let (Some(window), Some(shell)) = (self.window.clone(), self.shell.as_mut()) else {
            return Ok(());
        };
        self.next_repaint = None;
        let started = Instant::now();
        self.last_frame = Some(started);
        let title = self.alias.clone();
        let mut close_requested = false;
        let mut placement = None;
        let input = shell.input.take_egui_input(&window);
        let stage = &mut self.stage;
        let output = shell.context.run_ui(input, |ui| {
            let ctx = ui.ctx().clone();
            resize_regions(ui, &window, |response, direction| {
                update_nonmodal_window_resize(
                    &ctx,
                    &window,
                    response,
                    direction,
                    &mut shell.resize_state,
                    None,
                );
            });
            if window.fullscreen().is_none() {
                title_bar_panel(ui, "player-window-chrome", title_bar_height(), |ui| {
                    let bar = ui.available_rect_before_wrap();
                    close_requested |=
                        window_title_bar(ui, &window, &title, Some(&mut shell.move_state));
                    if let Stage::Playing(player) = stage {
                        player.title_bar_controls(ui, bar, &window);
                    }
                });
            }
            match stage {
                Stage::Connecting(app) => app.draw(ui),
                Stage::Playing(player) => placement = player.draw(ui, &window),
                Stage::Finished => {}
            }
            crate::ui::controls::show_notices(ui.ctx());
        });
        if let Stage::Playing(player) = &mut self.stage
            && let Some(frame) = player.take_frame()
        {
            player.publish(&frame);
            if let RenderSurface::CpuRgba8(pixels) = &frame.surface {
                shell.presenter.upload_video(
                    frame.width,
                    frame.height,
                    bytemuck::cast_slice(pixels),
                )?;
            }
            player.current = Some(frame);
        }
        let placement = placement.filter(|_| shell.presenter.has_video());
        shell.presenter.set_video_placement(placement);
        let (drawing, platform, viewports) = split_output(output);
        shell.input.handle_platform_output(&window, platform);
        close_requested |= viewports
            .get(&egui::ViewportId::ROOT)
            .is_some_and(|viewport| {
                viewport
                    .commands
                    .iter()
                    .any(|command| matches!(command, egui::ViewportCommand::Close))
            });
        let repaint_at = viewports
            .get(&egui::ViewportId::ROOT)
            .and_then(|viewport| Instant::now().checked_add(viewport.repaint_delay));
        let layout = started.elapsed();
        let mut presented = false;
        if window.is_minimized() == Some(true) {
            shell.presenter.defer_output(drawing);
        } else {
            presented = shell.presenter.render(&shell.context, drawing, false)?;
            if presented && window.is_visible() == Some(false) {
                window.set_visible(true);
            }
        }
        if let Some(audit) = UiTimingAudit::active(&mut shell.timing_audit, started) {
            audit.record(
                started,
                layout,
                started.elapsed().saturating_sub(layout),
                repaint_at.is_some_and(|when| when <= started),
                presented,
            );
        }
        self.close_requested |= close_requested;
        if let Some(when) = repaint_at {
            self.schedule(when);
        }
        Ok(())
    }
}

impl ApplicationHandler<UiEvent> for ConnectingWindowsRunner {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if let Err(error) = self.create(event_loop) {
            self.fail(event_loop, &error);
            return;
        }
        self.poll_session(event_loop);
        if let Err(error) = self.render() {
            self.fail(event_loop, &error);
            return;
        }
        if let Some(window) = &self.window {
            window.set_visible(true);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if !self.owns(id) {
            return;
        }
        let (Some(window), Some(shell)) = (self.window.clone(), self.shell.as_mut()) else {
            return;
        };
        let response = shell.input.on_window_event(&window, &event);
        let consumed = response.consumed;
        if let Stage::Playing(player) = &mut self.stage {
            player.on_window_event(&window, &shell.context, &event, consumed);
        }
        match event {
            WindowEvent::CloseRequested | WindowEvent::Destroyed => {
                self.exit(event_loop);
                return;
            }
            WindowEvent::Focused(focused) => {
                if !focused {
                    cancel_pointer_operation(&mut shell.move_state, &mut shell.resize_state);
                }
                if let Stage::Playing(player) = &mut self.stage {
                    player.on_focus_changed(&window, focused);
                }
                crate::viewer_shortcuts::set_focused_window(if focused {
                    u64::from(id)
                } else {
                    0
                });
                self.schedule(Instant::now());
            }
            WindowEvent::Resized(size) => {
                if let Err(error) = shell.presenter.resize(size) {
                    self.fail(event_loop, &error);
                    return;
                }
                self.schedule(Instant::now());
            }
            WindowEvent::RedrawRequested => {
                if let Err(error) = self.render() {
                    self.fail(event_loop, &error);
                    return;
                }
            }
            _ => {
                if response.repaint {
                    self.schedule(Instant::now());
                }
            }
        }
        if self.close_requested {
            self.exit(event_loop);
        }
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device: DeviceId,
        event: DeviceEvent,
    ) {
        // Raw motion is the only pointer source once the pointer is locked.
        if let Stage::Playing(player) = &mut self.stage
            && player.on_device_event(&event)
        {
            self.schedule(Instant::now());
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UiEvent) {
        let UiEvent::Repaint(event) = event else {
            return;
        };
        if !self.owns(event.window) {
            return;
        }
        self.schedule(event.when);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.poll_session(event_loop);
        if self.close_requested {
            self.exit(event_loop);
            return;
        }
        if let Stage::Playing(player) = &mut self.stage {
            // Decoded frames arrive without waking the event loop, so playback
            // keeps its own cadence at the display refresh rate.
            match player.poll() {
                Ok(()) => self.schedule(Instant::now()),
                Err(error) => {
                    self.fail(event_loop, &error);
                    return;
                }
            }
        }
        match self.next_repaint {
            Some(when) if when <= Instant::now() => {
                if let Err(error) = self.render() {
                    self.fail(event_loop, &error);
                    return;
                }
            }
            _ => {}
        }
        event_loop.set_control_flow(
            self.next_repaint
                .map_or(ControlFlow::Wait, ControlFlow::WaitUntil),
        );
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.stage = Stage::Finished;
        self.shell = None;
        self.window = None;
    }
}

/// The playing half of the window: video, input forwarding and the menus.
struct Player {
    session: NativeViewerSession,
    /// Keeps the decoder publishing frames and nudges the event loop; held for
    /// its lifetime, which is what registers this window with the session.
    #[allow(dead_code, reason = "Owned for its registration and its Drop.")]
    wake: FrameWakeBridge,
    /// Identifies this window to `RemoteInput`, which arbitrates between windows.
    owner: u64,
    stream_control_ui: StreamControlUi,
    performance_mode: PerformancePanelMode,
    intercept_shortcuts: bool,
    current: Option<DecodedVideoFrame>,
    video_size: Option<(u32, u32, u16)>,
    /// Content area of the last frame, used to map pointer positions.
    video_rect: Option<egui::Rect>,
    pointer_in_video: bool,
    last_position: Option<PhysicalPosition<f64>>,
    modifiers: winit::keyboard::ModifiersState,
    held_keys: Vec<u16>,
    /// Everything physically down right now, whether or not it was forwarded.
    /// Entering control waits for this to empty before the remote accepts input.
    physical_keys: Vec<u16>,
    physical_buttons: u8,
    /// Pixels per point of the last frame, for pointer mapping.
    scale: f32,
    /// Last failure from a control request, shown next to the toolbar.
    control_error: Option<String>,
    /// The remote cursor shape, decoded once per distinct image.
    cursor: Option<(usize, egui::TextureHandle, [u32; 2], [u32; 2])>,
    /// Whether the pointer is currently locked to the window for raw motion.
    pointer_locked: bool,
    /// Sub-pixel motion carried between raw events.
    motion_remainder: [f64; 2],
}

impl Player {
    fn new(
        session: NativeViewerSession,
        window: &Arc<Window>,
        proxy: EventLoopProxy<UiEvent>,
    ) -> Result<Self> {
        session.ensure_running()?;
        let owner = u64::from(window.id());
        let wake = FrameWakeBridge::install(&session, proxy, window.id())?;
        Ok(Self {
            session,
            wake,
            owner,
            stream_control_ui: StreamControlUi::default(),
            performance_mode: PerformancePanelMode::Hidden,
            intercept_shortcuts: true,
            current: None,
            video_size: None,
            video_rect: None,
            pointer_in_video: false,
            last_position: None,
            modifiers: winit::keyboard::ModifiersState::empty(),
            held_keys: Vec::new(),
            physical_keys: Vec::new(),
            physical_buttons: 0,
            scale: window.scale_factor() as f32,
            control_error: None,
            cursor: None,
            pointer_locked: false,
            motion_remainder: [0.0, 0.0],
        })
    }

    fn control(&self) -> &crate::stream_control::StreamControlHandle {
        &self.session.stream_control
    }

    fn input(&self) -> &crate::remote_input::RemoteInput {
        self.session.stream_control.mouse()
    }

    fn poll(&mut self) -> Result<()> {
        self.confirm_neutral();
        self.session.ensure_running()
    }

    /// Take the newest decoded frame, dropping anything the UI cadence skipped.
    fn take_frame(&mut self) -> Option<DecodedVideoFrame> {
        let mut queue = mutex_lock(&self.session.frame_queue);
        let mut frame = take_next_frame(&mut queue, &self.session.performance)?;
        while let Some(newer) = take_next_frame(&mut queue, &self.session.performance) {
            self.session.performance.record_dropped_present_frame();
            frame = newer;
        }
        Some(frame)
    }

    fn publish(&mut self, frame: &DecodedVideoFrame) {
        self.video_size = Some((frame.width, frame.height, frame.rotation));
        self.session
            .performance
            .record_rendered_frame(crate::performance::RenderedFrameTiming {
                is_new_picture: frame.is_new_picture,
                width: frame.width,
                height: frame.height,
                decoded_at: frame.decoded_at,
                local: frame.received_at.elapsed(),
                assembly: frame.assembly_delay,
                input_queue: frame.input_queue_delay,
                decode_pipeline: frame.decode_pipeline_delay,
                surface_transfer: Duration::ZERO,
                present_wait: Duration::ZERO,
                render_queue: frame.decoded_at.elapsed(),
                sender_capture_at: frame.sender_timing.capture_at,
                sender_capture: frame.sender_timing.capture_delay,
                sender_encode: frame.sender_timing.encode_delay,
                sender_pacer: frame.sender_timing.pacer_delay,
                sender_total: frame.sender_timing.sending_delay,
                transport: frame.sender_timing.transport_delay,
            });
    }

    /// Rotation swaps the aspect the video should be fitted to.
    fn display_size(&self) -> Option<(u32, u32)> {
        let (width, height, rotation) = self.video_size?;
        Some(if matches!(rotation % 360, 90 | 270) {
            (height, width)
        } else {
            (width, height)
        })
    }

    fn draw(&mut self, ui: &mut egui::Ui, window: &Arc<Window>) -> Option<VideoPlacement> {
        // The title bar is a panel above this ui; max_rect would include it and
        // push the first rows of the picture underneath it.
        let content = ui.available_rect_before_wrap();
        self.video_rect = None;
        let scale = ui.ctx().pixels_per_point();
        self.scale = scale;
        let placement = self.display_size().map(|(width, height)| {
            let available = content.size();
            let video_aspect = (width as f32).max(1.0) / (height as f32).max(1.0);
            let area_aspect = available.x / available.y.max(1.0);
            let size = if video_aspect > area_aspect {
                egui::vec2(available.x, available.x / video_aspect)
            } else {
                egui::vec2(available.y * video_aspect, available.y)
            };
            let rect = egui::Rect::from_center_size(content.center(), size);
            self.video_rect = Some(rect);
            VideoPlacement {
                x: rect.min.x * scale,
                y: rect.min.y * scale,
                width: rect.width() * scale,
                height: rect.height() * scale,
                rotation: self.video_size.map_or(0, |size| size.2),
            }
        });
        if placement.is_none() {
            ui.centered_and_justified(|ui| {
                ui.label(
                    egui::RichText::new("正在等待画面…")
                        .color(crate::ui::theme::MUTED)
                        .size(crate::ui::theme::BODY),
                );
            });
        }
        let snapshot = self.session.performance.snapshot();
        let mut view = super::stream_menu::LocalViewSettings {
            performance_mode: self.performance_mode,
            intercept_shortcuts: self.intercept_shortcuts,
            send_ctrl_alt_del: false,
        };
        let display_size = self.display_size();
        let screen_id = self.session.screen_id();
        show_stream_control_window(
            ui.ctx(),
            &self.session.stream_control,
            &mut self.stream_control_ui,
            &mut view,
            screen_id,
            display_size,
        );
        self.performance_mode = view.performance_mode;
        self.intercept_shortcuts = view.intercept_shortcuts;
        if view.send_ctrl_alt_del {
            let _ = self
                .session
                .stream_control
                .mouse()
                .send_ctrl_alt_del(self.owner);
        }
        if self.performance_mode != PerformancePanelMode::Hidden {
            super::performance_panel::show(
                ui.ctx(),
                &self.session.performance,
                &snapshot,
                "linux-player",
            );
        }
        self.update_pointer_lock(window);
        self.draw_remote_cursor(ui, window);
        placement
    }

    /// Paint the remote pointer over the video and hide the local one.
    ///
    /// In absolute mode the local pointer already stands for the remote one, so
    /// the shape is drawn where the local pointer is rather than at the position
    /// the remote sampled with it.
    fn draw_remote_cursor(&mut self, ui: &mut egui::Ui, window: &Arc<Window>) {
        if self.pointer_locked {
            // The remote draws its own pointer into the video in this mode.
            return;
        }
        let controlling = self.session.stream_control.mouse().mode() != MouseMode::View;
        let hidden = self.session.stream_control.remote_cursor_hidden();
        let pointer = ui.ctx().pointer_latest_pos();
        let Some((rect, position)) = self.video_rect.zip(pointer) else {
            window.set_cursor_visible(true);
            return;
        };
        if !controlling || !rect.contains(position) {
            window.set_cursor_visible(true);
            return;
        }
        window.set_cursor_visible(false);
        if hidden {
            return;
        }
        let Some(cursor) = self.session.stream_control.remote_cursor() else {
            return;
        };
        let identity = Arc::as_ptr(&cursor.image) as usize;
        if self.cursor.as_ref().is_none_or(|(id, ..)| *id != identity) {
            match decode_cursor(ui.ctx(), &cursor.image) {
                Some(entry) => self.cursor = Some((identity, entry.0, entry.1, entry.2)),
                None => return,
            }
        }
        let Some((_, texture, size, hotspot)) = &self.cursor else {
            return;
        };
        // The cursor is authored in remote pixels; scale it with the video.
        let scale = self
            .session
            .stream_control
            .mouse_screen(self.session.track_index)
            .map_or(1.0, |(_, remote_width, _)| {
                rect.width() / (remote_width.max(1) as f32)
            });
        let size = egui::vec2(size[0] as f32 * scale, size[1] as f32 * scale);
        let origin = position - egui::vec2(hotspot[0] as f32 * scale, hotspot[1] as f32 * scale);
        egui::Image::from_texture(egui::load::SizedTexture::new(texture.id(), size))
            .paint_at(ui, egui::Rect::from_min_size(origin, size));
    }

    /// The player commands, placed in the window caption between the title and
    /// the window buttons. The Windows player has the same row.
    pub(super) fn title_bar_controls(
        &mut self,
        ui: &mut egui::Ui,
        bar: egui::Rect,
        window: &Arc<Window>,
    ) {
        // Leave the logo and alias on the left and the window buttons on the right.
        let left = bar.left() + TITLE_CAPTION_WIDTH;
        let right = bar.right() - crate::ui::theme::WINDOW_CONTROLS_WIDTH - 4.0;
        if right - left < 120.0 {
            return;
        }
        let rect = egui::Rect::from_min_max(
            egui::pos2(left, bar.top()),
            egui::pos2(
                right,
                bar.top() + crate::ui::theme::WINDOW_TITLE_CONTENT_HEIGHT,
            ),
        );
        let mut row = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        row.set_clip_rect(rect);
        row.spacing_mut().item_spacing.x = 6.0;
        let controlling = self.input().mode() != MouseMode::View;
        if row.button("全屏").clicked() {
            toggle_fullscreen(window);
        }
        if row.button("性能").clicked() {
            self.performance_mode = self.performance_mode.next();
        }
        if row.button("串流设置").clicked() {
            self.stream_control_ui.open = !self.stream_control_ui.open;
        }
        if row
            .selectable_label(controlling, "键鼠控制")
            .on_hover_text(format!(
                "退出控制：{}",
                crate::viewer_shortcuts::label(ViewerShortcut::ReleaseMouse)
            ))
            .clicked()
        {
            self.set_control(!controlling);
        }
        if let Some(error) = &self.control_error {
            row.add(
                egui::Label::new(
                    egui::RichText::new(error)
                        .size(crate::ui::theme::SMALL)
                        .color(crate::ui::theme::DANGER_FILL),
                )
                .truncate(),
            );
        }
    }

    fn set_control(&mut self, enabled: bool) {
        let mode = if enabled {
            MouseMode::Smart
        } else {
            MouseMode::View
        };
        if let Err(error) = self.control().set_mouse_mode(mode) {
            self.control_error = Some(format!("{error:#}"));
        }
        if !enabled {
            self.release_keys();
        }
    }

    fn release_pointer(&mut self, window: &Arc<Window>) {
        if self.pointer_locked {
            let _ = window.set_cursor_grab(winit::window::CursorGrabMode::None);
            window.set_cursor_visible(true);
            self.pointer_locked = false;
        }
    }

    /// Raw pointer motion, used while the remote drives its own cursor.
    fn on_device_event(&mut self, event: &DeviceEvent) -> bool {
        let DeviceEvent::MouseMotion { delta } = event else {
            return false;
        };
        {
            let input = self.input();
            if !input.relative_mode()
                || input.mode() == MouseMode::View
                || input.waiting_for_neutral()
            {
                return false;
            }
        }
        // Whole pixels go out now; the fraction waits for the next event so slow
        // movement is not rounded away.
        let x = delta.0 + self.motion_remainder[0];
        let y = delta.1 + self.motion_remainder[1];
        self.motion_remainder = [x.fract(), y.fract()];
        let (x, y) = (x.trunc(), y.trunc());
        if x == 0.0 && y == 0.0 {
            return false;
        }
        self.input().relative(self.owner, x as i32, y as i32);
        true
    }

    /// Keep the pointer inside the window while the remote owns it, so the
    /// local cursor cannot wander onto another window mid-game.
    fn update_pointer_lock(&mut self, window: &Arc<Window>) {
        let input = self.input();
        let wanted = input.relative_mode() && input.mode() != MouseMode::View;
        if wanted == self.pointer_locked {
            return;
        }
        if wanted {
            // X11 only confines; Wayland and Windows can lock in place.
            let locked = window
                .set_cursor_grab(winit::window::CursorGrabMode::Locked)
                .or_else(|_| window.set_cursor_grab(winit::window::CursorGrabMode::Confined));
            match locked {
                Ok(()) => {
                    window.set_cursor_visible(false);
                    self.pointer_locked = true;
                }
                Err(error) => {
                    tracing::warn!(%error, "无法锁定指针，相对模式可能不可用");
                    self.control_error = Some("当前桌面不允许锁定指针".to_owned());
                }
            }
        } else {
            let _ = window.set_cursor_grab(winit::window::CursorGrabMode::None);
            window.set_cursor_visible(true);
            self.pointer_locked = false;
            self.motion_remainder = [0.0, 0.0];
        }
    }

    /// The remote refuses input until every key and button held when control
    /// was granted has been released; nothing else reports that on Linux.
    fn confirm_neutral(&self) {
        let input = self.input();
        if !input.waiting_for_neutral() {
            return;
        }
        if self.physical_keys.is_empty() && self.physical_buttons == 0 {
            input.confirm_neutral(input.activation_generation());
        }
    }

    fn release_keys(&mut self) {
        for key in std::mem::take(&mut self.held_keys) {
            self.input().key(self.owner, key, false, None);
        }
    }

    fn on_focus_changed(&mut self, window: &Arc<Window>, focused: bool) {
        if !focused {
            self.release_pointer(window);
            // A key released while another window has focus never reaches us.
            self.release_keys();
            self.physical_keys.clear();
            self.physical_buttons = 0;
            self.modifiers = winit::keyboard::ModifiersState::empty();
            self.input().pause_owner(self.owner);
        }
    }

    /// Returns true when the event was consumed by remote input.
    fn on_window_event(
        &mut self,
        window: &Arc<Window>,
        context: &egui::Context,
        event: &WindowEvent,
        consumed_by_ui: bool,
    ) -> bool {
        match event {
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
                false
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.repeat {
                    return false;
                }
                let down = event.state == ElementState::Pressed;
                if let Some(key) = crate::viewer_shortcuts::physical_key(event.physical_key) {
                    if down {
                        if !self.physical_keys.contains(&key) {
                            self.physical_keys.push(key);
                        }
                    } else {
                        self.physical_keys.retain(|held| *held != key);
                    }
                    self.confirm_neutral();
                }
                if down && let Some(action) = viewer_shortcut(self.modifiers, event.physical_key) {
                    self.run_shortcut(action, window);
                    return true;
                }
                if consumed_by_ui || self.input().mode() == MouseMode::View {
                    return false;
                }
                let Some(key) = crate::viewer_shortcuts::physical_key(event.physical_key) else {
                    return false;
                };
                if down {
                    if !self.held_keys.contains(&key) {
                        self.held_keys.push(key);
                    }
                } else {
                    self.held_keys.retain(|held| *held != key);
                }
                self.input().key(self.owner, key, down, None)
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.last_position = Some(*position);
                self.pointer_in_video = self.hit_video(context, *position);
                self.send_position(*position);
                false
            }
            WindowEvent::CursorLeft { .. } => {
                self.pointer_in_video = false;
                self.last_position = None;
                false
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(bit) = mouse_button(*button).map(button_bit) {
                    if *state == ElementState::Pressed {
                        self.physical_buttons |= bit;
                    } else {
                        self.physical_buttons &= !bit;
                    }
                    self.confirm_neutral();
                }
                if consumed_by_ui || self.input().mode() == MouseMode::View {
                    return false;
                }
                if !self.pointer_in_video && *state == ElementState::Pressed {
                    return false;
                }
                let Some(code) = mouse_button(*button) else {
                    return false;
                };
                self.input()
                    .button(self.owner, code, *state == ElementState::Pressed);
                true
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if consumed_by_ui || !self.pointer_in_video {
                    return false;
                }
                let (horizontal, amount) = match delta {
                    // One notch is 120 units on the wire, as Windows reports it.
                    MouseScrollDelta::LineDelta(x, y) => {
                        if x.abs() > y.abs() {
                            (true, *x * 120.0)
                        } else {
                            (false, *y * 120.0)
                        }
                    }
                    MouseScrollDelta::PixelDelta(position) => {
                        if position.x.abs() > position.y.abs() {
                            (true, position.x as f32 * 2.0)
                        } else {
                            (false, position.y as f32 * 2.0)
                        }
                    }
                };
                let amount = amount.round() as i32;
                if amount == 0 {
                    return false;
                }
                self.input().wheel(self.owner, amount, horizontal);
                true
            }
            _ => false,
        }
    }

    fn run_shortcut(&mut self, action: ViewerShortcut, window: &Arc<Window>) {
        match action {
            ViewerShortcut::ReleaseMouse => self.set_control(false),
            ViewerShortcut::Fullscreen => toggle_fullscreen(window),
            ViewerShortcut::Close => {
                self.release_keys();
                window.set_visible(false);
                self.session.close_handle().close();
            }
            ViewerShortcut::Performance => {
                self.performance_mode = self.performance_mode.next();
            }
        }
    }

    fn hit_video(&self, context: &egui::Context, position: PhysicalPosition<f64>) -> bool {
        let Some(rect) = self.video_rect else {
            return false;
        };
        let scale = f64::from(context.pixels_per_point());
        let point = egui::pos2((position.x / scale) as f32, (position.y / scale) as f32);
        rect.contains(point)
            && context
                .layer_id_at(point)
                .is_none_or(|layer| layer.order == egui::Order::Background)
    }

    fn send_position(&mut self, position: PhysicalPosition<f64>) {
        let input = self.input();
        if input.mode() == MouseMode::View || input.waiting_for_neutral() {
            return;
        }
        let dragging = input.owner_holds_buttons(self.owner);
        if !dragging && !self.pointer_in_video {
            return;
        }
        let Some(rect) = self.video_rect else {
            return;
        };
        let Some((screen, remote_width, remote_height)) =
            self.control().mouse_screen(self.session.track_index)
        else {
            return;
        };
        let local_x = (position.x / f64::from(self.scale_factor())) - f64::from(rect.min.x);
        let local_y = (position.y / f64::from(self.scale_factor())) - f64::from(rect.min.y);
        let width = f64::from(rect.width());
        let height = f64::from(rect.height());
        if width <= 0.0 || height <= 0.0 {
            return;
        }
        // The host multiplies by the screen size and rounds, so the last
        // physical pixel must stay inside the screen.
        let x = (local_x.clamp(0.0, width - 1.0) / width)
            .min(1.0 - 1.0 / f64::from(remote_width.max(1)));
        let y = (local_y.clamp(0.0, height - 1.0) / height)
            .min(1.0 - 1.0 / f64::from(remote_height.max(1)));
        input.absolute(self.owner, screen, x, y);
    }

    fn scale_factor(&self) -> f32 {
        self.scale.max(0.1)
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.release_keys();
        self.input().disable();
    }
}

/// The decoder drops every decoded frame unless a render thread is registered
/// with the session, which is how the Windows player signals that its window is
/// showing video. Here the frames are drawn by the UI thread, so this thread
/// holds that registration and turns each decoded frame into a repaint request.
struct FrameWakeBridge {
    wake: super::FrameWake,
    running: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FrameWakeBridge {
    fn install(
        session: &NativeViewerSession,
        proxy: EventLoopProxy<UiEvent>,
        window: WindowId,
    ) -> Result<Self> {
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let worker_running = Arc::clone(&running);
        let thread = std::thread::Builder::new()
            .name("Video wake".to_owned())
            .spawn(move || {
                while worker_running.load(std::sync::atomic::Ordering::Acquire) {
                    std::thread::park();
                    if !worker_running.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    let _ = proxy.send_event(UiEvent::Repaint(UiRepaintEvent {
                        window,
                        generation: 0,
                        pass: 0,
                        when: Instant::now(),
                    }));
                }
            })
            .context("create video wake thread")?;
        let handle = thread.thread().clone();
        session.frame_wake.install_render_thread(handle.clone());
        handle.unpark();
        Ok(Self {
            wake: session.frame_wake.clone(),
            running,
            thread: Some(thread),
        })
    }
}

impl Drop for FrameWakeBridge {
    fn drop(&mut self) {
        // Stop the decoder queueing frames nothing will draw, then release the
        // registration before the thread goes away.
        self.wake
            .visible
            .store(false, std::sync::atomic::Ordering::Release);
        *mutex_lock(&self.wake.render_thread) = None;
        self.running
            .store(false, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

fn toggle_fullscreen(window: &Arc<Window>) {
    if window.fullscreen().is_some() {
        window.set_fullscreen(None);
    } else {
        window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
    }
}

/// The wire codes `RemoteInput` accepts, which are not the 1..=6 numbering the
/// plugin hotkeys use.
fn mouse_button(button: MouseButton) -> Option<u32> {
    Some(match button {
        MouseButton::Left => 1,
        MouseButton::Right => 2,
        MouseButton::Middle => 16,
        MouseButton::Back => 32,
        MouseButton::Forward => 64,
        MouseButton::Other(_) => return None,
    })
}

/// One bit per wire code, for tracking what is physically held.
fn button_bit(code: u32) -> u8 {
    match code {
        1 => 1,
        2 => 2,
        16 => 4,
        32 => 8,
        64 => 16,
        _ => 0,
    }
}

fn viewer_shortcut(
    modifiers: winit::keyboard::ModifiersState,
    key: winit::keyboard::PhysicalKey,
) -> Option<ViewerShortcut> {
    if crate::viewer_shortcuts::suspended() {
        return None;
    }
    crate::viewer_shortcuts::match_key(
        crate::viewer_shortcuts::physical_key(key)?,
        crate::viewer_shortcuts::modifiers(modifiers),
    )
}

/// Decode the remote cursor PNG into a texture, with its size and hotspot.
fn decode_cursor(
    ctx: &egui::Context,
    image: &crate::remote_cursor::CursorImage,
) -> Option<(egui::TextureHandle, [u32; 2], [u32; 2])> {
    if image.png.is_empty() {
        return None;
    }
    let decoded = image::load_from_memory_with_format(&image.png, image::ImageFormat::Png)
        .map_err(|error| tracing::debug!(%error, "远端光标图像无法解码"))
        .ok()?
        .to_rgba8();
    let size = [decoded.width() as usize, decoded.height() as usize];
    let texture = ctx.load_texture(
        "remote-cursor",
        egui::ColorImage::from_rgba_unmultiplied(size, decoded.as_raw()),
        egui::TextureOptions::LINEAR,
    );
    Some((texture, [decoded.width(), decoded.height()], image.hotspot))
}
