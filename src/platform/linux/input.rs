//! Controlled-session input injection on Linux.
//!
//! Events are injected through the X Test extension, which needs no device
//! permissions inside the user's own Xorg session. Keys travel by physical
//! position: a Windows virtual-key code is mapped to its evdev scan position
//! and from there to the X keycode (evdev + 8 under the evdev and libinput
//! drivers), so the local layout decides the character, as it would for a
//! keyboard plugged into this machine.
//!
//! In a Wayland session XTest only reaches XWayland clients, so there keys,
//! buttons and motion go through a uinput device, as Sunshine does (it needs
//! write access to `/dev/uinput`, granted by the udev rule the packages
//! ship), and text through `zwp_virtual_keyboard_v1`. X11 still reads the key
//! map, the pointer and the lock keys. Xorg sessions keep XTest alone, which
//! needs no setup and positions the pointer absolutely. An absolute position
//! in a Wayland session goes to a second, absolute uinput pointer that the
//! compositor resolves in its own layout (xdg-output); without that layout it
//! becomes a relative move from the current pointer position.
use anyhow::{Context, Result, bail, ensure};
use std::time::Duration;
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Keycode, Keysym, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION_NOTIFY: u8 = 6;
const NO_SYMBOL: Keysym = 0;
/// The pause after each typed character.
const TEXT_PACE: Duration = Duration::from_millis(3);

/// Lock keys whose state a controller can ask to match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lock {
    Caps,
    Num,
    Scroll,
}
impl Lock {
    pub fn from_virtual_key(key: u16) -> Option<Self> {
        match key {
            0x14 => Some(Self::Caps),
            0x90 => Some(Self::Num),
            0x91 => Some(Self::Scroll),
            _ => None,
        }
    }
}

/// uinput 注入后端。
///
/// Wayland 会话下 XTest 只能影响 XWayland 客户端，Hyprland 等合成器里的
/// 原生客户端收不到；Wayland 会话有 `/dev/uinput` 时改用它注入。X11 仍保留
/// 用于读取键位表、指针位置与锁定键状态。
///
/// 绝对定位另有一台带 `ABS_X`/`ABS_Y` 的设备（Sunshine 的做法），轴范围固定，
/// 由合成器按它自己全局布局的外接矩形解算：不经过 X，也就没有 XWayland 那套
/// 尺寸与缩放。
mod uinput {
    use anyhow::{Context, Result, ensure};
    use std::fs::{File, OpenOptions};
    use std::io::Write;
    use std::os::unix::io::AsRawFd;

    const EV_SYN: u16 = 0x00;
    const EV_KEY: u16 = 0x01;
    const EV_REL: u16 = 0x02;
    const EV_ABS: u16 = 0x03;

    const REL_X: u16 = 0x00;
    const REL_Y: u16 = 0x01;
    const REL_HWHEEL: u16 = 0x06;
    const REL_WHEEL: u16 = 0x08;
    const ABS_X: u16 = 0x00;
    const ABS_Y: u16 = 0x01;
    const SYN_REPORT: u16 = 0x00;
    /// 绝对轴只受一根指针控制，而不是某一块屏上的坐标。
    const INPUT_PROP_POINTER: u32 = 0;
    /// 绝对轴的最大值，与 Sunshine 一样固定，不随布局变：布局变了只改换算，
    /// 不用换设备（换设备又要等合成器重新接手）。
    pub(crate) const AXIS_MAX: i32 = 65535;

    const BTN_LEFT: u16 = 0x110;
    const BTN_RIGHT: u16 = 0x111;
    const BTN_MIDDLE: u16 = 0x112;
    const BTN_SIDE: u16 = 0x113;
    const BTN_EXTRA: u16 = 0x114;

    // linux/uinput.h 的 _IOW('U', n, size)；x86、ARM 与 RISC-V 上相同。
    const UI_DEV_CREATE: libc::c_ulong = 0x5501;
    const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
    const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;
    const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
    const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
    const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;
    const UI_SET_ABSBIT: libc::c_ulong = 0x4004_5567;
    const UI_SET_PROPBIT: libc::c_ulong = 0x4004_556E;
    const UI_ABS_SETUP: libc::c_ulong = 0x401c_5504;

