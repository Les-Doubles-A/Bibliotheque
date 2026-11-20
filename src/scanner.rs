use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use crossbeam_channel::{Sender, unbounded};
use crossbeam_deque::{Injector, Steal, Worker};
use rustc_hash::FxHashSet;
use windows_sys::Win32::UI::Shell::ExtractIconExW;

use crate::db::{ItemKind, LibraryDatabase, ScannedEntry};
use crate::win_utils::to_wide_null;

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x0000_0004;
const IMAGE_FILE_DLL: u16 = 0x2000;
const IMAGE_SUBSYSTEM_WINDOWS_GUI: u16 = 2;

pub enum ScanEvent {
    Progress(String),
    Completed(usize),
    Failed(String),
}

#[inline]
fn is_hidden_or_system(meta: &fs::Metadata) -> bool {
    (meta.file_attributes() & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM)) != 0
}

/// Vérifie sans allocation si le nom de dossier doit être ignoré (système, caches, dev, redist, paquets).
#[inline]
pub fn should_skip_dir(name: &OsStr) -> bool {
    let bytes = name.as_encoded_bytes();
    if bytes.starts_with(b".") {
        return true;
    }
    match bytes.len() {
        3 => bytes.eq_ignore_ascii_case(b"tmp") || bytes.eq_ignore_ascii_case(b"obj"),
        4 => {
            bytes.eq_ignore_ascii_case(b"temp")
                || bytes.eq_ignore_ascii_case(b"logs")
                || bytes.eq_ignore_ascii_case(b"venv")
        }
        5 => bytes.eq_ignore_ascii_case(b"cache"),
        6 => {
            bytes.eq_ignore_ascii_case(b"winsxs")
                || bytes.eq_ignore_ascii_case(b"caches")
                || bytes.eq_ignore_ascii_case(b"target")
                || bytes.eq_ignore_ascii_case(b"vendor")
                || bytes.eq_ignore_ascii_case(b"redist")
                || bytes.eq_ignore_ascii_case(b"dotnet")
                || bytes.eq_ignore_ascii_case(b"winget")
        }
        7 => {
            bytes.eq_ignore_ascii_case(b"windows")
                || bytes.eq_ignore_ascii_case(b"updates")
                || bytes.eq_ignore_ascii_case(b"directx")
                || bytes.eq_ignore_ascii_case(b"drivers")
                || bytes.eq_ignore_ascii_case(b"staging")
        }
        8 => {
            bytes.eq_ignore_ascii_case(b"recovery")
                || bytes.eq_ignore_ascii_case(b"perflogs")
                || bytes.eq_ignore_ascii_case(b"msocache")
                || bytes.eq_ignore_ascii_case(b"gpucache")
                || bytes.eq_ignore_ascii_case(b"locallow")
                || bytes.eq_ignore_ascii_case(b"vcredist")
                || bytes.eq_ignore_ascii_case(b"unigetui")
        }
        9 => bytes.eq_ignore_ascii_case(b"installer"),
        10 => {
            bytes.eq_ignore_ascii_case(b"config.msi")
                || bytes.eq_ignore_ascii_case(b"code cache")
                || bytes.eq_ignore_ascii_case(b"crashdumps")
                || bytes.eq_ignore_ascii_case(b"installer2")
                || bytes.eq_ignore_ascii_case(b"chocolatey")
        }
        11 => {
            bytes.eq_ignore_ascii_case(b"$winreagent")
                || bytes.eq_ignore_ascii_case(b"windowsapps")
                || bytes.eq_ignore_ascii_case(b"programdata")
                || bytes.eq_ignore_ascii_case(b"__pycache__")
        }
        12 => {
            bytes.eq_ignore_ascii_case(b"$recycle.bin")
                || bytes.eq_ignore_ascii_case(b"node_modules")
                || bytes.eq_ignore_ascii_case(b"crashreports")
                || bytes.eq_ignore_ascii_case(b"squirreltemp")
                || bytes.eq_ignore_ascii_case(b"windows kits")
                || bytes.eq_ignore_ascii_case(b"common files")
        }
        13 => {
            bytes.eq_ignore_ascii_case(b"package cache")
                || bytes.eq_ignore_ascii_case(b"_commonredist")
        }
        20 => bytes.eq_ignore_ascii_case(b"reference assemblies"),
        25 => bytes.eq_ignore_ascii_case(b"system volume information"),
        _ => false,
    }
}

