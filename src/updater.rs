use std::env;
use std::ffi::c_void;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::ptr;
use std::thread;

use crossbeam_channel::Sender;
use egui::Context;

use crate::win_utils::{open_file, to_wide_null};

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const REPO_OWNER_AND_NAME: &str = "Les-Doubles-A/Bibliotheque";
pub const REPO_RELEASES_URL: &str = "https://github.com/Les-Doubles-A/Bibliotheque/releases";

type HInternet = *mut c_void;

const WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY: u32 = 4;
const WINHTTP_FLAG_SECURE: u32 = 0x0080_0000;
const WINHTTP_QUERY_STATUS_CODE: u32 = 19;
const WINHTTP_QUERY_FLAG_NUMBER: u32 = 0x2000_0000;
const INTERNET_DEFAULT_HTTPS_PORT: u16 = 443;

#[link(name = "winhttp", kind = "raw-dylib")]
unsafe extern "system" {
    fn WinHttpOpen(
        psz_agent_w: *const u16,
        dw_access_type: u32,
        psz_proxy_w: *const u16,
        psz_proxy_bypass_w: *const u16,
        dw_flags: u32,
    ) -> HInternet;
    fn WinHttpConnect(
        h_session: HInternet,
        pswz_server_name: *const u16,
        n_server_port: u16,
        dw_reserved: u32,
    ) -> HInternet;
    fn WinHttpOpenRequest(
        h_connect: HInternet,
        pwsz_verb: *const u16,
        pwsz_object_name: *const u16,
        pwsz_version: *const u16,
        pwsz_referrer: *const u16,
        ppwsz_accept_types: *const *const u16,
        dw_flags: u32,
    ) -> HInternet;
    fn WinHttpSetTimeouts(
        h_internet: HInternet,
        n_resolve_timeout: i32,
        n_connect_timeout: i32,
        n_send_timeout: i32,
        n_receive_timeout: i32,
    ) -> i32;
    fn WinHttpSendRequest(
        h_request: HInternet,
        lpsz_headers: *const u16,
        dw_headers_length: u32,
        lp_optional: *const c_void,
        dw_optional_length: u32,
        dw_total_length: u32,
        dw_context: usize,
    ) -> i32;
    fn WinHttpReceiveResponse(h_request: HInternet, lp_reserved: *mut c_void) -> i32;
    fn WinHttpQueryHeaders(
        h_request: HInternet,
        dw_info_level: u32,
        pwsz_name: *const u16,
        lp_buffer: *mut c_void,
        lpdw_buffer_length: *mut u32,
        lpdw_index: *mut u32,
    ) -> i32;
    fn WinHttpReadData(
        h_request: HInternet,
        lp_buffer: *mut c_void,
        dw_number_of_bytes_to_read: u32,
        lpdw_number_of_bytes_read: *mut u32,
    ) -> i32;
    fn WinHttpCloseHandle(h_internet: HInternet) -> i32;
}

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub version: String,
    pub download_url: String,
    pub html_url: String,
}

pub enum UpdateEvent {
    UpdateAvailable(UpdateInfo),
    UpToDate,
    DownloadProgress(String),
    UpdateInstalledAndRestarting,
    UpdateFailed(String),
}

/// Nettoie au démarrage l'ancien binaire `.old` laissé par une mise à jour précédente.
pub fn cleanup_old_executable() {
    if let Ok(exe) = env::current_exe() {
        let old = exe.with_extension("exe.old");
        if old.exists() {
            let _ = fs::remove_file(old);
        }
        let tmp = exe.with_extension("exe.new");
        if tmp.exists() {
            let _ = fs::remove_file(tmp);
        }
    }
}

/// Lance en arrière-plan la vérification de nouvelle version sur GitHub.
pub fn spawn_update_check(tx: Sender<UpdateEvent>, ctx: Context) {
    thread::Builder::new()
        .name("update-checker".into())
        .spawn(move || {
            if let Some(info) = check_github_for_update() {
                let _ = tx.send(UpdateEvent::UpdateAvailable(info));
                ctx.request_repaint();
            } else {
                let _ = tx.send(UpdateEvent::UpToDate);
            }
        })
        .ok();
}

/// Télécharge et applique la mise à jour en arrière-plan, puis relance l'application.
pub fn spawn_perform_update(info: UpdateInfo, tx: Sender<UpdateEvent>, ctx: Context) {
    thread::Builder::new()
        .name("update-installer".into())
        .spawn(move || {
            let _ = tx.send(UpdateEvent::DownloadProgress(format!(
                "Telechargement de la mise a jour v{}...",
                info.version
            )));
            ctx.request_repaint();

            match download_and_replace_exe(&info) {
                Ok(()) => {
                    let _ = tx.send(UpdateEvent::UpdateInstalledAndRestarting);
                    ctx.request_repaint();
                }
                Err(err) => {
                    // En cas d'échec (ex: dossier protégé en écriture), on ouvre la page GitHub
                    let _ = open_file(&info.html_url);
                    let _ = tx.send(UpdateEvent::UpdateFailed(err));
                    ctx.request_repaint();
                }
            }
        })
        .ok();
}

