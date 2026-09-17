//! Linux clipboard adapter for X11 and Wayland.
//!
//! Differences from the Windows/OLE adapter this replaces:
//! * X11 and Wayland have no delayed rendering across processes, so an offer
//!   from the remote is fetched immediately instead of when the user pastes.
//! * There is no clipboard-change notification, so local changes are polled.
//! * Files are not offered: serving them needs delayed rendering, and every
//!   session therefore reports file support as unavailable.
use super::formats::{self, Format};
use super::*;
use anyhow::Context as _;
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

pub(super) use super::formats::safe_name;

/// Anything larger is a transfer, not a clipboard paste.
const MAX_CLIP: usize = 32 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(400);

pub(super) enum Command {
    Activate(Weak<Inner>),
    Remove(u64),
    Offer(Weak<Inner>, u64, Vec<ClipboardFormat>),
    Request(Weak<Inner>, u64, i64, ClipboardRequestKind),
    Text(Weak<Inner>, u64, i64, String),
}

struct Worker {
    sender: SyncSender<Command>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

static WORKER: OnceLock<std::result::Result<Worker, String>> = OnceLock::new();

/// What this client currently owns locally, as the remote would see it.
#[derive(Default)]
struct LocalSnapshot {
    text: Option<String>,
    image: Option<Vec<u8>>,
}

impl LocalSnapshot {
    fn is_empty(&self) -> bool {
        self.text.is_none() && self.image.is_none()
    }

    /// CF_UNICODETEXT and CF_DIB are what a Windows peer understands.
    fn format_ids(&self) -> Vec<(u32, String)> {
        let mut ids = Vec::new();
        if self.text.is_some() {
            ids.push((13, String::new()));
            ids.push((1, String::new()));
        }
        if self.image.is_some() {
            ids.push((8, String::new()));
        }
        ids
    }

