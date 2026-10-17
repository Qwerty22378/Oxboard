//! Read or set a process's per-app output (Windows' `IAudioPolicyConfigFactory`).
//!
//!   app_output PID                  print it
//!   app_output PID DEVICE-ID        send the process there ({0.0.0.00000000}.{guid})
//!   app_output PID default          back to the Windows default

#[cfg(not(windows))]
fn main() {
    eprintln!("per-app outputs are a Windows thing");
}

#[cfg(windows)]
fn main() {
    use sb_controller::win::policy::{Policy, build};
    use sb_shared::ui::DeviceKey;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let _ =
        unsafe { windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED) };
    println!("Windows build {:?}", build());
    let policy = match Policy::new() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    let Some(pid) = args.first().and_then(|p| p.parse().ok()) else {
        eprintln!("usage: app_output PID [DEVICE-ID|default]");
        std::process::exit(2);
    };
    if let Some(target) = args.get(1) {
        let device = (target != "default").then(|| DeviceKey(target.clone()));
        match policy.set(pid, device.as_ref()) {
            Ok(()) => println!("set"),
            Err(e) => println!("set failed: {e}"),
        }
    }
    match policy.get(pid) {
        Ok(d) => println!("pid {pid}: {d:?}"),
        Err(e) => println!("get failed: {e}"),
    }
}
