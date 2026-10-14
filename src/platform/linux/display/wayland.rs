//! The compositor's own output layout.
//!
//! It is not the X11 one, and that matters because it is what an absolute
//! pointer position is resolved against: a compositor reads the pointer
//! device's absolute axes as a fraction of this layout, so a host that aims
//! with X11 rectangles hits a different point than the one it asked for.
//!
//! XWayland cannot be asked instead. It sizes every output in that output's own
//! pixels while it places it at the compositor's logical position, so a
//! fractional scale makes the two disagree, and `wl_output.scale` is no help:
//! the protocol rounds it to a whole number (2 for both outputs here, where the
//! scales are 1.6 and 1.35). `xdg_output` is the only place the logical
//! position and size are stated.
use crate::platform::CompositorOutput;
use anyhow::{Context, Result, bail};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};

/// `wl_output.name` arrived at 4; older revisions are matched by the name their
/// `xdg_output` reports instead.
const OUTPUT_VERSION: u32 = 4;

#[derive(Default)]
struct State {
    manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    entries: Vec<Entry>,
}

struct Entry {
    proxy: wl_output::WlOutput,
    name: Option<String>,
    /// Asked for once the manager is known: the registry announces the two in
    /// whatever order the compositor lists its globals.
    described: Option<zxdg_output_v1::ZxdgOutputV1>,
    logical: (i32, i32, u32, u32),
}

/// Every output, in the compositor's global space.
pub(crate) fn layout() -> Result<Vec<CompositorOutput>> {
    let connection =
        Connection::connect_to_env().context("没有可用的 Wayland 会话（WAYLAND_DISPLAY）")?;
    let mut queue = connection.new_event_queue::<State>();
    let handle = queue.handle();
    connection.display().get_registry(&handle, ());
    let mut state = State::default();
    queue
        .roundtrip(&mut state)
        .context("读取合成器全局对象失败")?;
    let Some(manager) = state.manager.clone() else {
        bail!("合成器没有提供 zxdg_output_manager_v1，无法得知输出的逻辑位置");
    };
    for entry in &mut state.entries {
        entry.described = Some(manager.get_xdg_output(&entry.proxy, &handle, ()));
    }
    // The compositor answers a description from its own event loop, so the
    // logical geometry arrives one round trip after it was asked for.
    queue.roundtrip(&mut state).context("读取输出描述失败")?;
    Ok(state
        .entries
        .into_iter()
        .filter_map(|entry| {
            let (left, top, width, height) = entry.logical;
            let name = entry.name?;
            (width > 0 && height > 0).then_some(CompositorOutput {
                name,
                left,
                top,
                width,
                height,
            })
        })
        .collect())
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "zxdg_output_manager_v1" => {
                    state.manager = Some(registry.bind(name, version.min(3), handle, ()));
                }
                "wl_output" => state.entries.push(Entry {
                    proxy: registry.bind(name, version.min(OUTPUT_VERSION), handle, ()),
                    name: None,
                    described: None,
                    logical: (0, 0, 0, 0),
                }),
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(entry) = state
            .entries
            .iter_mut()
            .find(|entry| entry.proxy == *output)
        else {
            return;
        };
        if let wl_output::Event::Name { name } = event {
            entry.name = Some(name);
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, ()> for State {
    fn event(
        state: &mut Self,
        description: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(entry) = state
            .entries
            .iter_mut()
            .find(|entry| entry.described.as_ref() == Some(description))
        else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                entry.logical.0 = x;
                entry.logical.1 = y;
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                entry.logical.2 = width.max(0) as u32;
                entry.logical.3 = height.max(0) as u32;
            }
            zxdg_output_v1::Event::Name { name } => entry.name = Some(name),
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(State: ignore zxdg_output_manager_v1::ZxdgOutputManagerV1);
