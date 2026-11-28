//! Thin Win32 helpers. Each replaces a plugin/crate with a few direct calls.

use std::ptr::{null, null_mut};
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{LocalFree, HWND};
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};
use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CLASSES_ROOT, HKEY_CURRENT_USER, REG_SZ, RRF_RT_ANY,
    RRF_RT_REG_SZ,
};
use windows_sys::Win32::System::StationsAndDesktops::{CloseDesktop, OpenInputDesktop, SwitchDesktop, DESKTOP_SWITCHDESKTOP};
use windows_sys::Win32::System::SystemInformation::GetTickCount;
use windows_sys::Win32::Globalization::GetUserDefaultLocaleName;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, IDYES, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MB_YESNO, SW_SHOWNORMAL};

pub static IS_FR: OnceLock<bool> = OnceLock::new();

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn user_locale_is_fr() -> bool {
    let mut buf = [0u16; 85];
    let n = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
    n > 0 && String::from_utf16_lossy(&buf[..(n as usize - 1)]).to_lowercase().starts_with("fr")
}

/// Seconds since the last keyboard/mouse input in this session.
pub fn idle_seconds() -> u64 {
    let mut li = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    if unsafe { GetLastInputInfo(&mut li) } == 0 {
        return 0;
    }
    (unsafe { GetTickCount() }.wrapping_sub(li.dwTime) / 1000) as u64
}

/// Locked workstation (or secure desktop): the input desktop can't be switched to.
pub fn session_locked() -> bool {
    unsafe {
        let h = OpenInputDesktop(0, 0, DESKTOP_SWITCHDESKTOP);
        if h.is_null() {
            return true;
        }
        let ok = SwitchDesktop(h);
        CloseDesktop(h);
        ok == 0
    }
}

/// DPAPI, current-user scope: the same protection Chromium gives its cookie key.
pub fn protect(data: &[u8], encrypt: bool) -> Option<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
    let ok = unsafe {
        if encrypt {
            CryptProtectData(&input, null(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        } else {
            CryptUnprotectData(&input, null_mut(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        }
    };
    if ok == 0 || out.pbData.is_null() {
        return None;
    }
    let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec() };
    unsafe { LocalFree(out.pbData as _) };
    Some(v)
}

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "Claude Usage Meter";

pub fn login_item_enabled() -> bool {
    let (k, n) = (w(RUN_KEY), w(RUN_NAME));
    unsafe { RegGetValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), RRF_RT_REG_SZ, null_mut(), null_mut(), null_mut()) == 0 }
}

pub fn set_login_item(enabled: bool) {
    let (k, n) = (w(RUN_KEY), w(RUN_NAME));
    unsafe {
        if enabled {
            let Ok(exe) = std::env::current_exe() else { return };
            let v = w(&format!("\"{}\" --hidden", exe.display()));
            RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr(), REG_SZ, v.as_ptr() as _, (v.len() * 2) as u32);
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n.as_ptr());
        }
    }
}

/// claude:// registered (Claude Desktop installed)? Avoids Windows' "find an app" prompt.
pub fn protocol_registered(scheme: &str) -> bool {
    let (k, n) = (w(scheme), w("URL Protocol"));
    unsafe { RegGetValueW(HKEY_CLASSES_ROOT, k.as_ptr(), n.as_ptr(), RRF_RT_ANY, null_mut(), null_mut(), null_mut()) == 0 }
}

pub fn copy_text(text: &str) -> bool {
    let v = w(text);
    unsafe {
        if OpenClipboard(null_mut()) == 0 {
            return false;
        }
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, v.len() * 2);
        let mut ok = false;
        if !h.is_null() {
            let p = GlobalLock(h) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(v.as_ptr(), p, v.len());
                GlobalUnlock(h);
                ok = !SetClipboardData(13 /* CF_UNICODETEXT */, h).is_null();
            }
        }
        CloseClipboard();
        ok
    }
}

/// Opens a URL with its registered handler. False when nothing handles it.
pub fn open(url: &str) -> bool {
    let (op, u) = (w("open"), w(url));
    let r = unsafe { ShellExecuteW(null_mut() as HWND, op.as_ptr(), u.as_ptr(), null(), null(), SW_SHOWNORMAL) };
    r as isize > 32
}

/// Blocking message box; call from a spare thread, not the event loop.
pub fn message(title: &str, text: &str, yes_no: bool) -> bool {
    let (t, b) = (w(title), w(text));
    let flags = MB_ICONINFORMATION | MB_SETFOREGROUND | if yes_no { MB_YESNO } else { MB_OK };
    unsafe { MessageBoxW(null_mut(), b.as_ptr(), t.as_ptr(), flags) == IDYES }
}

#[cfg(test)]
mod tests {
    #[test]
    fn dpapi_round_trip() {
        let enc = super::protect(b"secret", true).unwrap();
        assert_ne!(enc, b"secret");
        assert_eq!(super::protect(&enc, false).unwrap(), b"secret");
    }
}