/// Vérifie si le nom d'un exécutable (sans `.exe`, en minuscules) correspond à un fichier technique
/// ou indésirable (désinstalleur, installeur, updater, crash reporter, helper, service, temporaire).
pub fn should_skip_exe_stem(stem: &str) -> bool {
    let lower = stem.to_ascii_lowercase();

    // Fichiers temporaires / sauvegarde de mise à jour (ex: Bibliotheque.old.exe)
    if lower.ends_with(".old") || lower.ends_with(".new") || lower.ends_with(".tmp") {
        return true;
    }

    // Noms exacts
    if matches!(lower.as_str(), "au_" | "squirrel") {
        return true;
    }

    // Préfixes exclus (désinstalleurs, installeurs, redistribuables, diagnostics)
    const SKIP_PREFIXES: &[&str] = &[
        "unins",
        "uninst",
        "unsetup",
        "remove",
        "setup",
        "install",
        "vc_redist",
        "vcredist",
        "dxsetup",
        "ndp",
        "dotnetfx",
        "elevate",
        "elevation_service",
        "werfault",
        "dump",
    ];
    if SKIP_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return true;
    }

    // Suffixes exclus (installeurs, variantes UIA / 32-bit ANSI-Unicode)
    if lower.ends_with("setup")
        || lower.ends_with("_uia")
        || lower.ends_with("a32")
        || lower.ends_with("u32")
    {
        return true;
    }

    // Composants matériels ASUS Aura / HAL
    if lower.starts_with("aac") && lower.contains("hal") {
        return true;
    }

    // Sous-chaînes exclues (désinstalleurs, updaters, crash/télémétrie, helpers/services, HAL)
    const SKIP_CONTAINS: &[&str] = &[
        "uninstall",
        "installer",
        "-setup-",
        "_setup_",
        "update",
        "updater",
        "maintenanceservice",
        "crash",
        "telemetry",
        "reporter",
        "diag",
        "helper",
        "service",
        "daemon",
        "agent",
        "broker",
        "worker",
        "proxy",
        "stub",
        "bootstrap",
        "shim",
        "hal_x64",
        "hal_x86",
        "aacambient",
        "qtwebengineprocess",
        "cefsubproc",
        "chrome_pwa_launcher",
        "nacl64",
        "dllhost",
        "rundll32",
        "regsvr32",
        "msiexec",
        "conhost",
        "taskhost",
        "svchost",
        "sihost",
    ];
    SKIP_CONTAINS.iter().any(|s| lower.contains(s))
}

/// Vérifie que l'exécutable embarque au moins une icône native (`RT_GROUP_ICON`).
/// Élimine automatiquement les processus invisibles, serveurs COM et HAL matériels sans icône.
pub fn has_embedded_icon(path: &Path) -> bool {
    let Some(path_str) = path.to_str() else {
        return false;
    };
    let wide = to_wide_null(path_str);
    let count = unsafe {
        ExtractIconExW(
            wide.as_ptr(),
            -1,
            ptr::null_mut(),
            ptr::null_mut(),
            0,
        )
    };
    count > 0 && count != u32::MAX
}

/// Vérifie via l'en-tête PE Windows que le fichier est bien une application graphique (`GUI`)
/// et non un outil console (`CUI`) ou une DLL.
pub fn is_windows_gui_exe(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut dos = [0u8; 64];
    if file.read_exact(&mut dos).is_err() || &dos[0..2] != b"MZ" {
        return false;
    }

    let pe_offset = u32::from_le_bytes([dos[0x3C], dos[0x3D], dos[0x3E], dos[0x3F]]) as u64;
    if pe_offset < 64 || pe_offset > 16 * 1024 * 1024 {
        return false;
    }

    if file.seek(SeekFrom::Start(pe_offset)).is_err() {
        return false;
    }

    // 4 octets "PE\0\0" + 20 octets COFF Header + 70 octets Optional Header jusqu'à Subsystem
    let mut pe = [0u8; 94];
    if file.read_exact(&mut pe).is_err() || &pe[0..4] != b"PE\0\0" {
        return false;
    }

    let characteristics = u16::from_le_bytes([pe[22], pe[23]]);
    if (characteristics & IMAGE_FILE_DLL) != 0 {
        return false;
    }

    let opt_magic = u16::from_le_bytes([pe[24], pe[25]]);
    if opt_magic != 0x010B && opt_magic != 0x020B {
        return false;
    }

    let subsystem = u16::from_le_bytes([pe[92], pe[93]]);
    subsystem == IMAGE_SUBSYSTEM_WINDOWS_GUI
}

