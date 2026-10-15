//! Owning the X11 clipboard for what this adapter publishes.
//!
//! 这个会话里 X11 与 Wayland 的选择区是两座互不相通的孤岛（niri 这类合成器不桥接，
//! 实测 `xclip` 写进去的东西 `wl-paste` 看不见，反过来也一样），而 arboard 只落在
//! Wayland 那一侧，所以想让 XWayland 的程序粘得到，就得自己去占 X11 的选择区。
//!
//! 一份复制往往不是一个载荷而是同一件事的好几种说法，读哪一种看程序：GTK 的文件管理
//! 器认 `x-special/gnome-copied-files`（它还带「是复制还是剪切」），别的多半读
//! `text/uri-list`，再老一点的直接读路径文本；文本那边现代程序问 `UTF8_STRING`，只认
//! `STRING` 的老程序也还有。只有选择区的持有者能替一种以上的类型作答，而 arboard 一次
//! 只挂一个名字，所以这里把选择区整个接过来。
//!
//! Ownership passes back the moment anything else claims the clipboard, including
//! this process publishing something else: the owner thread is told, and stops.

use anyhow::{Context as _, Result, bail};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, SelectionRequestEvent, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

/// How long the owner thread waits for an event before looking at whether it
/// has been asked to stop.
const POLL: Duration = Duration::from_millis(50);

/// The targets a file copy offers, most specific first, which is the order
/// `TARGETS` should list them in.
const FILE_TARGETS: [&str; 6] = [
    "x-special/gnome-copied-files",
    "text/uri-list",
    "UTF8_STRING",
    "text/plain;charset=utf-8",
    "STRING",
    "TEXT",
];

/// 文本那几种。GTK 与 Qt 问的是 `UTF8_STRING`（或者带字符集的 `text/plain`），
/// XTerm 和老的 Java 程序还只认 `STRING` / `TEXT`，那两个按 ICCCM 是 ISO-8859-1。
const TEXT_TARGETS: [&str; 5] = [
    "UTF8_STRING",
    "text/plain;charset=utf-8",
    "text/plain",
    "STRING",
    "TEXT",
];

/// 我们放在选择区上的东西。
enum Content {
    Files(Vec<std::path::PathBuf>),
    Text(String),
}

impl Content {
    /// 对外提供的类型，最具体的排前面。
    fn targets(&self) -> &'static [&'static str] {
        match self {
            Self::Files(_) => &FILE_TARGETS,
            Self::Text(_) => &TEXT_TARGETS,
        }
    }

    /// 某个类型上要给的字节。
    fn payload(&self, target: &str) -> Vec<u8> {
        match self {
            Self::Files(paths) => files_payload(target, paths),
            // `STRING` / `TEXT` 是 ISO-8859-1，其余按 UTF-8。落在 Latin-1 之外的
            // 字符换个 `?`：只认这两个类型的程序本来也显示不出它们，而拿 UTF-8 去
            // 冒充 Latin-1 会把整段文字变成乱码。
            Self::Text(text) => match target {
                "STRING" | "TEXT" => text
                    .chars()
                    .map(|c| if (c as u32) <= 0xff { c as u8 } else { b'?' })
                    .collect(),
                _ => text.as_bytes().to_vec(),
            },
        }
    }
}

/// Percent-encode a path for a `file:` URI.
///
/// Everything outside the unreserved set is escaped, which is stricter than
/// what file managers write -- they leave brackets and parentheses alone -- but
/// anything reading a URI has to decode the escapes anyway.
fn encode(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let mut out = String::from("file://");
    for byte in path.as_os_str().as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The bytes each target carries for `paths`.
fn files_payload(target: &str, paths: &[std::path::PathBuf]) -> Vec<u8> {
    let uris: Vec<String> = paths.iter().map(|path| encode(path)).collect();
    match target {
        // The first line is the operation; a paste that reads this one is
        // deciding between copying and moving.
        "x-special/gnome-copied-files" => format!("copy\n{}", uris.join("\n")).into_bytes(),
        // RFC 2483 delimits this with CRLF, including after the last entry.
        "text/uri-list" => uris
            .iter()
            .map(|uri| format!("{uri}\r\n"))
            .collect::<String>()
            .into_bytes(),
        // Plain text gets the paths themselves: a URI pasted into a terminal
        // or a text box is not what anyone wanted.
        _ => paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes(),
    }
}

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    timestamp: Atom,
    /// One per entry in the content's target list, in the same order.
    data: Vec<Atom>,
}

