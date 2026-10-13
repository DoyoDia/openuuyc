//! Keeps the compositor from acting on the shortcuts a remote desktop needs.
//!
//! "Intercept shortcuts" is the viewer's promise that a key it forwards stops
//! here, and on X11 that is the whole story: the window manager sees the
//! window, the client sees the key. On Wayland it is not — the compositor
//! takes `Super+O` and the overview opens before the remote ever hears about
//! it. `zwp_keyboard_shortcuts_inhibit_v1` is the compositor's own answer: a
//! window that holds an inhibitor gets its keys, and the compositor's own
//! bindings stand down while that window has the keyboard.
//!
//! The connection here is winit's, borrowed rather than opened: the protocol
//! is per surface and per seat, and the surface belongs to winit's connection.
//! Nothing is taken over — the surface proxy is built from the pointer winit
//! already owns, so its events keep going where they went before.
use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy as _, QueueHandle};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1 as manager,
    zwp_keyboard_shortcuts_inhibitor_v1 as inhibitor,
};
use winit::raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
};

pub(super) struct Shortcuts {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
    surface: wl_surface::WlSurface,
    seat: wl_seat::WlSeat,
    manager: manager::ZwpKeyboardShortcutsInhibitManagerV1,
    inhibitor: Option<inhibitor::ZwpKeyboardShortcutsInhibitorV1>,
}

#[derive(Default)]
struct State {
    /// What the compositor last said about the inhibitor. A compositor that
    /// refuses leaves the object inert, which is the same as having none, so
    /// this is only kept so the events do not pile up unread.
    active: Option<bool>,
}

impl Shortcuts {
    /// `None` when this is not a Wayland window, or the compositor does not
    /// serve the protocol — in which case the shortcut belongs to the desktop
    /// and there is nothing to be done about it.
    pub(super) fn new(window: &winit::window::Window) -> Option<Self> {
        let display = match window.display_handle().ok()?.as_raw() {
            RawDisplayHandle::Wayland(handle) => handle.display.as_ptr(),
            _ => return None,
        };
        let surface = match window.window_handle().ok()?.as_raw() {
            RawWindowHandle::Wayland(handle) => handle.surface.as_ptr(),
            _ => return None,
        };
        // Safe because the display belongs to the event loop, which outlives
        // every window, and this connection only borrows it.
        let backend =
            unsafe { wayland_client::backend::Backend::from_foreign_display(display.cast()) };
        let connection = Connection::from_backend(backend);
        // Safe because winit created this surface and the window is still
        // alive; the proxy is a second reference to the object, not a new one.
        let id = unsafe { ObjectId::from_ptr(wl_surface::WlSurface::interface(), surface.cast()) }
            .ok()?;
        let surface = wl_surface::WlSurface::from_id(&connection, id).ok()?;
        let (globals, queue) = registry_queue_init::<State>(&connection).ok()?;
        let handle = queue.handle();
        let manager: manager::ZwpKeyboardShortcutsInhibitManagerV1 =
            globals.bind(&handle, 1..=1, ()).ok()?;
        let seat: wl_seat::WlSeat = globals.bind(&handle, 1..=1, ()).ok()?;
        Some(Self {
            connection,
            queue,
            state: State::default(),
            surface,
            seat,
            manager,
            inhibitor: None,
        })
    }

    /// Holds the compositor's shortcuts back, or lets them go again. Being
    /// called with the state it is already in costs nothing.
    pub(super) fn set_active(&mut self, active: bool) {
        let _ = self.queue.dispatch_pending(&mut self.state);
        if active == self.inhibitor.is_some() {
            return;
        }
        if active {
            self.state.active = None;
            let handle = self.queue.handle();
            self.inhibitor =
                Some(
                    self.manager
                        .inhibit_shortcuts(&self.surface, &self.seat, &handle, ()),
                );
        } else if let Some(inhibitor) = self.inhibitor.take() {
            inhibitor.destroy();
        }
        let _ = self.connection.flush();
    }
}

impl Drop for Shortcuts {
    fn drop(&mut self) {
        if let Some(inhibitor) = self.inhibitor.take() {
            inhibitor.destroy();
        }
        let _ = self.connection.flush();
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

wayland_client::delegate_noop!(State: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(State: ignore manager::ZwpKeyboardShortcutsInhibitManagerV1);

impl Dispatch<inhibitor::ZwpKeyboardShortcutsInhibitorV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &inhibitor::ZwpKeyboardShortcutsInhibitorV1,
        event: inhibitor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            inhibitor::Event::Active => state.active = Some(true),
            inhibitor::Event::Inactive => state.active = Some(false),
            _ => {}
        }
    }
}