/// Détecte `.exe` directement sur les octets du nom de fichier sans allocation.
/// Retourne la longueur du stem (nom sans `.exe`).
#[inline]
fn extract_exe_stem_len(name: &OsStr) -> Option<usize> {
    let bytes = name.as_encoded_bytes();
    if bytes.len() <= 4 {
        return None;
    }
    let ext = &bytes[bytes.len() - 4..];
    if ext.eq_ignore_ascii_case(b".exe") {
        Some(bytes.len() - 4)
    } else {
        None
    }
}

/// Détecte si un exécutable est une variante d'architecture redondante (`*32.exe`, `*64.exe`, `*x86.exe`)
/// alors que l'exécutable principal (ou sa version 64-bit) existe déjà dans le même dossier.
fn is_redundant_arch_variant(path: &Path, stem: &str) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    let lower = stem.to_ascii_lowercase();
    if lower.len() > 2 && lower.ends_with("32") {
        let base = &stem[..stem.len() - 2];
        let trimmed = base.trim_end_matches(['_', '-']);
        if (!trimmed.is_empty() && parent.join(format!("{trimmed}.exe")).is_file())
            || parent.join(format!("{base}64.exe")).is_file()
        {
            return true;
        }
    } else if lower.len() > 3 && lower.ends_with("x86") {
        let base = &stem[..stem.len() - 3];
        let trimmed = base.trim_end_matches(['_', '-']);
        if (!trimmed.is_empty() && parent.join(format!("{trimmed}.exe")).is_file())
            || parent.join(format!("{base}x64.exe")).is_file()
            || parent.join(format!("{base}64.exe")).is_file()
        {
            return true;
        }
    } else if lower.len() > 2 && lower.ends_with("64") {
        let base = &stem[..stem.len() - 2];
        let trimmed = base.trim_end_matches(['_', '-']);
        if !trimmed.is_empty() && parent.join(format!("{trimmed}.exe")).is_file() {
            return true;
        }
    }
    false
}

/// Analyse un nom de dossier versionné (`v2.0.30`, `app-1.0.9161`, `130.0.6723.92`).
/// Retourne `(prefixe_normalise, composantes_numeriques)` s'il y a au moins 2 segments numériques.
pub fn parse_version_dir_name(name: &str) -> Option<(&'static str, Vec<u32>)> {
    let lower = name.to_ascii_lowercase();
    let (prefix, rest) = if let Some(r) = lower.strip_prefix("app-") {
        ("app-", r)
    } else if let Some(r) = lower.strip_prefix("version-") {
        ("version-", r)
    } else if let Some(r) = lower.strip_prefix("ver-") {
        ("ver-", r)
    } else if let Some(r) = lower.strip_prefix("rel-") {
        ("rel-", r)
    } else if let Some(r) = lower.strip_prefix('v') {
        ("v", r)
    } else {
        ("", lower.as_str())
    };

    if rest.is_empty() || !rest.as_bytes()[0].is_ascii_digit() {
        return None;
    }

    let mut parts = Vec::with_capacity(4);
    for seg in rest.split('.') {
        if seg.is_empty() || !seg.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        parts.push(seg.parse::<u32>().ok()?);
    }

    if parts.len() >= 2 {
        Some((prefix, parts))
    } else {
        None
    }
}

/// Indique si `dir` est un dossier versionné obsolète car son parent contient une version plus récente.
fn is_obsolete_version_dir(dir: &Path) -> bool {
    let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let Some((prefix, ver)) = parse_version_dir_name(name) else {
        return false;
    };
    let Some(parent) = dir.parent() else {
        return false;
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return false;
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|ft| ft.is_dir()) {
            continue;
        }
        let sib_name = entry.file_name();
        if let Some(sib_str) = sib_name.to_str()
            && let Some((sib_prefix, sib_ver)) = parse_version_dir_name(sib_str)
            && sib_prefix == prefix
            && sib_ver > ver
        {
            return true;
        }
    }
    false
}