fn intern(connection: &impl Connection, name: &str) -> Result<Atom> {
    Ok(connection
        .intern_atom(false, name.as_bytes())?
        .reply()
        .with_context(|| format!("intern X11 atom {name}"))?
        .atom)
}

/// A copy published on the clipboard, owned until this is dropped or something
/// else takes the selection.
pub(super) struct Offer {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Offer {
    /// Take the clipboard and answer for `paths` until told otherwise.
    pub(super) fn publish_files(paths: Vec<std::path::PathBuf>) -> Result<Self> {
        if paths.is_empty() {
            bail!("没有要发布的文件");
        }
        Self::publish(Content::Files(paths))
    }

    /// 同上，内容是这段文本。
    pub(super) fn publish_text(text: String) -> Result<Self> {
        Self::publish(Content::Text(text))
    }

    fn publish(content: Content) -> Result<Self> {
        let (connection, screen) = x11rb::connect(None).context("连接 X11 显示失败")?;
        let root = connection.setup().roots[screen].root;
        let window = connection.generate_id().context("分配 X11 窗口失败")?;
        connection
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )?
            .check()
            .context("创建 X11 剪贴板窗口失败")?;

        let atoms = Atoms {
            clipboard: intern(&connection, "CLIPBOARD")?,
            targets: intern(&connection, "TARGETS")?,
            timestamp: intern(&connection, "TIMESTAMP")?,
            data: content
                .targets()
                .iter()
                .map(|name| intern(&connection, name))
                .collect::<Result<_>>()?,
        };

