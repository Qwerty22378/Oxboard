//! Per-app default outputs, through Windows' undocumented `IAudioPolicyConfigFactory`: what
//! Settings → "App volume and device preferences" (and EarTrumpet) use.
//!
//! The interface isn't in any SDK. Its IID changed in build 21390 (Windows 11 / 21H2); the
//! methods used here kept their places. They're called through the vtable by index, after
//! the 6 `IInspectable` methods and 19 the controller doesn't use:
//! - 25 `SetPersistedDefaultAudioEndpoint(pid, flow, role, HSTRING device)`;
//! - 26 `GetPersistedDefaultAudioEndpoint(pid, flow, role, out HSTRING)`.
//!
//! The device is named by its device interface path, not its endpoint ID (see
//! [`crate::watch::policy_path`]), and an empty one clears the app's default. The setting is
//! per process, but Windows remembers it for the executable. An app only moves when it opens
//! a new stream.
//!
//! If the factory can't be made (an older Windows, or a future one that changed the IID)
//! routing is reported unsupported ([`Policy::new`] fails), never guessed at.

use crate::watch;
use sb_shared::ui::DeviceKey;
use std::ffi::c_void;
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows::Win32::System::WinRT::RoGetActivationFactory;
use windows::core::{GUID, HRESULT, HSTRING, IInspectable, IUnknown, Interface, w};

/// From build 21390 on.
const IID_21H2: GUID = GUID::from_u128(0xab3d4648_e242_459f_b02f_541c70306324);
/// Builds 17134 (1803) to 21389.
const IID_DOWNLEVEL: GUID = GUID::from_u128(0x2a59116d_6c4f_45e0_a74f_707e3fef9258);

const SET_PERSISTED: usize = 25;
const GET_PERSISTED: usize = 26;

const E_RENDER: i32 = 0;
/// `eConsole` and `eMultimedia`, as the Settings page sets them.
const ROLES: [i32; 2] = [0, 1];

type SetFn = unsafe extern "system" fn(*mut c_void, u32, i32, i32, *mut c_void) -> HRESULT;
type GetFn = unsafe extern "system" fn(*mut c_void, u32, i32, i32, *mut *mut c_void) -> HRESULT;

pub struct Policy {
    factory: IUnknown,
}

/// Windows' build number (`CurrentBuildNumber`).
pub fn build() -> Option<u32> {
    let mut buf = [0u16; 32];
    let mut len = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
            w!("CurrentBuildNumber"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    if r.is_err() {
        return None;
    }
    let units = (len as usize / 2).min(buf.len());
    String::from_utf16_lossy(&buf[..units])
        .trim_end_matches('\0')
        .parse()
        .ok()
}

impl Policy {
    /// The factory for this Windows build. Needs COM (or WinRT) on the calling thread.
    pub fn new() -> Result<Self, String> {
        let build = build().ok_or("unknown Windows build")?;
        let iid = match build {
            21390.. => IID_21H2,
            17134.. => IID_DOWNLEVEL,
            _ => return Err(format!("Windows build {build} predates per-app outputs")),
        };
        let any: IInspectable =
            unsafe { RoGetActivationFactory(&HSTRING::from("Windows.Media.Internal.AudioPolicyConfig")) }
                .map_err(|e| format!("AudioPolicyConfig: {e}"))?;
        let mut raw = std::ptr::null_mut();
        unsafe { any.query(&iid, &mut raw) }
            .ok()
            .map_err(|e| format!("IAudioPolicyConfigFactory on build {build}: {e}"))?;
        Ok(Self {
            factory: unsafe { IUnknown::from_raw(raw) },
        })
    }

    fn method(&self, index: usize) -> *const c_void {
        unsafe {
            let vtable = *(self.factory.as_raw() as *const *const *const c_void);
            *vtable.add(index)
        }
    }

    /// Send the process's output to `device`, or back to the default (`None`).
    pub fn set(&self, pid: u32, device: Option<&DeviceKey>) -> Result<(), String> {
        let path = device.map(|d| HSTRING::from(watch::policy_path(d))).unwrap_or_default();
        let set: SetFn = unsafe { std::mem::transmute(self.method(SET_PERSISTED)) };
        for role in ROLES {
            // An HSTRING is one pointer (null for empty); the callee doesn't take ownership.
            let raw: *mut c_void = unsafe { std::mem::transmute_copy(&path) };
            unsafe { set(self.factory.as_raw(), pid, E_RENDER, role, raw) }
                .ok()
                .map_err(|e| format!("pid {pid}: {e}"))?;
        }
        Ok(())
    }

    /// The process's per-app output, if it has one (multimedia role).
    pub fn get(&self, pid: u32) -> Result<Option<DeviceKey>, String> {
        let get: GetFn = unsafe { std::mem::transmute(self.method(GET_PERSISTED)) };
        let mut out = HSTRING::new();
        unsafe {
            get(
                self.factory.as_raw(),
                pid,
                E_RENDER,
                ROLES[1],
                (&mut out as *mut HSTRING).cast(),
            )
        }
        .ok()
        .map_err(|e| format!("pid {pid}: {e}"))?;
        Ok(watch::device_of_policy_path(&out.to_string()))
    }
}
