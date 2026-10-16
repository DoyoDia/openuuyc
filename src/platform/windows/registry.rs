//! Windows 注册表的小工具：HKCU 下的键、字符串值的读写。
//!
//! 通知身份和 `uuremote://` 协议注册都用同一套写法——建键、写默认值、
//! 记下自己的归属标记，卸载时只删自己写下的那一棵树。
use anyhow::{Context, Result};
use windows::{
    Win32::{
        Foundation::*,
        System::Registry::*,
    },
    core::PCWSTR,
};

pub(super) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// 一个打开的键；句柄随值一起关掉。
pub(super) struct Key(pub(super) HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

pub(super) fn key(path: &str) -> Result<Key> {
    let mut h = HKEY::default();
    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide(path).as_ptr()),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_READ | KEY_WRITE,
            None,
            &mut h,
            None,
        )
        .ok()?;
    }
    Ok(Key(h))
}

pub(super) fn set(h: HKEY, name: &str, value: &str) -> Result<()> {
    let value = wide(value);
    unsafe {
        RegSetValueExW(
            h,
            PCWSTR(wide(name).as_ptr()),
            None,
            REG_SZ,
            Some(std::slice::from_raw_parts(
                value.as_ptr().cast(),
                value.len() * 2,
            )),
        )
        .ok()?;
    }
    Ok(())
}

/// 读一个字符串值；键或值不存在都算没有。
pub(super) fn read(path: &str, name: &str) -> Result<Option<String>> {
    let mut value = vec![0u16; 32768];
    let mut bytes = (value.len() * 2) as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(wide(path).as_ptr()),
            PCWSTR(wide(name).as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(value.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    };
    if result == ERROR_FILE_NOT_FOUND || result == ERROR_PATH_NOT_FOUND {
        return Ok(None);
    }
    result.ok()?;
    let end = value
        .iter()
        .position(|&v| v == 0)
        .context("注册表字符串值无效")?;
    Ok(Some(String::from_utf16(&value[..end])?))
}
