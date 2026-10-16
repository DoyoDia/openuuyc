//! Capture through the XDG ScreenCast portal, Sunshine's `portalgrab` path:
//! the desktop's own compositor hands over each shared monitor as a PipeWire
//! stream. It is the sanctioned way to capture a Wayland session, and works
//! on X11 desktops whose portal backend offers it (GNOME, KDE).
//!
//! The first session asks the local user which monitors to share. Sharing is
//! requested as persistent, and the restore token the portal returns is kept,
//! so later sessions (including after a restart) start without asking again
//! until the user revokes it.
//!
//! One portal session serves every captured monitor of this process, and its
//! cursor mode is fixed when it starts; a desktop wanting the other mode gets
//! its own session.
use super::Picture;
use super::Screen;
use super::pipewire::Stream;
use anyhow::{Context, Result, bail};
use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{
    CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
    StartCastOptions,
};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

/// One shared monitor as the portal described it.
#[derive(Clone, Debug)]
struct Source {
    node: u32,
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
}

struct Session {
    runtime: tokio::runtime::Runtime,
    proxy: Screencast,
    session: ashpd::desktop::Session<Screencast>,
    sources: Vec<Source>,
    cursor: bool,
}

/// Run portal I/O on a thread of its own, so it never blocks inside (or
/// panics because of) whatever runtime the caller may be part of.
fn on_portal<T: Send>(
    runtime: &tokio::runtime::Runtime,
    work: impl std::future::Future<Output = T> + Send,
) -> T {
    std::thread::scope(|scope| {
        scope
            .spawn(|| runtime.block_on(work))
            .join()
            .expect("portal thread panicked")
    })
}

fn token_path() -> Result<std::path::PathBuf> {
    Ok(crate::platform::paths::require_local_app_data()?.join("OpenUUYC/screencast-restore-token"))
}

/// Whether the user has granted a session that the portal can restore.
pub(super) fn remembered() -> bool {
    token_path()
        .and_then(|path| Ok(std::fs::read_to_string(path)?))
        .is_ok_and(|token| !token.trim().is_empty())
}

impl Session {
    fn start(cursor: bool) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("screencast-portal")
            .enable_all()
            .build()?;
        let token = std::fs::read_to_string(token_path()?)
            .ok()
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty());
        let started = on_portal(&runtime, async {
            let proxy = Screencast::new()
                .await
                .context("桌面没有提供 ScreenCast 门户")?;
            let session = proxy.create_session(Default::default()).await?;
            proxy
                .select_sources(
                    &session,
                    SelectSourcesOptions::default()
                        .set_sources(Some(SourceType::Monitor.into()))
                        .set_multiple(true)
                        .set_cursor_mode(if cursor {
                            CursorMode::Embedded
                        } else {
                            CursorMode::Hidden
                        })
                        .set_persist_mode(PersistMode::ExplicitlyRevoked)
                        .set_restore_token(token.as_deref()),
                )
                .await?
                .response()?;
            // Without a usable restore token this shows the sharing dialog on
            // this machine and waits for the local user.
            let streams = proxy
                .start(&session, None, StartCastOptions::default())
                .await?
                .response()
                .context("本机用户没有同意屏幕共享")?;
            let sources = streams
                .streams()
                .iter()
                .map(|stream| Source {
                    node: stream.pipe_wire_node_id(),
                    position: stream.position(),
                    size: stream.size(),
                })
                .collect::<Vec<_>>();
            let restore = streams.restore_token().map(str::to_owned);
            anyhow::Ok((proxy, session, sources, restore))
        })?;
        let (proxy, session, sources, restore) = started;
        if let Some(restore) = restore {
            let path = token_path()?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, restore)?;
        }
        if sources.is_empty() {
            bail!("屏幕共享没有包含任何显示器");
        }
        tracing::info!(?sources, cursor, "ScreenCast portal session started");
        Ok(Self {
            runtime,
            proxy,
            session,
            sources,
            cursor,
        })
    }

    /// The shared session with this cursor mode, started if none is alive.
    fn shared(cursor: bool) -> Result<Arc<Self>> {
        static SESSIONS: Mutex<Vec<Weak<Session>>> = Mutex::new(Vec::new());
        // Held across start: a restore token is single-use, so two sessions
        // starting together would leave one of them asking the user again.
        let mut sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        sessions.retain(|session| session.strong_count() > 0);
        if let Some(session) = sessions
            .iter()
            .filter_map(Weak::upgrade)
            .find(|session| session.cursor == cursor)
        {
            return Ok(session);
        }
        let session = Arc::new(Self::start(cursor)?);
        sessions.push(Arc::downgrade(&session));
        Ok(session)
    }

    /// The shared monitor that is `screen`, matched by the rectangle it covers
    /// on the desktop.
    fn source(&self, screen: &Screen) -> Result<&Source> {
        let rectangle = (
            (screen.left, screen.top),
            (screen.width as i32, screen.height as i32),
        );
        if let Some(source) = self
            .sources
            .iter()
            .find(|s| s.position == Some(rectangle.0) && s.size == Some(rectangle.1))
        {
            return Ok(source);
        }
        // A desktop scaled by anything but 1 leaves that comparison without an
        // answer: the portal gives the monitor as the compositor lays it out,
        // while `Screen` carries the size XWayland gives the output, which is
        // the layout size times the output's own scale. The two rectangles sit
        // at the same place all the same, and the scale cancels out of their
        // shapes, so the monitor is found by the shape it shares with the
        // portal rectangle at that position. Without this a scaled desktop has
        // no working capture method at all on a compositor without
        // wlroots screencopy, such as Mutter or KWin.
        if let Some(source) = self.sources.iter().find(|s| {
            s.position == Some(rectangle.0)
                && s.size.is_some_and(|size| same_shape(size, rectangle.1))
        }) {
            return Ok(source);
        }
        match self.sources.as_slice() {
            // A single shared monitor without geometry can only be this one.
            [only] if only.position.is_none() => Ok(only),
            _ => bail!("所选显示器不在本机授权共享的屏幕中"),
        }
    }
}

