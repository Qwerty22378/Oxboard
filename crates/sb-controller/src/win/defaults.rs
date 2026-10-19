//! Windows' default output, read with `IMMDeviceEnumerator` and set through the undocumented
//! `IPolicyConfig` (what the Sound control panel and tools like EarTrumpet use). Its IID and
//! method order have been stable since Windows 7; the method is called through the vtable by
//! index, like `policy.rs` does.

use crate::defaults::DefaultsMap;
use crate::state::Role;
use sb_shared::ui::DeviceKey;
use std::ffi::c_void;
use windows::Win32::Media::Audio::{ERole, IMMDeviceEnumerator, eCommunications, eConsole, eMultimedia, eRender};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};
use windows::core::{GUID, HRESULT, HSTRING, IUnknown, Interface};

const CLSID_POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
const IID_POLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);
/// After IUnknown's 3 and 10 format/period/share-mode/property methods.
const SET_DEFAULT_ENDPOINT: usize = 13;

type SetDefaultFn = unsafe extern "system" fn(*mut c_void, *const u16, ERole) -> HRESULT;

fn erole(r: Role) -> ERole {
    match r {
        Role::Console => eConsole,
        Role::Multimedia => eMultimedia,
        Role::Communications => eCommunications,
    }
}

/// The default render device for every role that has one.
pub fn read(en: &IMMDeviceEnumerator) -> DefaultsMap {
    Role::ALL
        .into_iter()
        .filter_map(|r| unsafe {
            let d = en.GetDefaultAudioEndpoint(eRender, erole(r)).ok()?;
            let p = d.GetId().ok()?;
            let id = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const _));
            Some((r, DeviceKey(id?)))
        })
        .collect()
}

/// Make each role's default the given device. Roles whose device isn't there any more are
/// skipped (Windows has picked another); the first other failure is reported.
pub fn set(en: &IMMDeviceEnumerator, defaults: &DefaultsMap) -> Result<(), String> {
    let config: IUnknown = unsafe { CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL) }
        .map_err(|e| format!("PolicyConfigClient: {e}"))?;
    let mut raw = std::ptr::null_mut();
    unsafe { config.query(&IID_POLICY_CONFIG, &mut raw) }
        .ok()
        .map_err(|e| format!("IPolicyConfig: {e}"))?;
    let policy = unsafe { IUnknown::from_raw(raw) };
    let set: SetDefaultFn = unsafe {
        let vtable = *(policy.as_raw() as *const *const *const c_void);
        std::mem::transmute(*vtable.add(SET_DEFAULT_ENDPOINT))
    };
    let mut first_error = None;
    for (role, device) in defaults {
        // Gone (unplugged since): nothing to give back to.
        if unsafe { en.GetDevice(&HSTRING::from(device.0.as_str())) }.is_err() {
            continue;
        }
        let id = HSTRING::from(device.0.as_str());
        let hr = unsafe { set(policy.as_raw(), id.as_ptr(), erole(*role)) };
        if let Err(e) = hr.ok() {
            first_error.get_or_insert(format!("{} → {}: {e}", role.name(), device.0));
        }
    }
    first_error.map_or(Ok(()), Err)
}
