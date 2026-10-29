use std::env;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr;
use std::time::{SystemTime, UNIX_EPOCH};

use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

pub const APP_NAME: &str = "Bibliotheque";

pub fn app_data_dir() -> PathBuf {
    let base = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join(APP_NAME);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn database_path() -> PathBuf {
    app_data_dir().join("library.db")
}

/// Lit directement dans le registre Windows (`AppsUseLightTheme`) sans allocation.
pub fn os_prefers_dark_mode() -> bool {
    // "Software\Microsoft\Windows\CurrentVersion\Themes\Personalize\0"
    const SUBKEY: &[u16] = &[
        b'S' as u16, b'o' as u16, b'f' as u16, b't' as u16, b'w' as u16, b'a' as u16, b'r' as u16, b'e' as u16, b'\\' as u16,
        b'M' as u16, b'i' as u16, b'c' as u16, b'r' as u16, b'o' as u16, b's' as u16, b'o' as u16, b'f' as u16, b't' as u16, b'\\' as u16,
        b'W' as u16, b'i' as u16, b'n' as u16, b'd' as u16, b'o' as u16, b'w' as u16, b's' as u16, b'\\' as u16,
        b'C' as u16, b'u' as u16, b'r' as u16, b'r' as u16, b'e' as u16, b'n' as u16, b't' as u16, b'V' as u16, b'e' as u16, b'r' as u16, b's' as u16, b'i' as u16, b'o' as u16, b'n' as u16, b'\\' as u16,
        b'T' as u16, b'h' as u16, b'e' as u16, b'm' as u16, b'e' as u16, b's' as u16, b'\\' as u16,
        b'P' as u16, b'e' as u16, b'r' as u16, b's' as u16, b'o' as u16, b'n' as u16, b'a' as u16, b'l' as u16, b'i' as u16, b'z' as u16, b'e' as u16, 0,
    ];
    // "AppsUseLightTheme\0"
    const VALUE: &[u16] = &[
        b'A' as u16, b'p' as u16, b'p' as u16, b's' as u16, b'U' as u16, b's' as u16, b'e' as u16,
        b'L' as u16, b'i' as u16, b'g' as u16, b'h' as u16, b't' as u16, b'T' as u16, b'h' as u16, b'e' as u16, b'm' as u16, b'e' as u16, 0,
    ];

    let mut data: u32 = 1;
    let mut size: u32 = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            SUBKEY.as_ptr(),
            VALUE.as_ptr(),
            RRF_RT_REG_DWORD,
            ptr::null_mut(),
            (&mut data as *mut u32).cast(),
            &mut size,
        )
    };
    status == 0 && data == 0
}

pub fn standard_locations() -> Vec<PathBuf> {
    let mut locations = Vec::with_capacity(6);
    if let Some(home) = env::var_os("USERPROFILE").map(PathBuf::from) {
        locations.push(home.join("Desktop"));
        locations.push(home.join("Downloads"));
    }
    let pf = env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
    locations.push(pf);

    let pf86 = env::var_os("ProgramFiles(x86)")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files (x86)"));
    locations.push(pf86);

    if let Some(appdata) = env::var_os("APPDATA").map(PathBuf::from) {
        locations.push(appdata.join(r"Microsoft\Windows\Start Menu\Programs"));
    }
    let progdata = env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    locations.push(progdata.join(r"Microsoft\Windows\Start Menu\Programs"));

    locations.into_iter().filter(|p| p.exists()).collect()
}

pub fn to_wide_null(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// Ouvre un exécutable ou raccourci `.lnk` via le Shell Windows (équivalent natif de `os.startfile`).
pub fn open_file(path: &str) -> Result<(), String> {
    const OPEN_VERB: &[u16] = &[b'o' as u16, b'p' as u16, b'e' as u16, b'n' as u16, 0];
    let file_wide = to_wide_null(path);
    let working_dir = Path::new(path)
        .parent()
        .and_then(|p| p.to_str())
        .map(to_wide_null);
    let dir_ptr = working_dir.as_ref().map_or(ptr::null(), |v| v.as_ptr());

    let result = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            OPEN_VERB.as_ptr(),
            file_wide.as_ptr(),
            ptr::null(),
            dir_ptr,
            SW_SHOWNORMAL,
        )
    };
    if (result as isize) <= 32 {
        Err(format!(
            "Impossible d'ouvrir « {} » (code erreur ShellExecute: {}).",
            path, result as isize
        ))
    } else {
        Ok(())
    }
}

/// Ouvre l'Explorateur Windows en sélectionnant le fichier.
pub fn reveal_in_explorer(path: &str) {
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let _ = Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{}\"", path))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

/// Formate l'horodatage courant en ISO-8601 (`YYYY-MM-DDTHH:MM:SS`) sans dépendance externe.
pub fn now_iso8601() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    let second = rem % 60;

    // Algorithme civil_from_days de Howard Hinnant
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        year, m, d, hour, minute, second
    )
}