fn check_github_for_update() -> Option<UpdateInfo> {
    let current_semver = parse_semver(CURRENT_VERSION)?;
    let mut best_update: Option<((u32, u32, u32), UpdateInfo)> = None;

    // 1. Vérifie d'abord l'API GitHub Releases (/releases/latest)
    let api_path = format!("/repos/{REPO_OWNER_AND_NAME}/releases/latest");
    if let Ok(bytes) = https_get("api.github.com", &api_path, 8_000)
        && let Ok(json) = std::str::from_utf8(&bytes)
        && let Some(info) = parse_github_release_json(json)
        && let Some(ver) = parse_semver(&info.version)
        && ver > current_semver
    {
        best_update = Some((ver, info));
    }

    // 2. Vérifie également le Cargo.toml de la branche `main` (si le dépôt est mis à jour par push direct)
    let raw_cargo_path = format!("/{REPO_OWNER_AND_NAME}/main/Cargo.toml");
    if let Ok(bytes) = https_get("raw.githubusercontent.com", &raw_cargo_path, 8_000)
        && let Ok(toml_str) = std::str::from_utf8(&bytes)
        && let Some(ver_str) = parse_cargo_toml_version(toml_str)
        && let Some(ver) = parse_semver(&ver_str)
        && ver > current_semver
    {
        let is_newer = best_update.as_ref().is_none_or(|(b_ver, _)| ver > *b_ver);
        if is_newer {
            best_update = Some((
                ver,
                UpdateInfo {
                    version: ver_str,
                    download_url: format!(
                        "https://raw.githubusercontent.com/{REPO_OWNER_AND_NAME}/main/dist/Bibliotheque.exe"
                    ),
                    html_url: format!("https://github.com/{REPO_OWNER_AND_NAME}"),
                },
            ));
        }
    }

    best_update.map(|(_, info)| info)
}

fn download_and_replace_exe(info: &UpdateInfo) -> Result<(), String> {
    let (host, path) = split_https_url(&info.download_url)
        .ok_or_else(|| format!("URL de mise a jour invalide : {}", info.download_url))?;

    let bytes = https_get(&host, &path, 60_000)?;
    // Vérifie l'en-tête PE Windows ("MZ") et une taille minimale cohérente (> 200 Ko)
    if bytes.len() < 200_000 || &bytes[0..2] != b"MZ" {
        return Err(
            "Le binaire de mise a jour telecharge est introuvable ou invalide.".to_string(),
        );
    }

    let current_exe: PathBuf = env::current_exe().map_err(|e| e.to_string())?;
    let new_exe = current_exe.with_extension("exe.new");
    let old_exe = current_exe.with_extension("exe.old");

    {
        let mut file = fs::File::create(&new_exe).map_err(|e| {
            format!("Impossible d'ecrire le nouveau binaire ({e}). Ouverture de GitHub...")
        })?;
        file.write_all(&bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
    }

    let _ = fs::remove_file(&old_exe);
    fs::rename(&current_exe, &old_exe).map_err(|e| {
        let _ = fs::remove_file(&new_exe);
        format!("Impossible de remplacer l'executable en cours ({e}).")
    })?;

    if let Err(e) = fs::rename(&new_exe, &current_exe) {
        // Restauration en cas d'erreur
        let _ = fs::rename(&old_exe, &current_exe);
        return Err(format!("Echec de l'installation de la mise a jour ({e})."));
    }

    // Relance la nouvelle version et ferme l'instance actuelle
    let _ = Command::new(&current_exe).spawn();
    std::process::exit(0);
}

fn https_get(host: &str, path: &str, timeout_ms: i32) -> Result<Vec<u8>, String> {
    let agent = to_wide_null("Bibliotheque-Updater/2.0");
    let host_w = to_wide_null(host);
    let path_w = to_wide_null(path);
    let verb_w = to_wide_null("GET");
    let headers_w = to_wide_null("Accept: application/vnd.github+json, */*\r\n");

    unsafe {
        let session = WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            ptr::null(),
            ptr::null(),
            0,
        );
        if session.is_null() {
            return Err("WinHttpOpen a echoue".to_string());
        }

        WinHttpSetTimeouts(session, timeout_ms, timeout_ms, timeout_ms, timeout_ms);

        let connect = WinHttpConnect(session, host_w.as_ptr(), INTERNET_DEFAULT_HTTPS_PORT, 0);
        if connect.is_null() {
            WinHttpCloseHandle(session);
            return Err("WinHttpConnect a echoue".to_string());
        }

        let request = WinHttpOpenRequest(
            connect,
            verb_w.as_ptr(),
            path_w.as_ptr(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            WINHTTP_FLAG_SECURE,
        );
        if request.is_null() {
            WinHttpCloseHandle(connect);
            WinHttpCloseHandle(session);
            return Err("WinHttpOpenRequest a echoue".to_string());
        }

        let sent = WinHttpSendRequest(
            request,
            headers_w.as_ptr(),
            u32::MAX,
            ptr::null(),
            0,
            0,
            0,
        );
        if sent == 0 || WinHttpReceiveResponse(request, ptr::null_mut()) == 0 {
            WinHttpCloseHandle(request);
            WinHttpCloseHandle(connect);
            WinHttpCloseHandle(session);
            return Err("Requete HTTPS echouee".to_string());
        }

        let mut status_code: u32 = 0;
        let mut status_size = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            ptr::null(),
            (&mut status_code as *mut u32).cast(),
            &mut status_size,
            ptr::null_mut(),
        );

        if status_code != 200 {
            WinHttpCloseHandle(request);
            WinHttpCloseHandle(connect);
            WinHttpCloseHandle(session);
            return Err(format!("Statut HTTP {status_code}"));
        }

        let mut body = Vec::with_capacity(8192);
        let mut chunk = [0u8; 16384];
        loop {
            let mut bytes_read: u32 = 0;
            let ok = WinHttpReadData(
                request,
                chunk.as_mut_ptr().cast(),
                chunk.len() as u32,
                &mut bytes_read,
            );
            if ok == 0 || bytes_read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..bytes_read as usize]);
        }

        WinHttpCloseHandle(request);
        WinHttpCloseHandle(connect);
        WinHttpCloseHandle(session);
        Ok(body)
    }
}

