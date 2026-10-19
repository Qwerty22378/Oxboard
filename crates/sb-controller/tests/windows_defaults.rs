//! Windows' default output on the real audio stack: read it, move every role to the other
//! output through `IPolicyConfig`, see Windows report it, and put the user's back.
#![cfg(windows)]

use sb_controller::defaults::DefaultsMap;
use sb_controller::state::Role;
use sb_controller::win::defaults;
use sb_shared::ui::DeviceKey;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree};

#[test]
fn the_default_output_moves_and_comes_back() {
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let en: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.unwrap();
    let before = defaults::read(&en);
    println!("before: {before:?}");
    assert_eq!(before.len(), 3, "every role has a default");

    // Another active output.
    let other = unsafe {
        let all = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE).unwrap();
        (0..all.GetCount().unwrap())
            .map(|i| {
                let p = all.Item(i).unwrap().GetId().unwrap();
                let id = p.to_string().unwrap();
                CoTaskMemFree(Some(p.0 as *const _));
                DeviceKey(id)
            })
            .find(|k| Some(k) != before.get(&Role::Console))
            .expect("this test needs two outputs")
    };
    let all_other: DefaultsMap = Role::ALL.iter().map(|r| (*r, other.clone())).collect();
    defaults::set(&en, &all_other).unwrap();
    assert_eq!(defaults::read(&en), all_other, "Windows has ours");

    defaults::set(&en, &before).unwrap();
    assert_eq!(defaults::read(&en), before, "and the user's again");

    // A device that isn't there is skipped, not an error.
    let mut gone = before.clone();
    gone.insert(
        Role::Communications,
        DeviceKey("{0.0.0.00000000}.{not-a-device}".into()),
    );
    defaults::set(&en, &gone).unwrap();
    assert_eq!(defaults::read(&en), before);
}
