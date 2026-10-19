//! Windows' default output, per role, with device names:
//!
//!   default_output              print them
//!   default_output DEVICE       make DEVICE (a name, by substring) the default for every role
//!
//! Handy beside `sb-controller` to see what it did with the defaults (M6).

#[cfg(windows)]
fn main() {
    use sb_controller::state::Role;
    use sb_controller::win::defaults;
    use sb_shared::ui::DeviceKey;
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, STGM_READ,
    };

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap();
        let all = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE).unwrap();
        let outputs: Vec<(DeviceKey, String)> = (0..all.GetCount().unwrap())
            .map(|i| {
                let d = all.Item(i).unwrap();
                let p = d.GetId().unwrap();
                let id = p.to_string().unwrap();
                CoTaskMemFree(Some(p.0 as *const _));
                let name = d
                    .OpenPropertyStore(STGM_READ)
                    .and_then(|s| s.GetValue(&PKEY_Device_FriendlyName))
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                (DeviceKey(id), name)
            })
            .collect();
        let name = |k: &DeviceKey| {
            outputs
                .iter()
                .find(|(o, _)| o == k)
                .map_or(k.0.clone(), |(_, n)| n.clone())
        };
        if let Some(want) = std::env::args().nth(1) {
            let Some((key, n)) = outputs
                .iter()
                .find(|(_, n)| n.to_lowercase().contains(&want.to_lowercase()))
            else {
                eprintln!("no output matches {want:?}");
                std::process::exit(1);
            };
            let map = Role::ALL.iter().map(|r| (*r, key.clone())).collect();
            defaults::set(&en, &map).unwrap();
            println!("default output: {n}");
            return;
        }
        for (role, key) in defaults::read(&en) {
            println!("{:<15} {}", role.name(), name(&key));
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Windows only");
}