pub fn parse_semver(raw: &str) -> Option<(u32, u32, u32)> {
    let trimmed = raw.trim().trim_start_matches(['v', 'V']);
    let version_part = trimmed.split(['-', '+']).next()?;
    let mut parts = version_part.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next().unwrap_or("0").parse().ok()?;
    let patch: u32 = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

pub fn parse_cargo_toml_version(toml_content: &str) -> Option<String> {
    let mut in_package = false;
    for line in toml_content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if in_package
            && let Some(rest) = line.strip_prefix("version")
        {
            let rest = rest.trim_start();
            if let Some(val) = rest.strip_prefix('=') {
                let unquoted = val.trim().trim_matches('"').trim_matches('\'');
                if parse_semver(unquoted).is_some() {
                    return Some(unquoted.to_string());
                }
            }
        }
    }
    None
}

pub fn parse_github_release_json(json: &str) -> Option<UpdateInfo> {
    let tag_raw = extract_json_string_field(json, "tag_name")?;
    let version = tag_raw.trim_start_matches(['v', 'V']).to_string();
    let html_url = extract_json_string_field(json, "html_url")
        .unwrap_or_else(|| REPO_RELEASES_URL.to_string());

    // Cherche un asset .exe dans browser_download_url
    let mut download_url = None;
    let mut search_slice = json;
    while let Some(url) = extract_json_string_field(search_slice, "browser_download_url") {
        let lower = url.to_ascii_lowercase();
        if lower.ends_with(".exe") && !lower.contains("setup") {
            download_url = Some(url);
            break;
        }
        if let Some(pos) = search_slice.find("browser_download_url") {
            search_slice = &search_slice[pos + 20..];
        } else {
            break;
        }
    }

    let download_url = download_url.unwrap_or_else(|| {
        format!(
            "https://raw.githubusercontent.com/{REPO_OWNER_AND_NAME}/{tag_raw}/dist/Bibliotheque.exe"
        )
    });

    Some(UpdateInfo {
        version,
        download_url,
        html_url,
    })
}

fn extract_json_string_field(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{key}\"");
    let start = json.find(&pattern)?;
    let after_key = &json[start + pattern.len()..];
    let colon = after_key.find(':')?;
    let after_colon = after_key[colon + 1..].trim_start();
    if !after_colon.starts_with('"') {
        return None;
    }
    let content = &after_colon[1..];
    let mut out = String::new();
    let mut escaped = false;
    for ch in content.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some(out);
        } else {
            out.push(ch);
        }
    }
    None
}

fn split_https_url(url: &str) -> Option<(String, String)> {
    let without_scheme = url.strip_prefix("https://")?;
    let slash_pos = without_scheme.find('/')?;
    let host = without_scheme[..slash_pos].to_string();
    let path = without_scheme[slash_pos..].to_string();
    Some((host, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_semver_and_parsers() {
        assert_eq!(parse_semver("v2.1.0"), Some((2, 1, 0)));
        assert_eq!(parse_semver("2.0.0"), Some((2, 0, 0)));
        assert!(parse_semver("2.1.0") > parse_semver("2.0.0"));

        let sample_toml = "[package]\nname = \"bibliotheque\"\nversion = \"2.3.1\"\nedition = \"2024\"";
        assert_eq!(
            parse_cargo_toml_version(sample_toml),
            Some("2.3.1".to_string())
        );

        let sample_json = r#"{
            "tag_name": "v2.4.0",
            "html_url": "https://github.com/Les-Doubles-A/Bibliotheque/releases/tag/v2.4.0",
            "assets": [
                {
                    "name": "Bibliotheque.exe",
                    "browser_download_url": "https://github.com/Les-Doubles-A/Bibliotheque/releases/download/v2.4.0/Bibliotheque.exe"
                }
            ]
        }"#;
        let info = parse_github_release_json(sample_json).expect("parse release json");
        assert_eq!(info.version, "2.4.0");
        assert!(info.download_url.ends_with("Bibliotheque.exe"));
    }
}