/// How far the shapes of a portal rectangle and a screen rectangle may drift
/// apart and still be one monitor. Rounding the scaled size to whole layout
/// units moves the ratio by about one part in a thousand on a 4K monitor and
/// by under a percent on the smallest one, while the shapes of any two
/// monitors a desktop holds are at least several percent apart.
const SHAPE_TOLERANCE: f64 = 0.01;

/// Whether the rectangle a portal stream covers and the rectangle `Screen`
/// carries are the same monitor.
///
/// They differ by the output's scale — the portal counts layout units, `Screen`
/// counts the output's own pixels — and a scale is the same in both directions,
/// so it cancels out of the aspect ratio. A monitor turned a quarter turn is
/// described with its sides swapped by one of the two, so the ratio is also
/// tried the other way round.
fn same_shape(logical: (i32, i32), pixels: (i32, i32)) -> bool {
    let (width, height) = (f64::from(logical.0), f64::from(logical.1));
    let (pixel_width, pixel_height) = (f64::from(pixels.0), f64::from(pixels.1));
    // A source the portal left empty describes nothing to compare against.
    if width <= 0.0 || height <= 0.0 || pixel_width <= 0.0 || pixel_height <= 0.0 {
        return false;
    }
    let ratio = width / height;
    let pixel_ratio = pixel_width / pixel_height;
    let close = |other: f64| (ratio - other).abs() <= ratio * SHAPE_TOLERANCE;
    close(pixel_ratio) || close(pixel_ratio.recip())
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = on_portal(&self.runtime, self.session.close());
        let _ = &self.proxy;
    }
}

pub(super) struct Grab {
    stream: Stream,
    session: Arc<Session>,
}

impl Grab {
    pub fn open(screen: &Screen, cursor: bool) -> Result<Self> {
        let session = Session::shared(cursor)?;
        let node = session.source(screen)?.node;
        let fd = on_portal(
            &session.runtime,
            session
                .proxy
                .open_pipe_wire_remote(&session.session, OpenPipeWireRemoteOptions::default()),
        )
        .context("无法打开屏幕共享的 PipeWire 连接")?;
        let stream = Stream::connect(fd, node)?;
        Ok(Self { stream, session })
    }

    pub fn name(&self) -> &'static str {
        "XDG 门户 · PipeWire"
    }

    /// Whether the stream draws the cursor into its pictures.
    pub fn cursor(&self) -> bool {
        self.session.cursor
    }

    /// Output geometry request for the GPU converter: `(quality, maximum)`.
    pub fn set_target(&self, target: Option<(i32, (u32, u32))>) {
        self.stream.set_target(target);
    }

    pub fn next(&mut self, timeout: Duration) -> Result<Option<Picture>> {
        self.stream.next(timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::same_shape;

    /// The desktop this went wrong on: an AMD output of 5120x2160 at scale
    /// 1.35, which the portal describes as 3792x1600 layout units.
    #[test]
    fn a_fractionally_scaled_monitor_matches_by_its_shape() {
        assert!(same_shape((3792, 1600), (5120, 2160)));
    }

    /// A desktop at scale 1 is the same rectangle in both coordinate spaces.
    #[test]
    fn an_unscaled_monitor_matches_to_the_pixel() {
        assert!(same_shape((1920, 1080), (1920, 1080)));
    }

    /// A monitor turned a quarter turn is described with its sides swapped by
    /// one of the two coordinate spaces.
    #[test]
    fn a_rotated_monitor_matches_with_its_sides_the_other_way_round() {
        assert!(same_shape((1080, 1920), (1920, 1080)));
        assert!(same_shape((1200, 1920), (1920, 1200)));
    }

    /// 4:3 and 5:4 are the closest two monitors get, and they stay apart.
    #[test]
    fn monitors_of_other_shapes_do_not_match() {
        assert!(!same_shape((1280, 960), (1280, 1024)));
        assert!(!same_shape((1920, 1080), (2560, 1600)));
        assert!(!same_shape((5120, 2160), (2560, 1600)));
    }

    /// An empty rectangle is not a shape, and never the monitor asked for.
    #[test]
    fn an_empty_rectangle_does_not_match() {
        assert!(!same_shape((0, 0), (0, 0)));
        assert!(!same_shape((1920, 1080), (0, 0)));
        assert!(!same_shape((0, 0), (1920, 1080)));
    }
}
