//! Read-only pointer shape sampling through XFixes, independent of desktop
//! pixel capture.
use anyhow::{Context, Result, ensure};
use std::sync::Mutex;
use x11rb::protocol::xfixes::{ConnectionExt as _, GetCursorImageAndNameReply};
use x11rb::rust_connection::RustConnection;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Pointer {
    /// The XFixes cursor serial: changes whenever the shape does.
    pub handle: usize,
    pub showing: bool,
    pub x: i32,
    pub y: i32,
}
pub(crate) struct Shape {
    pub width: u32,
    pub height: u32,
    pub hotspot: [u32; 2],
    pub rgba: Vec<u8>,
    pub kind: i32,
}

struct Sampler {
    connection: RustConnection,
    last: Option<GetCursorImageAndNameReply>,
}
/// One connection for the session's pointer polling; reopened after an error.
static SAMPLER: Mutex<Option<Sampler>> = Mutex::new(None);

fn sample<T>(read: impl FnOnce(&GetCursorImageAndNameReply) -> T) -> Result<T> {
    let mut sampler = SAMPLER.lock().unwrap_or_else(|e| e.into_inner());
    if sampler.is_none() {
        let (connection, _) = x11rb::connect(None).context("连接 X11 显示失败")?;
        connection
            .xfixes_query_version(4, 0)?
            .reply()
            .context("X 服务器不支持 XFixes")?;
        *sampler = Some(Sampler {
            connection,
            last: None,
        });
    }
    let active = sampler.as_mut().unwrap();
    let reply = active
        .connection
        .xfixes_get_cursor_image_and_name()
        .map_err(anyhow::Error::from)
        .and_then(|cookie| cookie.reply().map_err(anyhow::Error::from));
    match reply {
        Ok(reply) => {
            let result = read(&reply);
            active.last = Some(reply);
            Ok(result)
        }
        Err(error) => {
            *sampler = None;
            Err(error.context("读取光标失败"))
        }
    }
}

pub(crate) fn pointer() -> Result<Pointer> {
    sample(|reply| Pointer {
        handle: reply.cursor_serial as usize,
        // XFixes reports a hidden pointer as an empty image.
        showing: reply.width > 0
            && reply.height > 0
            && reply.cursor_image.iter().any(|pixel| pixel >> 24 != 0),
        x: i32::from(reply.x),
        y: i32::from(reply.y),
    })
}

pub(crate) fn shape(handle: usize) -> Result<Shape> {
    let cached = {
        let sampler = SAMPLER.lock().unwrap_or_else(|e| e.into_inner());
        sampler
            .as_ref()
            .and_then(|s| s.last.as_ref())
            .filter(|last| last.cursor_serial as usize == handle)
            .map(convert)
    };
    match cached {
        Some(shape) => shape,
        None => sample(convert)?,
    }
}

fn convert(reply: &GetCursorImageAndNameReply) -> Result<Shape> {
    let (width, height) = (u32::from(reply.width), u32::from(reply.height));
    ensure!(width > 0 && height > 0, "光标不可见");
    ensure!(
        reply.cursor_image.len() >= (width * height) as usize,
        "光标图像不完整"
    );
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for &argb in &reply.cursor_image[..(width * height) as usize] {
        // XFixes hands out premultiplied ARGB; PNG wants straight alpha.
        let alpha = argb >> 24;
        let straight = |shift: u32| {
            let channel = (argb >> shift) & 0xff;
            if alpha == 0 {
                0
            } else {
                ((channel * 255 + alpha / 2) / alpha).min(255) as u8
            }
        };
        rgba.extend_from_slice(&[straight(16), straight(8), straight(0), alpha as u8]);
    }
    Ok(Shape {
        width,
        height,
        hotspot: [u32::from(reply.xhot), u32::from(reply.yhot)],
        rgba,
        kind: system_kind(&String::from_utf8_lossy(&reply.name)),
    })
}

/// The Windows system cursor a themed X cursor stands for, by its name in the
/// cursor theme (both the X11 core and the CSS names are in use), so the
/// controller can show its own native pointer for common shapes.
fn system_kind(name: &str) -> i32 {
    match name {
        "left_ptr" | "default" | "arrow" | "top_left_arrow" => 32512,
        "xterm" | "text" | "ibeam" => 32513,
        "watch" | "wait" => 32514,
        "crosshair" | "cross" | "tcross" => 32515,
        "sb_up_arrow" | "up-arrow" => 32516,
        "top_left_corner"
        | "bottom_right_corner"
        | "nwse-resize"
        | "nw-resize"
        | "se-resize"
        | "size_fdiag" => 32642,
        "top_right_corner" | "bottom_left_corner" | "nesw-resize" | "ne-resize" | "sw-resize"
        | "size_bdiag" => 32643,
        "sb_h_double_arrow" | "ew-resize" | "col-resize" | "left_side" | "right_side"
        | "e-resize" | "w-resize" | "size_hor" => 32644,
        "sb_v_double_arrow" | "ns-resize" | "row-resize" | "top_side" | "bottom_side"
        | "n-resize" | "s-resize" | "size_ver" => 32645,
        "fleur" | "move" | "all-scroll" | "size_all" => 32646,
        "not-allowed" | "crossed_circle" | "no-drop" | "forbidden" => 32648,
        "hand1" | "hand2" | "pointer" | "pointing_hand" => 32649,
        "left_ptr_watch" | "progress" => 32650,
        "question_arrow" | "help" | "whats_this" => 32651,
        _ => 0,
    }
}