/// Vérifie si un chemin déjà enregistré en base reste valide selon les règles de filtrage actuelles.
pub fn is_valid_library_path(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() || is_hidden_or_system(&meta) {
        return false;
    }

    let Some(file_name) = path.file_name() else {
        return false;
    };
    let Some(stem_len) = extract_exe_stem_len(file_name) else {
        return false;
    };
    let name_full = file_name.to_string_lossy();
    let stem = name_full.get(..stem_len).unwrap_or(&name_full);
    if should_skip_exe_stem(stem) || is_redundant_arch_variant(path, stem) {
        return false;
    }

    if let Some(parent) = path.parent() {
        for comp in parent.components() {
            if let Component::Normal(dir_name) = comp
                && should_skip_dir(dir_name)
            {
                return false;
            }
        }
        let mut cur = Some(parent);
        while let Some(dir) = cur {
            if is_obsolete_version_dir(dir) {
                return false;
            }
            cur = dir.parent();
        }
    }

    is_windows_gui_exe(path) && has_embedded_icon(path)
}

pub fn spawn_scan(folders: Vec<PathBuf>, event_tx: Sender<ScanEvent>, ctx: egui::Context) {
    thread::Builder::new()
        .name("scan-coordinator".into())
        .spawn(move || {
            if let Err(err) = run_parallel_scan(folders, &event_tx, &ctx) {
                let _ = event_tx.send(ScanEvent::Failed(err));
                ctx.request_repaint();
            }
        })
        .ok();
}