    #[repr(C)]
    struct InputId {
        bustype: u16,
        vendor: u16,
        product: u16,
        version: u16,
    }
    #[repr(C)]
    struct UinputSetup {
        id: InputId,
        name: [libc::c_char; 80],
        ff_effects_max: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct InputAbsInfo {
        value: i32,
        minimum: i32,
        maximum: i32,
        fuzz: i32,
        flat: i32,
        resolution: i32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct UinputAbsSetup {
        code: u16,
        pad: u16,
        absinfo: InputAbsInfo,
    }
    #[repr(C)]
    struct InputEvent {
        sec: libc::c_long,
        usec: libc::c_long,
        kind: u16,
        code: u16,
        value: i32,
    }

    pub(crate) struct Device {
        file: File,
    }

    fn set(fd: i32, request: libc::c_ulong, value: libc::c_ulong) -> Result<()> {
        let rc = unsafe { libc::ioctl(fd, request, value) };
        ensure!(
            rc >= 0,
            "uinput ioctl 0x{request:x} 失败：{}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }

    /// 声明一条绝对轴以及它的范围：合成器就是按这个范围把轴上的值解算成
    /// 全局位置的，范围不对落点就整体偏。
    fn set_abs(fd: i32, axis: u16, maximum: i32) -> Result<()> {
        set(fd, UI_SET_ABSBIT, axis.into())?;
        let setup = UinputAbsSetup {
            code: axis,
            absinfo: InputAbsInfo {
                maximum,
                ..Default::default()
            },
            ..Default::default()
        };
        let rc = unsafe { libc::ioctl(fd, UI_ABS_SETUP, &setup as *const _) };
        ensure!(
            rc >= 0,
            "设置绝对轴范围失败：{}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }

    /// X 按键编号到 evdev 按键码（1 左、2 中、3 右、8 侧、9 扩展）。
    pub(crate) fn button_code(x_button: u8) -> Option<u16> {
        Some(match x_button {
            1 => BTN_LEFT,
            2 => BTN_MIDDLE,
            3 => BTN_RIGHT,
            8 => BTN_SIDE,
            9 => BTN_EXTRA,
            _ => return None,
        })
    }

    impl Device {
        /// 打开 `/dev/uinput` 并注册一台设备，`capabilities` 声明它能报什么。
        fn create(name: &str, capabilities: impl FnOnce(i32) -> Result<()>) -> Result<Self> {
            let file = OpenOptions::new()
                .write(true)
                .open("/dev/uinput")
                .context("打开 /dev/uinput 失败（需要 input 组与 udev 规则）")?;
            let fd = file.as_raw_fd();
            capabilities(fd)?;
            let mut device_name = [0 as libc::c_char; 80];
            for (slot, byte) in device_name.iter_mut().zip(name.bytes()) {
                *slot = byte as libc::c_char;
            }
            let setup = UinputSetup {
                id: InputId {
                    bustype: 0x06, // BUS_VIRTUAL，与 ydotool 一致
                    vendor: 0x2333,
                    product: 0x6666,
                    version: 1,
                },
                name: device_name,
                ff_effects_max: 0,
            };
            let rc = unsafe { libc::ioctl(fd, UI_DEV_SETUP, &setup as *const _) };
            ensure!(
                rc >= 0,
                "注册 uinput 设备失败：{}",
                std::io::Error::last_os_error()
            );
            set(fd, UI_DEV_CREATE, 0)?;
            Ok(Self { file })
        }

        /// 键鼠合一的相对设备，X 与合成器都能用。
        pub(crate) fn open() -> Result<Self> {
            Self::create("OpenUUYC virtual input", |fd| {
                set(fd, UI_SET_EVBIT, EV_KEY.into())?;
                set(fd, UI_SET_EVBIT, EV_REL.into())?;
                // Keyboard keys (every code `evdev` maps to is below 256) and the
                // mouse buttons only. Declaring BTN_TOUCH, BTN_TOOL_* or the
                // joystick range makes udev and libinput classify the device as
                // something else.
                for code in (1..0x100u16).chain(BTN_LEFT..=BTN_EXTRA) {
                    set(fd, UI_SET_KEYBIT, code.into())?;
                }
                for bit in [REL_X, REL_Y, REL_WHEEL, REL_HWHEEL] {
                    set(fd, UI_SET_RELBIT, bit.into())?;
                }
                Ok(())
            })
        }

        /// 绝对指针：`ABS_X`/`ABS_Y` 的范围固定为 `0..=AXIS_MAX`，合成器把它按
        /// 比例映射到自己全部输出的外接矩形上。
        pub(crate) fn open_absolute() -> Result<Self> {
            Self::create("OpenUUYC virtual absolute pointer", |fd| {
                // 按钮也要声明：libinput 靠它认出这是一只鼠标，否则一条只有绝对
                // 轴的设备会被归到触摸屏之类里去。
                set(fd, UI_SET_EVBIT, EV_KEY.into())?;
                for code in BTN_LEFT..=BTN_EXTRA {
                    set(fd, UI_SET_KEYBIT, code.into())?;
                }
                set(fd, UI_SET_EVBIT, EV_ABS.into())?;
                set_abs(fd, ABS_X, AXIS_MAX)?;
                set_abs(fd, ABS_Y, AXIS_MAX)?;
                // 指针而不是“直接设备”：INPUT_PROP_DIRECT 是在说坐标只落在
                // 一块屏上，而这里描述的是整个桌面。
                set(fd, UI_SET_PROPBIT, INPUT_PROP_POINTER.into())
            })
            .context("创建虚拟绝对鼠标失败")
        }

        fn emit(&self, kind: u16, code: u16, value: i32) -> Result<()> {
            let event = InputEvent {
                sec: 0,
                usec: 0,
                kind,
                code,
                value,
            };
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    &event as *const _ as *const u8,
                    std::mem::size_of::<InputEvent>(),
                )
            };
            let mut sink = &self.file;
            sink.write_all(bytes).context("写入 uinput 事件失败")?;
            Ok(())
        }

        fn sync(&self) -> Result<()> {
            self.emit(EV_SYN, SYN_REPORT, 0)
        }

        pub(crate) fn key(&self, code: u16, down: bool) -> Result<()> {
            self.emit(EV_KEY, code, i32::from(down))?;
            self.sync()
        }

        pub(crate) fn button(&self, x_button: u8, down: bool) -> Result<()> {
            let code = button_code(x_button).context("不支持的鼠标按键")?;
            self.key(code, down)
        }

        pub(crate) fn move_by(&self, dx: i32, dy: i32) -> Result<()> {
            self.emit(EV_REL, REL_X, dx)?;
            self.emit(EV_REL, REL_Y, dy)?;
            self.sync()
        }

        /// 绝对位置，取的是设备轴上的值（`0..=AXIS_MAX`）。
        pub(crate) fn move_absolute(&self, x: i32, y: i32) -> Result<()> {
            self.emit(EV_ABS, ABS_X, x)?;
            self.emit(EV_ABS, ABS_Y, y)?;
            self.sync()
        }

        pub(crate) fn wheel(&self, horizontal: bool, positive: bool) -> Result<()> {
            let (code, value) = match (horizontal, positive) {
                (false, true) => (REL_WHEEL, 1),
                (false, false) => (REL_WHEEL, -1),
                (true, true) => (REL_HWHEEL, 1),
                (true, false) => (REL_HWHEEL, -1),
            };
            self.emit(EV_REL, code, value)?;
            self.sync()
        }
    }

    impl Drop for Device {
        fn drop(&mut self) {
            let _ = unsafe { libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY, 0) };
        }
    }
}

/// Whether Hyprland's IPC is there to ask; other compositors have no
/// equivalent, and positions then come from XWayland.
fn hyprland() -> bool {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
}

/// 向 Hyprland 的 IPC socket 发一条请求并读回响应。
///
/// XWayland 的 `query_pointer` 只在 XWayland 表面上跟随指针；光标停在原生
/// Wayland 客户端上时它返回的是陈旧值（实测移动到极限后仍然不动），所以
/// 绝对定位必须用合成器的权威数据。
fn hyprland_request(command: &str) -> Option<String> {
    use std::io::{Read as _, Write as _};
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    let runtime = std::env::var("XDG_RUNTIME_DIR").ok()?;
    let mut socket =
        std::os::unix::net::UnixStream::connect(format!("{runtime}/hypr/{signature}/.socket.sock"))
            .ok()?;
    socket
        .set_read_timeout(Some(Duration::from_millis(300)))
        .ok()?;
    socket.write_all(command.as_bytes()).ok()?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match socket.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                buffer.extend_from_slice(&chunk[..read]);
                if buffer.len() > (1 << 20) {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8(buffer).ok()
}

/// Hyprland 合成器里的光标位置（逻辑全局布局坐标）。
fn hyprland_cursor() -> Option<(i32, i32)> {
    let reply = hyprland_request("cursorpos")?;
    let (x, y) = reply.trim().split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// Hyprland 每个显示器的逻辑矩形：`(名称, left, top, 逻辑宽, 逻辑高)`。
///
/// `hyprctl monitors` 报的是物理分辨率与缩放，光标坐标用的是逻辑尺寸，
/// 所以要按 `scale` 折算。
fn hyprland_monitors() -> Option<Vec<(String, i32, i32, i32, i32)>> {
    let reply = hyprland_request("j/monitors")?;
    let value: serde_json::Value = serde_json::from_str(&reply).ok()?;
    let mut monitors = Vec::new();
    for monitor in value.as_array()? {
        let name = monitor.get("name")?.as_str()?.to_owned();
        let left = monitor.get("x")?.as_i64()? as i32;
        let top = monitor.get("y")?.as_i64()? as i32;
        let width = monitor.get("width")?.as_i64()? as i32;
        let height = monitor.get("height")?.as_i64()? as i32;
        let scale = monitor.get("scale").and_then(|v| v.as_f64()).unwrap_or(1.0);
        let scale = if scale > 0.0 { scale } else { 1.0 };
        monitors.push((
            name,
            left,
            top,
            (f64::from(width) / scale).round() as i32,
            (f64::from(height) / scale).round() as i32,
        ));
    }
    (!monitors.is_empty()).then_some(monitors)
}

/// X11 侧每个显示器的矩形，按键名对齐到 Hyprland 的逻辑矩形。
///
/// 两套坐标空间完全不同：XWayland 把显示器按物理像素横排（本机是
/// 4800x1800，eDP-1 在左、HDMI-A-3 在右），Hyprland 按逻辑像素竖排
/// （2304x2520，HDMI-A-3 在上、eDP-1 在下）。绝对定位要把程序算出的
/// X11 全局坐标换算到 Hyprland 空间。
fn screen_rect_maps() -> Vec<(String, (i32, i32, i32, i32), (i32, i32, i32, i32))> {
    let Ok(topology) = super::display::topology::Topology::query(true) else {
        return Vec::new();
    };
    let x11 = topology.monitors();
    let Some(hypr) = hyprland_monitors() else {
        return Vec::new();
    };
    let mut maps = Vec::new();
    for monitor in &x11 {
        let Some((_, hx, hy, hw, hh)) = hypr.iter().find(|(name, ..)| *name == monitor.name) else {
            continue;
        };
        maps.push((
            monitor.name.clone(),
            (
                monitor.left,
                monitor.top,
                monitor.width as i32,
                monitor.height as i32,
            ),
            (*hx, *hy, *hw, *hh),
        ));
    }
    maps
}

/// 通过 `zwp_virtual_keyboard_v1` 输入文本。
///
/// 与 uinput 不同，这条协议允许客户端**自带 xkb keymap**，因此可以输入键位表
/// 里不存在的字符（中文等）。Hyprland 暴露 `zwp_virtual_keyboard_manager_v1`。
///
/// 注意：XWayland 客户端使用的是 X 自己的键位表，虚拟键盘的自定义 keymap 对它
/// 无效，那类窗口仍走 XTest。
mod virtual_keyboard {
    use anyhow::{Context, Result};
    use std::os::fd::AsFd;
    use std::time::Duration;
    use wayland_client::globals::{GlobalList, registry_queue_init};
    use wayland_client::protocol::{wl_registry, wl_seat};
    use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
    use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
        zwp_virtual_keyboard_manager_v1 as manager, zwp_virtual_keyboard_v1 as keyboard,
    };

    struct State;

    impl Dispatch<wl_registry::WlRegistry, wayland_client::globals::GlobalListContents> for State {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &wayland_client::globals::GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    wayland_client::delegate_noop!(State: ignore manager::ZwpVirtualKeyboardManagerV1);
    wayland_client::delegate_noop!(State: ignore keyboard::ZwpVirtualKeyboardV1);
    wayland_client::delegate_noop!(State: ignore wl_seat::WlSeat);

    /// 发送给合成器的第一个键码。协议里传的是 **evdev 风格**键码，合成器
    /// 与客户端都会 `+8` 得到 xkb keycode（Hyprland `Keyboard.cpp`：
    /// `updateXkbStateWithKey(event.key + 8, ...)`），所以 keymap 里声明的
    /// 键码必须是 `FIRST_CODE + KEYCODE_OFFSET`。与 wtype 一致取 1。
    const FIRST_CODE: u8 = 1;
    /// evdev 键码到 xkb keycode 的固定偏移。
    const KEYCODE_OFFSET: u8 = 8;

    pub(crate) struct Keyboard {
        /// Held so the queue and the globals registry stay connected.
        connection: Connection,
        globals: GlobalList,
        queue: EventQueue<State>,
    }

    /// 生成只包含所需字符的 keymap，返回 (字节, 覆盖的字符数)。
    fn keymap_for(text: &str) -> (String, usize) {
        let mut chars: Vec<char> = Vec::new();
        for c in text.chars() {
            if !chars.contains(&c) && chars.len() < 56 {
                chars.push(c);
            }
        }
        let mut out = String::from("xkb_keymap {\n  xkb_keycodes \"vk\" {\n");
        out.push_str(&format!("    minimum = {};\n", FIRST_CODE + KEYCODE_OFFSET));
        out.push_str(&format!(
            "    maximum = {};\n",
            FIRST_CODE as usize + KEYCODE_OFFSET as usize + chars.len().saturating_sub(1)
        ));
        for index in 0..chars.len() {
            out.push_str(&format!(
                "    <VK{index:02}> = {};\n",
                FIRST_CODE as usize + KEYCODE_OFFSET as usize + index
            ));
        }
        out.push_str("  };\n  xkb_types \"vk\" { include \"complete\" };\n");
        out.push_str("  xkb_compat \"vk\" { include \"complete\" };\n");
        out.push_str("  xkb_symbols \"vk\" {\n");
        for (index, c) in chars.iter().enumerate() {
            let point = *c as u32;
            let sym = if point < 0x100 {
                point
            } else {
                0x0100_0000 | point
            };
            out.push_str(&format!("    key <VK{index:02}> {{ [ 0x{sym:08x} ] }};\n"));
        }
        out.push_str("  };\n};\n");
        (out, chars.len())
    }

    fn keymap_fd(bytes: &[u8]) -> Result<std::os::fd::OwnedFd> {
        use std::os::fd::FromRawFd;
        let name = c"openuuyc-vk";
        let raw = unsafe { libc::memfd_create(name.as_ptr(), 0) };
        anyhow::ensure!(raw >= 0, "创建 keymap 内存文件失败");
        let mut file = unsafe { std::fs::File::from_raw_fd(raw) };
        use std::io::Write as _;
        file.write_all(bytes).context("写入 keymap 失败")?;
        Ok(std::os::fd::OwnedFd::from(file))
    }

    impl Keyboard {
        pub(crate) fn connect() -> Result<Self> {
            let connection = Connection::connect_to_env().context("连接 Wayland 失败")?;
            let (globals, queue): (GlobalList, EventQueue<State>) =
                registry_queue_init(&connection).context("枚举 Wayland 全局对象失败")?;
            // Bind once up front so a desktop without the protocol is reported
            // at startup rather than on the first keystroke.
            {
                let qh = queue.handle();
                let _manager: manager::ZwpVirtualKeyboardManagerV1 =
                    globals
                        .bind(&qh, 1..=1, ())
                        .context("桌面没有提供 zwp_virtual_keyboard_manager_v1")?;
                let _seat: wl_seat::WlSeat =
                    globals.bind(&qh, 1..=8, ()).context("没有 Wayland seat")?;
            }
            queue.flush().context("提交虚拟键盘请求失败")?;
            Ok(Self {
                connection,
                globals,
                queue,
            })
        }

        /// 输入一段文本：新建一个虚拟键盘对象，上传含这些字符的 keymap，
        /// 逐个按键，最后销毁该对象。
        ///
        /// **每次调用都必须新建对象。** Hyprland 只在对象第一次收到 keymap
        /// 时建立键位映射，之后在同一对象上重新上传 keymap 不会生效；手机输入
        /// 法逐字符发送（每次 `chars=1`），复用对象会让每个字符都按第一个字符
        /// 的键位输入（输入 `XYZ` 得到 `XXX`）。wtype 同样每次新建对象。
        pub(crate) fn text(&mut self, text: &str, permitted: &dyn Fn() -> bool) -> Result<()> {
            let (keymap, bound) = keymap_for(text);
            if bound == 0 {
                return Ok(());
            }
            let qh = self.queue.handle();
            let manager: manager::ZwpVirtualKeyboardManagerV1 =
                self.globals
                    .bind(&qh, 1..=1, ())
                    .context("桌面没有提供 zwp_virtual_keyboard_manager_v1")?;
            let seat: wl_seat::WlSeat = self
                .globals
                .bind(&qh, 1..=8, ())
                .context("没有 Wayland seat")?;
            let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
            // keymap 必须在上报按键之前上传，且要等合成器处理完
            // （否则合成器会报 NO_KEYMAP）。xkbcommon 的
            // xkb_keymap_new_from_string 要求 NUL 结尾，长度也要算上。
            let mut bytes = keymap.into_bytes();
            bytes.push(0);
            let fd = keymap_fd(&bytes)?;
            keyboard.keymap(1, fd.as_fd(), bytes.len() as u32);
            self.queue
                .roundtrip(&mut State)
                .context("等待虚拟键盘 keymap 生效失败")?;
            // 清掉可能残留的修饰键状态。
            keyboard.modifiers(0, 0, 0, 0);
            let _ = self.queue.roundtrip(&mut State);

            let mut index = 0u8;
            let mut seen: Vec<char> = Vec::new();
            for c in text.chars() {
                if !permitted() {
                    break;
                }
                let position = match seen.iter().position(|seen| *seen == c) {
                    Some(found) => found as u8,
                    None => {
                        if seen.len() >= 56 {
                            continue;
                        }
                        seen.push(c);
                        let assigned = index;
                        index += 1;
                        assigned
                    }
                };
                // 发送 evdev 风格键码；合成器与客户端会各自 +8 查 keymap。
                let code = u32::from(FIRST_CODE + position);
                keyboard.key(0, code, 1);
                let _ = self.queue.roundtrip(&mut State);
                keyboard.key(0, code, 0);
                let _ = self.queue.roundtrip(&mut State);
                // 输入法（IBus）会丢过快的按键。
                std::thread::sleep(Duration::from_millis(4));
            }
            keyboard.destroy();
            let _ = self.queue.roundtrip(&mut State);
            let _ = &self.connection;
            Ok(())
        }
    }
}

/// 绝对指针覆盖的区域：合成器全局布局的外接矩形，左上角加尺寸。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Desktop {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

/// 合成器全局布局的外接矩形，包括没有名字的输出：合成器把绝对指针映射到
/// 它全部输出的外接矩形上（wlroots 的 output layout、Hyprland、niri、KWin 的
/// 工作区、Mutter 的 stage），少算一块屏落点就整体偏。
pub(crate) fn layout_bounds(layout: &crate::platform::CompositorLayout) -> Option<Desktop> {
    let mut outputs = layout
        .outputs
        .iter()
        .filter(|o| o.width > 0 && o.height > 0);
    let first = outputs.next()?;
    let span = |o: &crate::platform::CompositorOutput| {
        (
            i64::from(o.left),
            i64::from(o.top),
            i64::from(o.left) + i64::from(o.width),
            i64::from(o.top) + i64::from(o.height),
        )
    };
    let (mut left, mut top, mut right, mut bottom) = span(first);
    for output in outputs {
        let (l, t, r, b) = span(output);
        left = left.min(l);
        top = top.min(t);
        right = right.max(r);
        bottom = bottom.max(b);
    }
    Some(Desktop {
        left: i32::try_from(left).ok()?,
        top: i32::try_from(top).ok()?,
        width: u32::try_from(right - left).ok()?,
        height: u32::try_from(bottom - top).ok()?,
    })
}

/// 合成器把一个轴值落到像素 `origin + floor(value × extent / (AXIS_MAX + 1))`
/// 上（libinput 按 `max - min + 1` 换算）。取像素中心对应的值，取整之后正好
/// 落回这个像素。超过 `AXIS_MAX + 1` 像素宽的布局会有相邻像素共用一个值。
fn axis_value(position: i32, origin: i32, extent: u32) -> i32 {
    if extent == 0 {
        return 0;
    }
    let range = i64::from(uinput::AXIS_MAX) + 1;
    let extent = i64::from(extent);
    let offset = (i64::from(position) - i64::from(origin)).clamp(0, extent - 1);
    ((2 * offset + 1) * range / (2 * extent)).clamp(0, i64::from(uinput::AXIS_MAX)) as i32
}

/// 内核丢弃与设备当前值相同的绝对坐标，所以要重写同一个值时先在单独一帧里
/// 挪开一个单位（不到一像素）。在轴的最大值处往回挪，不能出界。只有两条轴
/// 都相同才需要：只丢一条轴时，合成器沿用设备自己的那个值，正是目标值。
fn nudge(last: (i32, i32), value: (i32, i32)) -> Option<(i32, i32)> {
    (last == value).then(|| {
        let x = if value.0 >= uinput::AXIS_MAX {
            value.0 - 1
        } else {
            value.0 + 1
        };
        (x, value.1)
    })
}

/// 合成器接手一台刚建的 uinput 设备要多久。这段时间里写进去的绝对坐标谁都
/// 收不到（evdev 只把 open 之后的事件交给读者），所以要反复补写。
const FRESH_FOR: Duration = Duration::from_millis(1500);

/// 上面那个窗口期里补写的间隔。
const REPEAT_EVERY: Duration = Duration::from_millis(250);

/// 窗口期里还要补写的位置。
struct Repeat {
    until: std::time::Instant,
    next: std::time::Instant,
    value: (i32, i32),
}

/// 由合成器解算的绝对指针。轴范围固定（与 Sunshine 一样），按布局外接矩形
/// 换算，所以布局变了也不用换设备。
struct Absolute {
    device: uinput::Device,
    made: std::time::Instant,
    /// 设备轴上的当前值（内核里的那份），新建时为 0。
    last: (i32, i32),
    repeat: Option<Repeat>,
}

impl Absolute {
    fn create() -> Result<Self> {
        Ok(Self {
            device: uinput::Device::open_absolute()?,
            made: std::time::Instant::now(),
            last: (0, 0),
            repeat: None,
        })
    }

    /// 写入一对轴值。与当前值相同时内核会把它整帧丢掉，而光标可能已经被别的
    /// 设备（相对设备、本机鼠标、程序自己移动指针）挪走了，所以先在单独一帧里
    /// 挪开一个单位，再写目标值。
    fn emit(&mut self, value: (i32, i32)) -> Result<()> {
        if let Some((x, y)) = nudge(self.last, value) {
            self.device.move_absolute(x, y)?;
        }
        self.device.move_absolute(value.0, value.1)?;
        self.last = value;
        Ok(())
    }

    /// 把合成器空间里的一点写进设备，设备还新的时候安排补写。
    fn aim(&mut self, x: i32, y: i32, bounds: Desktop) -> Result<()> {
        let value = (
            axis_value(x, bounds.left, bounds.width),
            axis_value(y, bounds.top, bounds.height),
        );
        self.emit(value)?;
        let now = std::time::Instant::now();
        let until = self.made + FRESH_FOR;
        self.repeat = (now < until).then(|| Repeat {
            until,
            next: now + REPEAT_EVERY,
            value,
        });
        Ok(())
    }

    /// 窗口期里按间隔补写。补写的值与当前值相同，`emit` 会先挪开一个单位，
    /// 每次补写都以目标值结束，窗口结束时不必再写。
    fn flush(&mut self) -> Result<()> {
        let Some(repeat) = &mut self.repeat else {
            return Ok(());
        };
        let now = std::time::Instant::now();
        if now >= repeat.until {
            self.repeat = None;
            return Ok(());
        }
        if now < repeat.next {
            return Ok(());
        }
        repeat.next = now + REPEAT_EVERY;
        let value = repeat.value;
        self.emit(value)
    }
}

pub(crate) struct Injector {
    connection: RustConnection,
    root: Window,
    min_keycode: Keycode,
    max_keycode: Keycode,
    /// 可用时优先进到这里，X11 只负责读键位表与指针位置。
    uinput: Option<uinput::Device>,
    /// 由合成器按它自己的全局布局解算的绝对指针。问得到合成器布局时才有，
    /// 没有就退回下面这条相对路径。
    absolute: Option<Absolute>,
    /// X11 矩形 → Hyprland 逻辑矩形的映射（按显示器名对齐）。
    screen_maps: Vec<(String, (i32, i32, i32, i32), (i32, i32, i32, i32))>,
    /// 原生 Wayland 客户端的文字输入（可携带自定义 keymap）。
    virtual_keyboard: Option<virtual_keyboard::Keyboard>,
}

impl Injector {
    pub fn open() -> Result<Self> {
        let (connection, screen) = x11rb::connect(None).context("连接 X11 显示失败")?;
        ensure!(
            connection
                .extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME)?
                .is_some(),
            "X 服务器没有 XTEST 扩展，无法注入输入"
        );
        connection.xtest_get_version(2, 2)?.reply()?;
        let setup = connection.setup();
        let root = setup.roots[screen].root;
        let (min_keycode, max_keycode) = (setup.min_keycode, setup.max_keycode);
        // Xorg delivers XTest to every client and positions absolutely;
        // uinput there would only add pointer acceleration.
        let wayland = super::capture::wayland_session();
        let uinput = if !wayland {
            None
        } else {
            match uinput::Device::open() {
                Ok(device) => {
                    tracing::info!("启用 uinput 输入注入（键盘、按键与相对位移）");
                    Some(device)
                }
                Err(error) => {
                    tracing::warn!(%error, "无法使用 uinput，回退到 XTest（只影响 XWayland 客户端）");
                    None
                }
            }
        };
        // 绝对定位另建一台设备，越早建越好：合成器接手它要几百毫秒。轴范围
        // 固定，用不用它由引擎按合成器布局决定。XTest 由 X 自己解算，用不着。
        let absolute = if uinput.is_some() {
            match Absolute::create() {
                Ok(absolute) => {
                    tracing::info!("启用 uinput 虚拟绝对指针（按合成器布局定位）");
                    Some(absolute)
                }
                Err(error) => {
                    tracing::warn!(%error, "虚拟绝对指针不可用，绝对定位改用相对移动");
                    None
                }
            }
        } else {
            None
        };
        let screen_maps = if uinput.is_some() && hyprland() {
            let maps = screen_rect_maps();
            if maps.is_empty() {
                tracing::warn!("未能对齐 X11 与 Hyprland 的显示器布局，绝对定位可能偏移");
            } else {
                tracing::info!(monitors = maps.len(), "已对齐 X11 与 Hyprland 显示器布局");
            }
            maps
        } else {
            Vec::new()
        };
        let virtual_keyboard = if wayland {
            match virtual_keyboard::Keyboard::connect() {
                Ok(keyboard) => {
                    tracing::info!("启用 zwp_virtual_keyboard 文字输入（支持任意 Unicode）");
                    Some(keyboard)
                }
                Err(error) => {
                    tracing::warn!(%error, "虚拟键盘不可用，文字输入将回退 XTest");
                    None
                }
            }
        } else {
            None
        };
        Ok(Self {
            connection,
            root,
            min_keycode,
            max_keycode,
            uinput,
            absolute,
            screen_maps,
            virtual_keyboard,
        })
    }

    /// 有没有虚拟绝对指针。有它，引擎才可能按合成器布局定位。
    pub fn has_absolute_pointer(&self) -> bool {
        self.absolute.is_some()
    }

    /// 移到合成器全局空间里的一点，由合成器按 `bounds`（它全部输出的外接
    /// 矩形）解算。设备写入失败就不再用它，之后的绝对定位回到相对移动。
    pub fn aim(&mut self, x: i32, y: i32, bounds: Desktop) -> Result<()> {
        let absolute = self.absolute.as_mut().context("虚拟绝对指针不可用")?;
        let result = absolute.aim(x, y, bounds);
        if let Err(error) = &result {
            tracing::warn!(%error, "虚拟绝对指针写入失败，停用并改用相对移动");
            self.absolute = None;
        }
        result
    }

    /// 放弃还没补写完的位置（输入被收回或释放时，不再替对端移动指针）。
    pub fn forget_aim(&mut self) {
        if let Some(absolute) = &mut self.absolute {
            absolute.repeat = None;
        }
    }

    /// 当前焦点窗口是否跑在 XWayland 里。
    fn foreground_is_xwayland(&self) -> bool {
        let Some(reply) = hyprland_request("j/activewindow") else {
            return false;
        };
        serde_json::from_str::<serde_json::Value>(&reply)
            .ok()
            .and_then(|value| value.get("xwayland").and_then(|v| v.as_bool()))
            .unwrap_or(false)
    }

    /// 把程序给出的 X11 全局坐标换算到 Hyprland 的逻辑布局坐标。
    fn to_hyprland(&self, x: i32, y: i32) -> Option<(i32, i32)> {
        for (_, x11, hypr) in &self.screen_maps {
            let (left, top, width, height) = *x11;
            let (hx, hy, hw, hh) = *hypr;
            let (right, bottom) = (left + width, top + height);
            if width <= 0 || height <= 0 || x < left || x >= right || y < top || y >= bottom {
                continue;
            }
            let u = f64::from(x - left) / f64::from(width);
            let v = f64::from(y - top) / f64::from(height);
            return Some((
                hx + (f64::from(hw) * u).round() as i32,
                hy + (f64::from(hh) * v).round() as i32,
            ));
        }
        None
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.connection
            .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)?
            .check()
            .context("注入输入事件失败")?;
        Ok(())
    }

    /// A key by Windows virtual-key code.
    pub fn key(&self, key: u16, down: bool) -> Result<()> {
        let code = evdev(key).with_context(|| format!("不支持的按键 {key:#04x}"))?;
        if let Some(device) = &self.uinput {
            return device.key(code, down);
        }
        let keycode = u8::try_from(code + 8).context("按键超出 X11 键码范围")?;
        ensure!(
            (self.min_keycode..=self.max_keycode).contains(&keycode),
            "按键超出 X 服务器键码范围"
        );
        self.fake(if down { KEY_PRESS } else { KEY_RELEASE }, keycode, 0, 0)
    }

    /// A button by X button number (1 left, 2 middle, 3 right, 8/9 side).
    pub fn button(&self, button: u8, down: bool) -> Result<()> {
        if let Some(device) = &self.uinput {
            return device.button(button, down);
        }
        self.fake(
            if down { BUTTON_PRESS } else { BUTTON_RELEASE },
            button,
            0,
            0,
        )
    }

    /// Move to a root-window position. Positions in the compositor's own
    /// layout go through [`Self::aim`] instead.
    pub fn move_to(&self, x: i32, y: i32) -> Result<()> {
        if let Some(device) = &self.uinput {
            // 设备不声明 ABS 轴（否则会被 libinput 误分类），所以按当前
            // 指针位置换算成相对位移。位置以合成器为准：每次重新读取，
            // 所以加速造成的单步偏差不会累积。
            let (cx, cy) = hyprland_cursor()
                .or_else(|| self.pointer().ok())
                .context("无法获取当前指针位置")?;
            let (tx, ty) = self.to_hyprland(x, y).unwrap_or((x, y));
            let (dx, dy) = (tx - cx, ty - cy);
            if dx == 0 && dy == 0 {
                return Ok(());
            }
            return device.move_by(dx, dy);
        }
        self.fake(
            MOTION_NOTIFY,
            0,
            i16::try_from(x).context("指针坐标超出 X11 范围")?,
            i16::try_from(y).context("指针坐标超出 X11 范围")?,
        )
    }

    /// Move by a pixel delta from wherever the pointer is.
    pub fn move_by(&self, mut dx: i32, mut dy: i32) -> Result<()> {
        if let Some(device) = &self.uinput {
            return device.move_by(dx, dy);
        }
        while dx != 0 || dy != 0 {
            let x = dx.clamp(i16::MIN.into(), i16::MAX.into());
            let y = dy.clamp(i16::MIN.into(), i16::MAX.into());
            self.fake(MOTION_NOTIFY, 1, x as i16, y as i16)?;
            dx -= x;
            dy -= y;
        }
        Ok(())
    }

    /// One wheel notch: X delivers wheels as buttons 4-7.
    pub fn wheel_notch(&self, horizontal: bool, positive: bool) -> Result<()> {
        if let Some(device) = &self.uinput {
            return device.wheel(horizontal, positive);
        }
        // Windows: positive vertical is away from the user (up), positive
        // horizontal is to the right.
        let button = match (horizontal, positive) {
            (false, true) => 4,
            (false, false) => 5,
            (true, false) => 6,
            (true, true) => 7,
        };
        self.button(button, true)?;
        self.button(button, false)
    }

    pub fn toggled(&self, lock: Lock) -> Result<bool> {
        let leds = self.connection.get_keyboard_control()?.reply()?.led_mask;
        // Core LEDs 1-3 are Caps, Num and Scroll Lock on every X keyboard map.
        Ok(leds
            & match lock {
                Lock::Caps => 1,
                Lock::Num => 2,
                Lock::Scroll => 4,
            }
            != 0)
    }

    pub fn pointer(&self) -> Result<(i32, i32)> {
        let reply = self.connection.query_pointer(self.root)?.reply()?;
        Ok((reply.root_x.into(), reply.root_y.into()))
    }

    /// Type text independently of the key layout, the way Windows' Unicode
    /// keyboard input does: each character's keysym is found in the current
    /// map, or bound for the moment to a spare keycode.
    ///
    /// Unicode input on Windows ignores the keys being held, so modifiers the
    /// controller still holds (the Ctrl of the Ctrl+V that asked for a paste)
    /// are released while typing and pressed again afterwards, and Caps Lock
    /// does not change the case of a letter. A Windows line break (CR LF)
    /// is one Enter.
    pub fn text(&mut self, text: &str, permitted: impl Fn() -> bool) -> Result<()> {
        // 原生 Wayland 客户端走虚拟键盘（XTest 到不了它们）；
        // XWayland 客户端用 X 自己的键位表，自带 keymap 的虚拟键盘对它无效，
        // 因此仍然走 XTest。焦点不在 Hyprland 的窗口上时按原生处理。
        let xwayland = self.foreground_is_xwayland();
        tracing::info!(
            chars = text.chars().count(),
            xwayland,
            virtual_keyboard = self.virtual_keyboard.is_some(),
            "文字输入"
        );
        if let Some(keyboard) = &mut self.virtual_keyboard
            && !xwayland
        {
            match keyboard.text(text, &permitted) {
                Ok(()) => {
                    tracing::info!("文字输入：虚拟键盘路径完成");
                    return Ok(());
                }
                Err(error) => {
                    tracing::warn!(%error, "虚拟键盘输入失败，回退 XTest");
                }
            }
        } else {
            tracing::info!("文字输入：使用 XTest 路径");
        }
        let count = self.max_keycode - self.min_keycode + 1;
        let map = self
            .connection
            .get_keyboard_mapping(self.min_keycode, count)?
            .reply()?;
        let per = usize::from(map.keysyms_per_keycode);
        ensure!(per > 0, "键盘映射无效");
        let find = |sym: Keysym| -> Option<(Keycode, bool)> {
            map.keysyms.chunks(per).enumerate().find_map(|(i, syms)| {
                let code = self.min_keycode + i as u8;
                if syms.first() == Some(&sym) {
                    Some((code, false))
                } else if syms.get(1) == Some(&sym) {
                    Some((code, true))
                } else {
                    None
                }
            })
        };
        let spare = map
            .keysyms
            .chunks(per)
            .rposition(|syms| syms.iter().all(|s| *s == NO_SYMBOL))
            .map(|i| self.min_keycode + i as u8);
        let shift = evdev(0xa0)
            .map(|code| (code + 8) as u8)
            .context("缺少 Shift 键")?;
        let caps = self.toggled(Lock::Caps).unwrap_or(false);
        let held = self.held_modifiers()?;
        for code in &held {
            self.fake(KEY_RELEASE, *code, 0, 0)?;
        }
        // Caps Lock changes the case of letters, including those bound to the
        // spare keycode, so it is off while typing.
        if caps {
            self.tap(0x14)?;
        }
        let mut bound = false;
        let result = (|| -> Result<()> {
            let mut previous = None;
            for c in text.chars() {
                ensure!(permitted(), "文字输入已取消");
                let after_cr = previous == Some('\r');
                previous = Some(c);
                if c == '\n' && after_cr {
                    continue;
                }
                let sym = keysym(c);
                let (code, shifted) = match find(sym) {
                    Some(found) => found,
                    None => {
                        let spare = spare.context("没有空闲键码可用于输入该字符")?;
                        let syms = vec![sym; per];
                        self.connection
                            .change_keyboard_mapping(1, spare, per as u8, &syms)?
                            .check()?;
                        bound = true;
                        // Clients refresh their keymap on MappingNotify; give
                        // the focused one a moment before the key arrives.
                        self.connection.get_input_focus()?.reply()?;
                        std::thread::sleep(Duration::from_millis(12));
                        (spare, false)
                    }
                };
                if shifted {
                    self.fake(KEY_PRESS, shift, 0, 0)?;
                }
                let typed = self
                    .fake(KEY_PRESS, code, 0, 0)
                    .and_then(|()| self.fake(KEY_RELEASE, code, 0, 0));
                if shifted {
                    self.fake(KEY_RELEASE, shift, 0, 0)?;
                }
                typed?;
                // Input methods (IBus) pass each key through another process
                // and drop keys that arrive faster than they are handled.
                std::thread::sleep(if bound {
                    Duration::from_millis(12)
                } else {
                    TEXT_PACE
                });
            }
            Ok(())
        })();
        if bound && let Some(spare) = spare {
            let empty = vec![NO_SYMBOL; per];
            let _ = self
                .connection
                .change_keyboard_mapping(1, spare, per as u8, &empty);
            let _ = self.connection.flush();
        }
        if caps {
            let _ = self.tap(0x14);
        }
        for code in &held {
            let _ = self.fake(KEY_PRESS, *code, 0, 0);
        }
        result
    }

    /// Press and release a key by Windows virtual-key code.
    fn tap(&self, key: u16) -> Result<()> {
        self.key(key, true)?;
        self.key(key, false)
    }

    /// Modifier keys other than the lock keys that are down right now.
    fn held_modifiers(&self) -> Result<Vec<Keycode>> {
        let mapping = self.connection.get_modifier_mapping()?.reply()?;
        let keys = self.connection.query_keymap()?.reply()?.keys;
        let per = usize::from(mapping.keycodes_per_modifier()).max(1);
        let mut held = Vec::new();
        for (index, codes) in mapping.keycodes.chunks(per).enumerate() {
            // Lock (index 1) is a toggle: pressing it again would flip it.
            if index == 1 {
                continue;
            }
            for &code in codes {
                if code != 0
                    && keys[usize::from(code / 8)] & (1 << (code % 8)) != 0
                    && !held.contains(&code)
                {
                    held.push(code);
                }
            }
        }
        Ok(held)
    }

    /// The process that owns the active window, as the window manager reports.
    pub fn foreground(&self) -> Option<u32> {
        let atom = |name: &str| {
            self.connection
                .intern_atom(true, name.as_bytes())
                .ok()?
                .reply()
                .ok()
                .map(|r| r.atom)
                .filter(|atom| *atom != 0)
        };
        let active = atom("_NET_ACTIVE_WINDOW")?;
        let pid = atom("_NET_WM_PID")?;
        let window = self
            .connection
            .get_property(false, self.root, active, AtomEnum::WINDOW, 0, 1)
            .ok()?
            .reply()
            .ok()?
            .value32()?
            .next()?;
        self.connection
            .get_property(false, window, pid, AtomEnum::CARDINAL, 0, 1)
            .ok()?
            .reply()
            .ok()?
            .value32()?
            .next()
    }

    pub fn flush(&mut self) -> Result<()> {
        // 合成器刚接手的设备上，位置可能要写上好几遍才有人收到。
        if let Some(absolute) = &mut self.absolute
            && let Err(error) = absolute.flush()
        {
            tracing::warn!(%error, "虚拟绝对指针写入失败，停用并改用相对移动");
            self.absolute = None;
            return Err(error);
        }
        self.connection.flush()?;
        Ok(())
    }
}

/// Lock the session through logind, as Win+L does on Windows.
///
/// A program the desktop starts runs in the user manager, outside any logind
/// session, so `loginctl lock-session` without a name cannot find one. `auto`
/// falls back to the user's graphical session.
pub(crate) fn lock_session() -> Result<()> {
    let status = std::process::Command::new("loginctl")
        .args(["lock-session", "auto"])
        .status()
        .context("无法调用 loginctl 锁定会话")?;
    ensure!(status.success(), "锁定会话失败：{status}");
    Ok(())
}

/// Open the desktop's system monitor, the counterpart of Task Manager.
pub(crate) fn system_monitor() -> Result<()> {
    for program in [
        "gnome-system-monitor",
        "plasma-systemmonitor",
        "ksysguard",
        "mate-system-monitor",
        "xfce4-taskmanager",
    ] {
        match std::process::Command::new(program)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                std::thread::spawn(move || child.wait());
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        }
    }
    bail!("没有找到系统监视器程序")
}

fn keysym(c: char) -> Keysym {
    match c {
        '\n' | '\r' => 0xff0d,
        '\t' => 0xff09,
        '\u{8}' => 0xff08,
        // Latin-1 keysyms equal their code points; everything else uses the
        // Unicode keysym range.
        ' '..='~' | '\u{a0}'..='\u{ff}' => c as u32,
        _ => 0x0100_0000 | c as u32,
    }
}

/// Windows virtual-key code to evdev key code, by physical position (US
/// names). The same table as the viewer's winit mapping, read backwards.
pub(crate) fn evdev(key: u16) -> Option<u16> {
    Some(match key {
        0x08 => 14,  // Backspace
        0x09 => 15,  // Tab
        0x0d => 28,  // Enter
        0x10 => 42,  // Shift
        0x11 => 29,  // Control
        0x12 => 56,  // Alt
        0x13 => 119, // Pause
        0x14 => 58,  // Caps Lock
        0x15 => 122, // Kana / Hangul
        0x19 => 123, // Hanja
        0x1b => 1,   // Escape
        0x1c => 92,  // Convert (Henkan)
        0x1d => 94,  // NonConvert (Muhenkan)
        0x20 => 57,  // Space
        0x21 => 104, // Page Up
        0x22 => 109, // Page Down
        0x23 => 107, // End
        0x24 => 102, // Home
        0x25 => 105, // Left
        0x26 => 103, // Up
        0x27 => 106, // Right
        0x28 => 108, // Down
        0x2c => 99,  // Print Screen
        0x2d => 110, // Insert
        0x2e => 111, // Delete
        0x2f => 138, // Help
        0x30 => 11,
        0x31..=0x39 => key - 0x31 + 2,
        0x41 => 30,
        0x42 => 48,
        0x43 => 46,
        0x44 => 32,
        0x45 => 18,
        0x46 => 33,
        0x47 => 34,
        0x48 => 35,
        0x49 => 23,
        0x4a => 36,
        0x4b => 37,
        0x4c => 38,
        0x4d => 50,
        0x4e => 49,
        0x4f => 24,
        0x50 => 25,
        0x51 => 16,
        0x52 => 19,
        0x53 => 31,
        0x54 => 20,
        0x55 => 22,
        0x56 => 47,
        0x57 => 17,
        0x58 => 45,
        0x59 => 21,
        0x5a => 44,
        0x5b => 125, // Left Windows / Super
        0x5c => 126, // Right Windows / Super
        0x5d => 127, // Menu
        0x5f => 142, // Sleep
        0x60 => 82,  // Numpad 0
        0x61 => 79,
        0x62 => 80,
        0x63 => 81,
        0x64 => 75,
        0x65 => 76,
        0x66 => 77,
        0x67 => 71,
        0x68 => 72,
        0x69 => 73,                      // Numpad 9
        0x6a => 55,                      // Numpad *
        0x6b => 78,                      // Numpad +
        0x6c => 121,                     // Numpad separator
        0x6d => 74,                      // Numpad -
        0x6e => 83,                      // Numpad .
        0x6f => 98,                      // Numpad /
        0x70..=0x79 => key - 0x70 + 59,  // F1-F10
        0x7a => 87,                      // F11
        0x7b => 88,                      // F12
        0x7c..=0x87 => key - 0x7c + 183, // F13-F24
        0x90 => 69,                      // Num Lock
        0x91 => 70,                      // Scroll Lock
        0xa0 => 42,                      // Left Shift
        0xa1 => 54,                      // Right Shift
        0xa2 => 29,                      // Left Control
        0xa3 => 97,                      // Right Control
        0xa4 => 56,                      // Left Alt
        0xa5 => 100,                     // Right Alt
        0xa6 => 158,                     // Browser Back
        0xa7 => 159,                     // Browser Forward
        0xa8 => 173,                     // Browser Refresh
        0xa9 => 128,                     // Browser Stop
        0xaa => 217,                     // Browser Search
        0xab => 156,                     // Browser Favorites
        0xac => 172,                     // Browser Home
        0xad => 113,                     // Volume Mute
        0xae => 114,                     // Volume Down
        0xaf => 115,                     // Volume Up
        0xb0 => 163,                     // Next Track
        0xb1 => 165,                     // Previous Track
        0xb2 => 166,                     // Stop
        0xb3 => 164,                     // Play/Pause
        0xb4 => 155,                     // Mail
        0xb5 => 226,                     // Media Select
        0xb6 => 157,                     // Launch App 1 (Computer)
        0xb7 => 140,                     // Launch App 2 (Calculator)
        0xba => 39,                      // ;
        0xbb => 13,                      // =
        0xbc => 51,                      // ,
        0xbd => 12,                      // -
        0xbe => 52,                      // .
        0xbf => 53,                      // /
        0xc0 => 41,                      // `
        0xc1 => 89,                      // Ro
        0xc2 => 121,                     // ABNT C2 / Numpad ,
        0xdb => 26,                      // [
        0xdc => 43,                      // \
        0xdd => 27,                      // ]
        0xde => 40,                      // '
        0xe2 => 86,                      // 102nd key
        0xff => 124,                     // Yen
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::evdev;

    #[test]
    fn letters_and_digits_follow_the_us_rows() {
        assert_eq!(evdev(b'Q'.into()), Some(16));
        assert_eq!(evdev(b'A'.into()), Some(30));
        assert_eq!(evdev(b'Z'.into()), Some(44));
        assert_eq!(evdev(b'1'.into()), Some(2));
        assert_eq!(evdev(b'9'.into()), Some(10));
        assert_eq!(evdev(0x70), Some(59));
        assert_eq!(evdev(0x79), Some(68));
        assert_eq!(evdev(0x7c), Some(183));
    }
}

#[cfg(test)]
mod absolute_tests {
    use super::{Desktop, axis_value, layout_bounds, nudge, uinput::AXIS_MAX};
    use crate::platform::{CompositorLayout, CompositorOutput};

    /// What libinput hands the compositor (`value × extent / (max − min + 1)`)
    /// and the pixel that position falls on.
    fn landed(value: i32, origin: i32, extent: u32) -> i32 {
        let position = f64::from(value) * f64::from(extent) / (f64::from(AXIS_MAX) + 1.0);
        origin + position.floor() as i32
    }

    #[test]
    fn every_pixel_maps_back_onto_itself() {
        for (origin, extent) in [(0, 1), (0, 1000), (-1920, 4793), (1000, 3793), (0, 65536)] {
            for position in origin..origin + extent as i32 {
                let value = axis_value(position, origin, extent);
                assert!((0..=AXIS_MAX).contains(&value));
                assert_eq!(landed(value, origin, extent), position, "{origin} {extent}");
            }
        }
    }

    #[test]
    fn positions_outside_the_bounds_stay_on_its_edge() {
        assert_eq!(
            axis_value(-5000, -1920, 4793),
            axis_value(-1920, -1920, 4793)
        );
        assert_eq!(axis_value(9000, -1920, 4793), axis_value(2872, -1920, 4793));
        assert_eq!(axis_value(10, 0, 0), 0);
    }

    #[test]
    fn a_repeated_value_is_nudged_within_the_axis() {
        assert_eq!(nudge((10, 20), (11, 20)), None);
        assert_eq!(nudge((10, 20), (10, 21)), None);
        assert_eq!(nudge((10, 20), (10, 20)), Some((11, 20)));
        assert_eq!(nudge((0, 0), (0, 0)), Some((1, 0)));
        assert_eq!(nudge((AXIS_MAX, 5), (AXIS_MAX, 5)), Some((AXIS_MAX - 1, 5)));
    }

    #[test]
    fn bounds_cover_every_output_named_or_not() {
        let output = |name: Option<&str>, left, top, width, height| CompositorOutput {
            name: name.map(Into::into),
            left,
            top,
            width,
            height,
        };
        let layout = CompositorLayout {
            outputs: vec![
                output(Some("DP-1"), 0, 0, 2560, 1440),
                output(None, -1080, -480, 1080, 1920),
                output(Some("HDMI-A-1"), 2560, 200, 1280, 1024),
                output(Some("broken"), 9000, 9000, 0, 0),
            ],
        };
        assert_eq!(
            layout_bounds(&layout),
            Some(Desktop {
                left: -1080,
                top: -480,
                width: 4920,
                height: 1920
            })
        );
        assert_eq!(layout_bounds(&CompositorLayout::default()), None);
    }
}

#[cfg(test)]
mod session_tests {
    /// Needs a running Xorg session: `DISPLAY=:0 cargo test -- --ignored`.
    /// Moves the pointer and puts it back.
    #[test]
    #[ignore]
    fn pointer_moves_where_asked() {
        let mut injector = super::Injector::open().unwrap();
        let before = injector.pointer().unwrap();
        injector.move_to(123, 234).unwrap();
        injector.flush().unwrap();
        assert_eq!(injector.pointer().unwrap(), (123, 234));
        injector.move_by(10, -4).unwrap();
        assert_eq!(injector.pointer().unwrap(), (133, 230));
        println!(
            "caps lock on: {}",
            injector.toggled(super::Lock::Caps).unwrap()
        );
        println!("foreground pid: {:?}", injector.foreground());
        injector.move_to(before.0, before.1).unwrap();
        injector.flush().unwrap();
    }

    /// Types OPENUUYC_TYPE_TEXT into whatever has focus on DISPLAY.
    #[test]
    #[ignore = "types into the focused window of an X display"]
    fn types_text() {
        let text = std::env::var("OPENUUYC_TYPE_TEXT").unwrap();
        super::Injector::open()
            .unwrap()
            .text(&text, || true)
            .unwrap();
    }
}
