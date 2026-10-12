//! Host GPU scheduling, scoped to active capture/encoding owners.
//! SDK 4.7.3 BB3190 -> 9A1E90(0x26) -> 99EC10 requests process GPU
//! priority as well as the device's relative priority. Decoder-only setup
//! does not cover the host's separate D3D devices or service process.
use std::sync::Mutex;
use windows::{
    Win32::{
        Foundation::HANDLE,
        Graphics::{Direct3D11::ID3D11Device, Dxgi::IDXGIDevice},
        System::Threading::GetCurrentProcess,
    },
    core::Interface,
};

#[link(name = "gdi32")]
unsafe extern "system" {
    fn D3DKMTGetProcessSchedulingPriorityClass(process: HANDLE, priority: *mut i32) -> i32;
    fn D3DKMTSetProcessSchedulingPriorityClass(process: HANDLE, priority: i32) -> i32;
}

#[derive(Default)]
struct State {
    users: usize,
    original: Option<i32>,
    changed: bool,
}
static PROCESS: Mutex<State> = Mutex::new(State {
    users: 0,
    original: None,
    changed: false,
});

pub(super) fn configure_device(device: &ID3D11Device) {
    let result = device.cast::<IDXGIDevice>().and_then(|device| unsafe {
        device.SetGPUThreadPriority(7)?;
        device.GetGPUThreadPriority()
    });
    match result {
        Ok(priority) => tracing::debug!(priority, "host D3D11 GPU scheduling configured"),
        Err(error) => tracing::warn!(%error, "host D3D11 GPU scheduling unavailable"),
    }
}

/// All captures/encoders in this process share one lease count. Ending one
/// screen must not restore the priority while another screen is still active.
pub(super) struct Lease {
    _private: (),
}
impl Lease {
    pub fn new() -> Self {
        let mut state = super::lock(&PROCESS);
        if state.users == 0 {
            // Retain the original value after a failed restoration, so a later
            // capture cannot accidentally adopt our own boost as its baseline.
            if state.original.is_none() {
                let mut priority = 0;
                let status = unsafe {
                    D3DKMTGetProcessSchedulingPriorityClass(GetCurrentProcess(), &mut priority)
                };
                if status >= 0 {
                    state.original = Some(priority);
                } else {
                    tracing::warn!(status, "host GPU process priority query failed");
                }
            }
            // HIGH is also the official NVIDIA+HAGS policy. Use it for host
            // work without REALTIME's starvation risk or changing global HAGS,
            // CPU priority, privileges, game settings, or encoder quality.
            if let Some(original) = state.original {
                let status =
                    unsafe { D3DKMTSetProcessSchedulingPriorityClass(GetCurrentProcess(), 4) };
                if status >= 0 {
                    state.changed = original != 4;
                    tracing::info!(
                        original,
                        priority = 4,
                        "host GPU process scheduling acquired"
                    );
                } else {
                    tracing::warn!(
                        status,
                        "host GPU process scheduling denied; retaining current priority"
                    );
                }
            }
        }
        state.users += 1;
        Self { _private: () }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = super::lock(&PROCESS);
        state.users -= 1;
        if state.users != 0 {
            return;
        }
        if state.changed
            && let Some(original) = state.original
        {
            let status =
                unsafe { D3DKMTSetProcessSchedulingPriorityClass(GetCurrentProcess(), original) };
            if status < 0 {
                tracing::warn!(
                    status,
                    original,
                    "host GPU process scheduling restoration failed"
                );
                return;
            }
            tracing::info!(priority = original, "host GPU process scheduling restored");
        }
        state.original = None;
        state.changed = false;
    }
}