fn run_parallel_scan(
    folders: Vec<PathBuf>,
    event_tx: &Sender<ScanEvent>,
    ctx: &egui::Context,
) -> Result<(), String> {
    let db = LibraryDatabase::open()?;
    let excluded: Arc<FxHashSet<String>> = Arc::new(db.excluded_paths());
    let mut total_indexed: usize = 0;

    let num_threads = thread::available_parallelism()
        .map(|n| n.get().clamp(2, 16))
        .unwrap_or(4);

    for root_folder in folders {
        let _ = event_tx.send(ScanEvent::Progress(format!(
            "Analyse : {}",
            root_folder.display()
        )));
        ctx.request_repaint();

        let injector = Arc::new(Injector::<PathBuf>::new());
        injector.push(root_folder);

        let active_tasks = Arc::new(AtomicUsize::new(1));
        let (batch_tx, batch_rx) = unbounded::<Vec<ScannedEntry>>();

        let mut workers = Vec::with_capacity(num_threads);
        let mut stealers = Vec::with_capacity(num_threads);
        for _ in 0..num_threads {
            let w = Worker::<PathBuf>::new_fifo();
            stealers.push(w.stealer());
            workers.push(w);
        }
        let stealers = Arc::new(stealers);

        thread::scope(|s| -> Result<(), String> {
            for worker in workers {
                let injector = Arc::clone(&injector);
                let stealers = Arc::clone(&stealers);
                let active_tasks = Arc::clone(&active_tasks);
                let excluded = Arc::clone(&excluded);
                let batch_tx = batch_tx.clone();

                s.spawn(move || {
                    let mut local_batch = Vec::with_capacity(512);

                    loop {
                        let maybe_dir = worker.pop().or_else(|| {
                            std::iter::repeat_with(|| {
                                injector
                                    .steal_batch_and_pop(&worker)
                                    .or_else(|| stealers.iter().map(|st| st.steal()).collect())
                            })
                            .find(|st| !matches!(st, Steal::Retry))
                            .and_then(Steal::success)
                        });

                        if let Some(dir) = maybe_dir {
                            if let Ok(entries) = fs::read_dir(&dir) {
                                let mut versioned_dirs: Vec<(&'static str, Vec<u32>, PathBuf)> =
                                    Vec::new();

                                for entry_res in entries {
                                    let Ok(entry) = entry_res else { continue };
                                    let Ok(ft) = entry.file_type() else { continue };
                                    if ft.is_symlink() {
                                        continue;
                                    }
                                    let file_name = entry.file_name();
                                    if ft.is_dir() {
                                        if should_skip_dir(&file_name) {
                                            continue;
                                        }
                                        if entry.metadata().is_ok_and(|m| is_hidden_or_system(&m)) {
                                            continue;
                                        }
                                        if let Some(name_str) = file_name.to_str()
                                            && let Some((prefix, ver)) =
                                                parse_version_dir_name(name_str)
                                        {
                                            versioned_dirs.push((prefix, ver, entry.path()));
                                        } else {
                                            active_tasks.fetch_add(1, Ordering::Relaxed);
                                            worker.push(entry.path());
                                        }
                                    } else if ft.is_file()
                                        && let Some(stem_len) = extract_exe_stem_len(&file_name)
                                    {
                                        let name_full = file_name.to_string_lossy();
                                        let stem = name_full
                                            .get(..stem_len)
                                            .unwrap_or(&name_full);

                                        if should_skip_exe_stem(stem) {
                                            continue;
                                        }
                                        if entry.metadata().is_ok_and(|m| is_hidden_or_system(&m)) {
                                            continue;
                                        }

                                        let full_path = entry.path();
                                        if is_redundant_arch_variant(&full_path, stem) {
                                            continue;
                                        }
                                        let path_str = full_path.to_string_lossy().into_owned();
                                        if excluded.contains(&path_str) {
                                            continue;
                                        }
                                        if !is_windows_gui_exe(&full_path)
                                            || !has_embedded_icon(&full_path)
                                        {
                                            continue;
                                        }

                                        local_batch.push(ScannedEntry {
                                            path: path_str,
                                            name: stem.to_string(),
                                            kind: ItemKind::Application,
                                        });

                                        if local_batch.len() >= 512 {
                                            let batch = std::mem::replace(
                                                &mut local_batch,
                                                Vec::with_capacity(512),
                                            );
                                            let _ = batch_tx.send(batch);
                                        }
                                    }
                                }

                                // Pour les sous-dossiers versionnés (v2.0.26, v2.0.30, app-1.0.x...),
                                // ne parcourir que la version la plus récente par préfixe.
                                if !versioned_dirs.is_empty() {
                                    let mut best_by_prefix: Vec<(&'static str, Vec<u32>, PathBuf)> =
                                        Vec::new();
                                    for (prefix, ver, path) in versioned_dirs {
                                        if let Some(slot) =
                                            best_by_prefix.iter_mut().find(|(p, _, _)| *p == prefix)
                                        {
                                            if ver > slot.1 {
                                                slot.1 = ver;
                                                slot.2 = path;
                                            }
                                        } else {
                                            best_by_prefix.push((prefix, ver, path));
                                        }
                                    }
                                    for (_, _, path) in best_by_prefix {
                                        active_tasks.fetch_add(1, Ordering::Relaxed);
                                        worker.push(path);
                                    }
                                }
                            }

                            if active_tasks.fetch_sub(1, Ordering::AcqRel) == 1 {
                                break;
                            }
                        } else if active_tasks.load(Ordering::Acquire) == 0 {
                            break;
                        } else {
                            thread::yield_now();
                        }
                    }

                    if !local_batch.is_empty() {
                        let _ = batch_tx.send(local_batch);
                    }
                });
            }

            drop(batch_tx);

            while let Ok(batch) = batch_rx.recv() {
                let len = batch.len();
                db.add_paths(&batch)?;
                total_indexed += len;
            }
            Ok(())
        })?;
    }

    let _ = event_tx.send(ScanEvent::Completed(total_indexed));
    ctx.request_repaint();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_should_skip_dir() {
        assert!(should_skip_dir(OsStr::new("Windows")));
        assert!(should_skip_dir(OsStr::new("node_modules")));
        assert!(should_skip_dir(OsStr::new("_CommonRedist")));
        assert!(should_skip_dir(OsStr::new("GPUCache")));
        assert!(should_skip_dir(OsStr::new(".m2")));
        assert!(should_skip_dir(OsStr::new(".staging")));
        assert!(should_skip_dir(OsStr::new("Chocolatey")));
        assert!(should_skip_dir(OsStr::new("UniGetUI")));
        assert!(should_skip_dir(OsStr::new("Reference Assemblies")));
        assert!(!should_skip_dir(OsStr::new("Program Files")));
        assert!(!should_skip_dir(OsStr::new("Programs")));
        assert!(!should_skip_dir(OsStr::new("Steam")));
    }

    #[test]
    fn test_version_dir_parsing() {
        let (p1, v1) = parse_version_dir_name("v2.0.26").unwrap();
        let (p2, v2) = parse_version_dir_name("v2.0.30").unwrap();
        assert_eq!(p1, p2);
        assert!(v2 > v1);
        assert_eq!(parse_version_dir_name("app-1.0.9161"), Some(("app-", vec![1, 0, 9161])));
        assert_eq!(parse_version_dir_name("130.0.6723.92"), Some(("", vec![130, 0, 6723, 92])));
        assert_eq!(parse_version_dir_name("v2"), None);
        assert_eq!(parse_version_dir_name("AutoHotkey"), None);
    }

    #[test]
    fn test_should_skip_exe_stem() {
        assert!(should_skip_exe_stem("unins000"));
        assert!(should_skip_exe_stem("Uninstall"));
        assert!(should_skip_exe_stem("Bibliotheque-Setup-2.0.0"));
        assert!(should_skip_exe_stem("vc_redist.x64"));
        assert!(should_skip_exe_stem("GoogleUpdate"));
        assert!(should_skip_exe_stem("crashpad_handler"));
        assert!(should_skip_exe_stem("notification_helper"));
        assert!(should_skip_exe_stem("QtWebEngineProcess"));
        assert!(should_skip_exe_stem("Bibliotheque.old"));
        assert!(should_skip_exe_stem("Squirrel"));
        assert!(should_skip_exe_stem("Aac3572DramHal_x64"));
        assert!(should_skip_exe_stem("AacAmbientLighting"));
        assert!(should_skip_exe_stem("AutoHotkey64_UIA"));
        assert!(should_skip_exe_stem("AutoHotkeyA32"));
        assert!(should_skip_exe_stem("AutoHotkeyU32"));

        assert!(!should_skip_exe_stem("Bibliotheque"));
        assert!(!should_skip_exe_stem("Discord"));
        assert!(!should_skip_exe_stem("Code"));
        assert!(!should_skip_exe_stem("firefox"));
        assert!(!should_skip_exe_stem("HollowKnight"));
    }

    fn write_fake_pe(path: &Path, magic: u16, subsystem: u16, is_dll: bool) {
        let mut buf = vec![0u8; 256];
        buf[0] = b'M';
        buf[1] = b'Z';
        let pe_off: u32 = 0x80;
        buf[0x3C..0x40].copy_from_slice(&pe_off.to_le_bytes());

        let o = pe_off as usize;
        buf[o..o + 4].copy_from_slice(b"PE\0\0");
        let chars: u16 = if is_dll { IMAGE_FILE_DLL } else { 0x0002 };
        buf[o + 22..o + 24].copy_from_slice(&chars.to_le_bytes());
        buf[o + 24..o + 26].copy_from_slice(&magic.to_le_bytes());
        buf[o + 92..o + 94].copy_from_slice(&subsystem.to_le_bytes());

        let mut f = File::create(path).expect("create fake pe");
        f.write_all(&buf).expect("write fake pe");
    }

    #[test]
    fn test_pe_gui_detection() {
        let tmp = std::env::temp_dir();
        let gui64 = tmp.join("bib_test_gui64.exe");
        let cui64 = tmp.join("bib_test_cui64.exe");
        let dll64 = tmp.join("bib_test_dll64.exe");

        write_fake_pe(&gui64, 0x020B, IMAGE_SUBSYSTEM_WINDOWS_GUI, false);
        write_fake_pe(&cui64, 0x020B, 3, false);
        write_fake_pe(&dll64, 0x020B, IMAGE_SUBSYSTEM_WINDOWS_GUI, true);

        assert!(is_windows_gui_exe(&gui64));
        assert!(!has_embedded_icon(&gui64));
        assert!(!is_windows_gui_exe(&cui64));
        assert!(!is_windows_gui_exe(&dll64));

        let _ = fs::remove_file(&gui64);
        let _ = fs::remove_file(&cui64);
        let _ = fs::remove_file(&dll64);
    }
}
