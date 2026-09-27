//! OpenUUYC's private driver interface. No UU driver names or protocol are used.
use anyhow::{Result, ensure};
use windows::{
    Win32::{
        Foundation::*,
        Storage::FileSystem::*,
        System::{IO::*, Threading::*},
    },
    core::w,
};

const BEGIN: u32 = 0x22a000;
const SUBMIT: u32 = 0x22a004;
const RESET: u32 = 0x22a008;
const ALIVE: u32 = 0x22a00c;
pub(crate) const MAGIC: u32 = 0x4f55494e;
pub(crate) const VERSION: u32 = 1;

pub(crate) struct Device {
    handle: HANDLE,
    epoch: u64,
    sequence: u64,
    alive: std::time::Instant,
}
impl Device {
    pub fn open() -> Result<Option<Self>> {
        let handle = match unsafe {
            CreateFileW(
                w!(r"\\.\OpenUUYCInput"),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
        } {
            Ok(h) => h,
            Err(e)
                if e.code() == ERROR_FILE_NOT_FOUND.to_hresult()
                    || e.code() == ERROR_PATH_NOT_FOUND.to_hresult() =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        let mut device = Self {
            handle,
            epoch: rand::random::<u64>().max(1),
            sequence: 0,
            alive: std::time::Instant::now(),
        };
        let mut bytes = [0u8; 16];
        bytes[..4].copy_from_slice(&MAGIC.to_le_bytes());
        bytes[4..8].copy_from_slice(&VERSION.to_le_bytes());
        bytes[8..].copy_from_slice(&device.epoch.to_le_bytes());
        device.ioctl(BEGIN, &bytes)?;
        Ok(Some(device))
    }
    fn ioctl(&mut self, code: u32, bytes: &[u8]) -> Result<()> {
        let mut count = 0;
        let event = super::super::host_service::pipe::Handle(unsafe {
            CreateEventW(None, true, false, None)?
        });
        let mut overlapped = OVERLAPPED {
            hEvent: event.0,
            ..Default::default()
        };
        unsafe {
            let result = DeviceIoControl(
                self.handle,
                code,
                Some(bytes.as_ptr().cast()),
                bytes.len() as u32,
                None,
                0,
                Some(&mut count),
                Some(&mut overlapped),
            );
            match result {
                Ok(()) => {}
                Err(error) if error.code() == ERROR_IO_PENDING.to_hresult() => {
                    if WaitForSingleObject(event.0, 500) != WAIT_OBJECT_0 {
                        let _ = CancelIoEx(self.handle, Some(&overlapped));
                        let _ = GetOverlappedResult(self.handle, &overlapped, &mut count, true);
                        anyhow::bail!("输入驱动请求超时，未重放输入");
                    }
                    GetOverlappedResult(self.handle, &overlapped, &mut count, false)?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
    pub fn report(&mut self, data: &[u8]) -> Result<()> {
        ensure!(data.len() <= 256 && !data.is_empty(), "HID报告长度无效");
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("输入序号已耗尽"))?;
        let mut bytes = Vec::with_capacity(28 + data.len());
        bytes.extend(MAGIC.to_le_bytes());
        bytes.extend(VERSION.to_le_bytes());
        bytes.extend(self.epoch.to_le_bytes());
        bytes.extend(self.sequence.to_le_bytes());
        bytes.extend((data.len() as u32).to_le_bytes());
        bytes.extend(data);
        self.ioctl(SUBMIT, &bytes)
    }
    pub fn reset(&mut self) -> Result<()> {
        self.ioctl(RESET, &self.epoch.to_le_bytes())
    }
    pub fn tick(&mut self) -> Result<()> {
        if self.alive.elapsed() >= std::time::Duration::from_millis(100) {
            self.ioctl(ALIVE, &self.epoch.to_le_bytes())?;
            self.alive = std::time::Instant::now();
        }
        Ok(())
    }
}
impl Drop for Device {
    fn drop(&mut self) {
        let _ = self.reset();
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Set-1 scan code to HID keyboard usage. Layout resolution occurs before this
/// mapping; media keys have their own consumer collection.
pub(crate) fn usage(vk: u16) -> Option<u8> {
    use windows::Win32::UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*};
    let scan = unsafe {
        MapVirtualKeyExW(
            u32::from(vk),
            MAPVK_VK_TO_VSC_EX,
            Some(GetKeyboardLayout(GetWindowThreadProcessId(
                GetForegroundWindow(),
                None,
            ))),
        )
    };
    if scan & 0xff00 == 0xe000 {
        return Some(match scan & 255 {
            0x1c => 0x58,
            0x1d => 0xe4,
            0x35 => 0x54,
            0x37 => 0x46,
            0x38 => 0xe6,
            0x47 => 0x4a,
            0x48 => 0x52,
            0x49 => 0x4b,
            0x4b => 0x50,
            0x4d => 0x4f,
            0x4f => 0x4d,
            0x50 => 0x51,
            0x51 => 0x4e,
            0x52 => 0x49,
            0x53 => 0x4c,
            0x5b => 0xe3,
            0x5c => 0xe7,
            0x5d => 0x65,
            _ => return None,
        });
    }
    if vk == 0x13 {
        return Some(0x48);
    } // Pause has an E1 sequence.
    if vk == 0x90 {
        return Some(0x53);
    }
    const SET1: [u8; 89] = [
        0, 0x29, 0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x2d, 0x2e, 0x2a,
        0x2b, 0x14, 0x1a, 0x08, 0x15, 0x17, 0x1c, 0x18, 0x0c, 0x12, 0x13, 0x2f, 0x30, 0x28, 0xe0,
        0x04, 0x16, 0x07, 0x09, 0x0a, 0x0b, 0x0d, 0x0e, 0x0f, 0x33, 0x34, 0x35, 0xe1, 0x31, 0x1d,
        0x1b, 0x06, 0x19, 0x05, 0x11, 0x10, 0x36, 0x37, 0x38, 0xe5, 0x55, 0xe2, 0x2c, 0x39, 0x3a,
        0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0x40, 0x41, 0x42, 0x43, 0x53, 0x47, 0x5f, 0x60, 0x61, 0x56,
        0x5c, 0x5d, 0x5e, 0x57, 0x59, 0x5a, 0x5b, 0x62, 0x63, 0, 0, 0x64, 0x44, 0x45,
    ];
    if (0x7c..=0x87).contains(&vk) {
        return Some(0x68 + (vk - 0x7c) as u8);
    }
    SET1.get((scan & 255) as usize).copied().filter(|&n| n != 0)
}
pub(crate) fn consumer(vk: u16) -> Option<u8> {
    Some(match vk {
        0xb0 => 0,
        0xb1 => 1,
        0xb3 => 2,
        0xb2 => 3,
        0xad => 4,
        0xaf => 5,
        0xae => 6,
        _ => return None,
    })
}
