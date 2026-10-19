//! The Windows file dialog, for adding clips. Modal over the overlay (its owner), so the
//! overlay stays up while it's open (see the focus rule in `win32.rs`).

use std::path::PathBuf;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH,
};
use windows::core::w;

/// The audio files the user picked (none if they cancelled).
pub fn pick_audio_files(owner: HWND) -> Vec<PathBuf> {
    unsafe {
        let Ok(dialog) = CoCreateInstance::<_, IFileOpenDialog>(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) else {
            return Vec::new();
        };
        let filters = [
            COMDLG_FILTERSPEC {
                pszName: w!("Audio files"),
                pszSpec: w!("*.ogg;*.mp3;*.wav;*.flac"),
            },
            COMDLG_FILTERSPEC {
                pszName: w!("All files"),
                pszSpec: w!("*.*"),
            },
        ];
        let _ = dialog.SetFileTypes(&filters);
        let _ = dialog.SetTitle(w!("Add sounds to the soundboard"));
        if let Ok(o) = dialog.GetOptions() {
            let _ = dialog.SetOptions(o | FOS_ALLOWMULTISELECT | FOS_FILEMUSTEXIST);
        }
        if dialog.Show(Some(owner)).is_err() {
            return Vec::new(); // cancelled
        }
        let Ok(items) = dialog.GetResults() else {
            return Vec::new();
        };
        let n = items.GetCount().unwrap_or(0);
        (0..n)
            .filter_map(|i| {
                let item = items.GetItemAt(i).ok()?;
                let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
                let path = name.to_string().ok();
                CoTaskMemFree(Some(name.0 as *const _));
                path.map(PathBuf::from)
            })
            .collect()
    }
}