        // ICCCM asks for a real timestamp rather than CurrentTime, and the way
        // to get one is to provoke an event that carries it.
        let marker = intern(&connection, "OPENUUYC_TIMESTAMP")?;
        connection.change_property8(PropMode::APPEND, window, marker, AtomEnum::STRING, &[])?;
        connection.flush()?;
        let mut time = x11rb::CURRENT_TIME;
        for _ in 0..100 {
            match connection.poll_for_event()? {
                Some(Event::PropertyNotify(event)) if event.window == window => {
                    time = event.time;
                    break;
                }
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }

        connection
            .set_selection_owner(window, atoms.clipboard, time)?
            .check()
            .context("取得剪贴板所有权失败")?;
        connection.flush()?;
        if connection
            .get_selection_owner(atoms.clipboard)?
            .reply()?
            .owner
            != window
        {
            bail!("剪贴板所有权被其他程序抢占");
        }

        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("UU clipboard owner".into())
            .spawn(move || serve(&connection, window, &atoms, &content, &worker_stop))
            .context("启动剪贴板所有权线程失败")?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Offer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Answer selection requests until ownership is lost or the offer is retired.
fn serve(
    connection: &impl Connection,
    window: x11rb::protocol::xproto::Window,
    atoms: &Atoms,
    content: &Content,
    stop: &AtomicBool,
) {
    while !stop.load(Ordering::Acquire) {
        let event = match connection.poll_for_event() {
            Ok(Some(event)) => event,
            Ok(None) => {
                std::thread::sleep(POLL);
                continue;
            }
            Err(error) => {
                tracing::debug!(%error, "剪贴板所有权连接中断");
                return;
            }
        };
        match event {
            // Something else took the clipboard, which retires this offer.
            Event::SelectionClear(event) if event.owner == window => return,
            Event::SelectionRequest(request) => {
                if let Err(error) = answer(connection, atoms, content, &request) {
                    tracing::debug!(%error, "回应剪贴板请求失败");
                }
            }
            _ => {}
        }
    }
    // 收尾时把手里的选择区交还出去——但只在它真的还是我们的时候。这个 offer 被
    // 换掉（用户又复制了一次，新的 offer 已经抢走了所有权）或者本地用户在别的
    // 程序里复制了东西之后，选择区早就易主；这时再置空就是替别人把剪贴板清掉，
    // 刚上任的那一份也跟着没了。
    let ours = connection
        .get_selection_owner(atoms.clipboard)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .is_some_and(|reply| reply.owner == window);
    if ours {
        let _ = connection.set_selection_owner(x11rb::NONE, atoms.clipboard, x11rb::CURRENT_TIME);
        let _ = connection.flush();
    }
}

fn answer(
    connection: &impl Connection,
    atoms: &Atoms,
    content: &Content,
    request: &SelectionRequestEvent,
) -> Result<()> {
    // A requestor that sent no property is using the obsolete convention; the
    // target doubles as the property in that case.
    let property = if request.property == x11rb::NONE {
        request.target
    } else {
        request.property
    };
    let mut accepted = true;
    if request.target == atoms.targets {
        let mut list = vec![atoms.targets, atoms.timestamp];
        list.extend_from_slice(&atoms.data);
        connection.change_property32(
            PropMode::REPLACE,
            request.requestor,
            property,
            AtomEnum::ATOM,
            &list,
        )?;
    } else if request.target == atoms.timestamp {
        connection.change_property32(
            PropMode::REPLACE,
            request.requestor,
            property,
            AtomEnum::INTEGER,
            &[request.time],
        )?;
    } else if let Some(index) = atoms.data.iter().position(|atom| *atom == request.target) {
        let data = content.payload(content.targets()[index]);
        connection.change_property8(
            PropMode::REPLACE,
            request.requestor,
            property,
            request.target,
            &data,
        )?;
    } else {
        // Refusing is reported by naming no property, which is what tells the
        // requestor to try a different target.
        accepted = false;
    }
    let notify = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: request.time,
        requestor: request.requestor,
        selection: request.selection,
        target: request.target,
        property: if accepted { property } else { x11rb::NONE },
    };
    connection.send_event(false, request.requestor, EventMask::NO_EVENT, notify)?;
    connection.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 文本那几种：现代程序问的 `UTF8_STRING` 给 UTF-8，ICCCM 里是 Latin-1 的
    /// `STRING` / `TEXT` 得转过来，落不下的字符换个问号——拿 UTF-8 去冒充 Latin-1
    /// 会让整段文字变乱码。
    #[test]
    fn a_text_copy_answers_in_the_encoding_each_target_asks_for() {
        let content = Content::Text("caf\u{e9} 中文".into());
        assert_eq!(
            content.payload("UTF8_STRING"),
            "caf\u{e9} 中文".as_bytes(),
            "现代程序读的是 UTF-8"
        );
        assert_eq!(
            content.payload("text/plain;charset=utf-8"),
            "caf\u{e9} 中文".as_bytes()
        );
        // `\u{e9}` 在 Latin-1 里是 0xE9，汉字不在，换问号。
        assert_eq!(
            content.payload("STRING"),
            [0x63, 0x61, 0x66, 0xe9, 0x20, b'?', b'?']
        );
        assert_eq!(content.payload("TEXT"), content.payload("STRING"));
    }

    /// 文件那份还是老样子：`text/uri-list` 是 file URI，其余类型给路径文本。
    #[test]
    fn a_file_copy_keeps_its_own_targets() {
        let content = Content::Files(vec![PathBuf::from("/tmp/a b.txt")]);
        assert_eq!(content.targets(), FILE_TARGETS);
        assert_eq!(
            String::from_utf8(content.payload("text/uri-list")).unwrap(),
            "file:///tmp/a%20b.txt\r\n"
        );
        assert_eq!(
            String::from_utf8(content.payload("UTF8_STRING")).unwrap(),
            "/tmp/a b.txt"
        );
    }
}
