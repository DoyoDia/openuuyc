//! Handle-owned power availability for live capture and viewing sessions.
use anyhow::{Context, Result, ensure};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::{
            Power::{
                ES_DISPLAY_REQUIRED, PowerClearRequest, PowerCreateRequest,
                PowerRequestDisplayRequired, PowerRequestSystemRequired, PowerSetRequest,
                SetThreadExecutionState,
            },
            Threading::{POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0},
        },
    },
    core::PWSTR,
};

pub(crate) struct Request {
    handle: HANDLE,
    system: bool,
    display: bool,
}

// Power request handles are process-wide kernel objects, not thread execution
// state. Moving the unique owner between async workers preserves its counts.
unsafe impl Send for Request {}

impl Request {
    pub(crate) fn capture() -> Result<Self> {
        let request = Self::new("OpenUUYC 正在采集远程桌面")?;
        // Explicitly wake an already idle display before opening duplication.
        // No CONTINUOUS thread state to leak into a reused blocking worker, and
        // no synthetic input, away mode or change to the user's power plan.
        ensure!(
            unsafe { SetThreadExecutionState(ES_DISPLAY_REQUIRED) }.0 != 0,
            "唤醒采集显示失败"
        );
        Ok(request)
    }

    pub(crate) fn viewing() -> Result<Self> {
        Self::new("OpenUUYC 正在进行远程观看会话")
    }

    fn new(reason: &str) -> Result<Self> {
        let mut reason: Vec<u16> = reason.encode_utf16().chain(Some(0)).collect();
        let context = REASON_CONTEXT {
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 {
                SimpleReasonString: PWSTR(reason.as_mut_ptr()),
            },
            ..Default::default()
        };
        let mut request = Self {
            handle: unsafe { PowerCreateRequest(&context) }.context("创建会话电源请求失败")?,
            system: false,
            display: false,
        };
        // Handle-scoped requests compose across concurrent captures and source
        // replacement; releasing one cannot clear another thread's requirement.
        unsafe { PowerSetRequest(request.handle, PowerRequestSystemRequired) }
            .context("保持会话系统运行失败")?;
        request.system = true;
        unsafe { PowerSetRequest(request.handle, PowerRequestDisplayRequired) }
            .context("保持会话显示工作失败")?;
        request.display = true;
        tracing::debug!("session display/system power request acquired");
        Ok(request)
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        for (active, kind) in [
            (self.display, PowerRequestDisplayRequired),
            (self.system, PowerRequestSystemRequired),
        ] {
            if active && let Err(error) = unsafe { PowerClearRequest(self.handle, kind) } {
                tracing::warn!(%error, ?kind, "session power request release failed");
            }
        }
        // Closing the unique handle also covers partially initialized requests.
        if let Err(error) = unsafe { CloseHandle(self.handle) } {
            tracing::warn!(%error, "session power request handle close failed");
        }
        tracing::debug!("session display/system power request released");
    }
}