    fn data(&self, format: &Format) -> Result<Vec<u8>> {
        match format.local {
            13 => self
                .text
                .as_deref()
                .map(formats::unicode)
                .context("本地剪贴板没有文本"),
            1 => self
                .text
                .as_deref()
                .map(|text| {
                    let mut bytes = text.replace('\n', "\r\n").into_bytes();
                    bytes.push(0);
                    bytes
                })
                .context("本地剪贴板没有文本"),
            8 => self.image.clone().context("本地剪贴板没有图片"),
            _ => bail!("不支持的本地剪贴板格式"),
        }
    }
}

struct State {
    receiver: Receiver<Command>,
    clipboard: Option<arboard::Clipboard>,
    sessions: HashMap<u64, Weak<Inner>>,
    published: HashMap<u64, Vec<Format>>,
    local: LocalSnapshot,
    /// Set while this adapter writes, so the poll does not report its own write.
    writing: bool,
}

pub(super) fn start() -> Result<()> {
    WORKER
        .get_or_init(|| {
            let (sender, receiver) = sync_channel(64);
            let thread = std::thread::Builder::new()
                .name("UU clipboard".into())
                .spawn(move || run(receiver))
                .map_err(|error| format!("启动剪贴板线程失败：{error}"))?;
            Ok(Worker {
                sender,
                thread: Mutex::new(Some(thread)),
            })
        })
        .as_ref()
        .map(|_| ())
        .map_err(|error| anyhow!(error.clone()))
}

pub(super) fn post(command: Command) -> Result<()> {
    start()?;
    let worker = WORKER
        .get()
        .and_then(|worker| worker.as_ref().ok())
        .context("剪贴板线程不可用")?;
    worker
        .sender
        .try_send(command)
        .map_err(|_| anyhow!("剪贴板队列已满或已关闭"))
}

/// The Windows adapter pumps its STA here. This worker is a plain thread, so
/// callers waiting on a response simply keep waiting on their condvar.
pub(super) fn pump() {}

pub(super) fn shutdown() {
    let Some(Ok(worker)) = WORKER.get() else {
        return;
    };
    // Dropping every sender ends the receive loop; the clone here is the last one.
    let thread = lock(&worker.thread).take();
    if let Some(thread) = thread {
        let _ = worker.sender.try_send(Command::Remove(u64::MAX));
        drop(thread);
    }
}

fn run(receiver: Receiver<Command>) {
    let clipboard = match arboard::Clipboard::new() {
        Ok(clipboard) => Some(clipboard),
        Err(error) => {
            tracing::warn!(%error, "系统剪贴板不可用，剪贴板同步将保持关闭");
            None
        }
    };
    let mut state = State {
        receiver,
        clipboard,
        sessions: HashMap::new(),
        published: HashMap::new(),
        local: LocalSnapshot::default(),
        writing: false,
    };
    loop {
        match state.receiver.recv_timeout(POLL) {
            Ok(command) => {
                if matches!(command, Command::Remove(u64::MAX)) {
                    break;
                }
                process(&mut state, command);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Err(error) = poll_local(&mut state) {
                    tracing::debug!(%error, "读取本地剪贴板失败");
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn sessions(state: &State) -> Vec<Arc<Inner>> {
    state
        .sessions
        .values()
        .filter_map(std::sync::Weak::upgrade)
        .collect()
}

fn process(state: &mut State, command: Command) {
    match command {
        Command::Activate(weak) => {
            if let Some(session) = weak.upgrade() {
                state.sessions.insert(session.id, Arc::downgrade(&session));
            }
        }
        Command::Remove(id) => {
            state.sessions.remove(&id);
            state.published.remove(&id);
        }
        Command::Offer(weak, epoch, formats) => {
            if let Some(session) = weak.upgrade().filter(|session| session.valid(epoch))
                && let Err(error) = accept_offer(state, &session, epoch, formats)
            {
                session.fail(error.to_string());
            }
        }
        Command::Text(weak, epoch, id, text) => {
            if let Some(session) = weak.upgrade().filter(|session| session.valid(epoch)) {
                let result = if text.is_empty() {
                    Err(anyhow!("空文本"))
                } else {
                    write_text(state, &text)
                };
                if let Err(error) = &result {
                    tracing::debug!(%error, "写入本地剪贴板文本失败");
                }
                let _ = session.emit(
                    epoch,
                    Envelope {
                        request: None,
                        response: Some(Response {
                            header: Some(Header { id }),
                            clip: None,
                            text: Some(ClipboardTextChangeResponse {
                                err: if result.is_ok() { 1 } else { 2 },
                            }),
                        }),
                    },
                );
            }
        }
        Command::Request(weak, epoch, id, kind) => {
            if let Some(session) = weak.upgrade().filter(|session| session.valid(epoch))
                && let Err(error) = serve(state, &session, epoch, id, kind)
            {
                session.fail(error.to_string());
            }
        }
    }
}

/// Fetch the best offered format now, because the local clipboard cannot
/// promise data it does not yet hold.
fn accept_offer(
    state: &mut State,
    session: &Arc<Inner>,
    epoch: u64,
    offered: Vec<ClipboardFormat>,
) -> Result<()> {
    if offered.is_empty() {
        return Ok(());
    }
    let platform = session.platform.load(Ordering::Acquire);
    // Files need delayed rendering, so they are never accepted here.
    let links: Vec<Format> = offered
        .into_iter()
        .filter_map(|format| formats::incoming(format, platform, false))
        .collect();
    let Some(format) = [13u32, 1, 8, 17]
        .into_iter()
        .find_map(|id| links.iter().find(|format| format.local == id))
        .cloned()
    else {
        return Ok(());
    };
    let data = session.data(epoch, &format.wire)?;
    ensure!(data.len() <= MAX_CLIP, "剪贴板内容过大");
    let data = formats::convert(data, &format, platform, false)?;
    match format.local {
        13 => {
            let text = utf16_text(&data)?;
            write_text(state, &text)?;
        }
        1 => {
            let text = String::from_utf8_lossy(&data)
                .trim_end_matches('\0')
                .replace("\r\n", "\n");
            write_text(state, &text)?;
        }
        8 | 17 => write_image(state, &data)?,
        _ => return Ok(()),
    }
    *lock(&session.error) = None;
    Ok(())
}

fn utf16_text(bytes: &[u8]) -> Result<String> {
    ensure!(bytes.len().is_multiple_of(2), "无效的Unicode剪贴板");
    let mut words: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    while words.last() == Some(&0) {
        words.pop();
    }
    if words.first() == Some(&0xfeff) {
        words.remove(0);
    }
    Ok(String::from_utf16(&words)?.replace("\r\n", "\n"))
}

fn write_text(state: &mut State, text: &str) -> Result<()> {
    let clipboard = state.clipboard.as_mut().context("系统剪贴板不可用")?;
    state.writing = true;
    let result = clipboard.set_text(text.to_owned());
    state.writing = false;
    result.context("写入系统剪贴板失败")?;
    state.local = LocalSnapshot {
        text: Some(text.to_owned()),
        image: None,
    };
    Ok(())
}

fn write_image(state: &mut State, dib: &[u8]) -> Result<()> {
    let image = dib_to_rgba(dib)?;
    let clipboard = state.clipboard.as_mut().context("系统剪贴板不可用")?;
    state.writing = true;
    let result = clipboard.set_image(arboard::ImageData {
        width: image.0 as usize,
        height: image.1 as usize,
        bytes: std::borrow::Cow::Owned(image.2),
    });
    state.writing = false;
    result.context("写入系统剪贴板失败")?;
    state.local = LocalSnapshot {
        text: None,
        image: Some(dib.to_vec()),
    };
    Ok(())
}

/// Read the local clipboard and, when it changed, announce the new formats.
fn poll_local(state: &mut State) -> Result<()> {
    if state.writing || state.sessions.is_empty() {
        return Ok(());
    }
    let Some(clipboard) = state.clipboard.as_mut() else {
        return Ok(());
    };
    let text = clipboard.get_text().ok().filter(|text| !text.is_empty());
    let snapshot = if let Some(text) = text {
        LocalSnapshot {
            text: Some(text),
            image: None,
        }
    } else {
        match clipboard.get_image() {
            Ok(image) => LocalSnapshot {
                text: None,
                image: Some(rgba_to_dib(
                    image.width as u32,
                    image.height as u32,
                    &image.bytes,
                )?),
            },
            Err(_) => LocalSnapshot::default(),
        }
    };
    if snapshot.text == state.local.text && snapshot.image == state.local.image {
        return Ok(());
    }
    state.local = snapshot;
    if state.local.is_empty() {
        state.published.clear();
        return Ok(());
    }
    let ids = state.local.format_ids();
    for session in sessions(state) {
        let links = formats::outgoing(&ids, session.platform.load(Ordering::Acquire), false);
        if links.is_empty() {
            state.published.remove(&session.id);
            continue;
        }
        state.published.insert(session.id, links.clone());
        session.emit(
            session.epoch.load(Ordering::Acquire),
            request(
                session.next(),
                ClipboardRequestKind::FormatList(ClipboardFormatListRequest {
                    formats: links.into_iter().map(|format| format.wire).collect(),
                    has_action: 0,
                    drag_drop_action: None,
                }),
            ),
        )?;
        *lock(&session.error) = None;
    }
    Ok(())
}

fn serve(
    state: &mut State,
    session: &Arc<Inner>,
    epoch: u64,
    id: i64,
    kind: ClipboardRequestKind,
) -> Result<()> {
    match kind {
        ClipboardRequestKind::FormatDataAsk(ask) => {
            let result = (|| -> Result<Vec<u8>> {
                let formats = state.published.get(&session.id).context("原剪贴板已失效")?;
                let format = formats
                    .iter()
                    .find(|format| {
                        format.wire.id == ask.format_id
                            && (ask.format_name.is_empty() || ask.format_name == format.wire.name)
                    })
                    .context("未发布该剪贴板格式")?;
                let data = state.local.data(format)?;
                formats::convert(data, format, session.platform.load(Ordering::Acquire), true)
            })();
            match result {
                Ok(data) if !data.is_empty() => {
                    session.enqueue(Outbound::Blocks(epoch, id, ask.block_key, data))
                }
                _ => session.emit(
                    epoch,
                    response(
                        id,
                        ClipboardResponseKind::FormatDataConfirm(ClipboardFormatDataConfirm {
                            err: 2,
                            block_key: ask.block_key,
                            block_count: 0,
                        }),
                    ),
                ),
            }
        }
        // Serving files needs a promise this platform cannot make.
        ClipboardRequestKind::FileDescListRequest(ask) => session.emit(
            epoch,
            response(
                id,
                ClipboardResponseKind::FileDescListResponse(ClipboardFileDescriptorListResponse {
                    task_id: ask.task_id,
                    segment_count: 0,
                    err: 2,
                }),
            ),
        ),
        ClipboardRequestKind::FileContentsRequest(ask) => session.emit(
            epoch,
            response(
                id,
                ClipboardResponseKind::FileContentsResponse(ClipboardFileContentsResponse {
                    task_id: ask.task_id,
                    data: Vec::new(),
                    err: 2,
                    pos_offset: 0,
                    list_index: 0,
                }),
            ),
        ),
        _ => Ok(()),
    }
}

/// A packed device-independent bitmap, as CF_DIB carries it.
fn dib_to_rgba(dib: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    ensure!(dib.len() >= 40, "位图数据过短");
    let header = u32::from_le_bytes(dib[0..4].try_into()?) as usize;
    ensure!(
        (40..=124).contains(&header) && dib.len() > header,
        "不支持的位图头"
    );
    let width = i32::from_le_bytes(dib[4..8].try_into()?);
    let height = i32::from_le_bytes(dib[8..12].try_into()?);
    let depth = u16::from_le_bytes(dib[14..16].try_into()?);
    let compression = u32::from_le_bytes(dib[16..20].try_into()?);
    ensure!(depth == 32 || depth == 24, "仅支持 24/32 位位图");
    // BI_RGB and BI_BITFIELDS with the usual masks share this pixel layout.
    ensure!(compression == 0 || compression == 3, "不支持压缩位图");
    let bottom_up = height > 0;
    let width = u32::try_from(width.abs()).context("位图宽度无效")?;
    let height = u32::try_from(height.abs()).context("位图高度无效")?;
    ensure!(
        width > 0 && height > 0 && width <= 32768 && height <= 32768,
        "位图尺寸无效"
    );
    let bytes = usize::from(depth / 8);
    let stride = ((width as usize * bytes) + 3) & !3;
    let masks = if compression == 3 { 12 } else { 0 };
    let start = header + masks;
    ensure!(
        dib.len() >= start + stride * height as usize,
        "位图数据不完整"
    );
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    for row in 0..height as usize {
        let source = if bottom_up {
            height as usize - 1 - row
        } else {
            row
        };
        let line = &dib[start + source * stride..][..stride];
        for column in 0..width as usize {
            let pixel = &line[column * bytes..][..bytes];
            let target = (row * width as usize + column) * 4;
            rgba[target] = pixel[2];
            rgba[target + 1] = pixel[1];
            rgba[target + 2] = pixel[0];
            rgba[target + 3] = if bytes == 4 { pixel[3] } else { 255 };
        }
    }
    // A 32-bit DIB with an all-zero alpha channel is opaque in practice.
    if bytes == 4 && rgba.iter().skip(3).step_by(4).all(|alpha| *alpha == 0) {
        for alpha in rgba.iter_mut().skip(3).step_by(4) {
            *alpha = 255;
        }
    }
    Ok((width, height, rgba))
}

fn rgba_to_dib(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    ensure!(width > 0 && height > 0, "图片尺寸无效");
    let pixels = width as usize * height as usize;
    ensure!(rgba.len() >= pixels * 4, "图片数据不完整");
    ensure!(pixels * 4 <= MAX_CLIP, "图片过大");
    let mut dib = Vec::with_capacity(40 + pixels * 4);
    dib.extend_from_slice(&40u32.to_le_bytes());
    dib.extend_from_slice(&(width as i32).to_le_bytes());
    // Positive height keeps the bottom-up order Windows applications expect.
    dib.extend_from_slice(&(height as i32).to_le_bytes());
    dib.extend_from_slice(&1u16.to_le_bytes());
    dib.extend_from_slice(&32u16.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes());
    dib.extend_from_slice(&((pixels * 4) as u32).to_le_bytes());
    dib.extend_from_slice(&0i32.to_le_bytes());
    dib.extend_from_slice(&0i32.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes());
    for row in (0..height as usize).rev() {
        for column in 0..width as usize {
            let pixel = &rgba[(row * width as usize + column) * 4..][..4];
            dib.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
    Ok(dib)
}
