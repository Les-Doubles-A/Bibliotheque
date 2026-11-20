use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use crossbeam_channel::{Sender, unbounded};
use crossbeam_deque::{Injector, Steal, Worker};

use crate::db::{ItemKind, LibraryDatabase, ScannedEntry};

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

/// Vérifie sans allocation si le nom de dossier doit être ignoré (système, caches, dev, redist).
#[inline]
pub fn should_skip_dir(name: &OsStr) -> bool {
    let bytes = name.as_encoded_bytes();
    match bytes.len() {
        3 => {
            bytes.eq_ignore_ascii_case(b"tmp")
                || bytes.eq_ignore_ascii_case(b"obj")
                || bytes.eq_ignore_ascii_case(b".m2")
        }
        4 => {
            bytes.eq_ignore_ascii_case(b".git")
                || bytes.eq_ignore_ascii_case(b"temp")
                || bytes.eq_ignore_ascii_case(b"logs")
                || bytes.eq_ignore_ascii_case(b"venv")
                || bytes.eq_ignore_ascii_case(b".npm")
        }
        5 => {
            bytes.eq_ignore_ascii_case(b"cache")
                || bytes.eq_ignore_ascii_case(b".venv")
                || bytes.eq_ignore_ascii_case(b".yarn")
        }
        6 => {
            bytes.eq_ignore_ascii_case(b"winsxs")
                || bytes.eq_ignore_ascii_case(b"caches")
                || bytes.eq_ignore_ascii_case(b"target")
                || bytes.eq_ignore_ascii_case(b"vendor")
                || bytes.eq_ignore_ascii_case(b"redist")
                || bytes.eq_ignore_ascii_case(b"dotnet")
                || bytes.eq_ignore_ascii_case(b".cargo")
                || bytes.eq_ignore_ascii_case(b".nuget")
        }
        7 => {
            bytes.eq_ignore_ascii_case(b"windows")
                || bytes.eq_ignore_ascii_case(b"updates")
                || bytes.eq_ignore_ascii_case(b"directx")
                || bytes.eq_ignore_ascii_case(b"drivers")
                || bytes.eq_ignore_ascii_case(b".gradle")
                || bytes.eq_ignore_ascii_case(b".rustup")
        }
        8 => {
            bytes.eq_ignore_ascii_case(b"recovery")
                || bytes.eq_ignore_ascii_case(b"perflogs")
                || bytes.eq_ignore_ascii_case(b"msocache")
                || bytes.eq_ignore_ascii_case(b"gpucache")
                || bytes.eq_ignore_ascii_case(b"locallow")
                || bytes.eq_ignore_ascii_case(b"vcredist")
        }
        9 => bytes.eq_ignore_ascii_case(b"installer"),
        10 => {
            bytes.eq_ignore_ascii_case(b"config.msi")
                || bytes.eq_ignore_ascii_case(b"code cache")
                || bytes.eq_ignore_ascii_case(b"crashdumps")
                || bytes.eq_ignore_ascii_case(b"installer2")
        }
        11 => {
            bytes.eq_ignore_ascii_case(b"$winreagent")
                || bytes.eq_ignore_ascii_case(b"windowsapps")
                || bytes.eq_ignore_ascii_case(b"programdata")
                || bytes.eq_ignore_ascii_case(b"__pycache__")
                || bytes.eq_ignore_ascii_case(b".pnpm-store")
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

    // Suffixes exclus
    if lower.ends_with("setup") {
        return true;
    }

    // Sous-chaînes exclues (désinstalleurs, updaters, crash/télémétrie, helpers/services)
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
    if should_skip_exe_stem(stem) {
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
    }

    is_windows_gui_exe(path)
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
                                        active_tasks.fetch_add(1, Ordering::Relaxed);
                                        worker.push(entry.path());
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
                                        if !is_windows_gui_exe(&full_path) {
                                            continue;
                                        }

                                        local_batch.push(ScannedEntry {
                                            path: full_path.to_string_lossy().into_owned(),
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
        assert!(should_skip_dir(OsStr::new("Reference Assemblies")));
        assert!(!should_skip_dir(OsStr::new("Program Files")));
        assert!(!should_skip_dir(OsStr::new("Programs")));
        assert!(!should_skip_dir(OsStr::new("Steam")));
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
        assert!(!is_windows_gui_exe(&cui64));
        assert!(!is_windows_gui_exe(&dll64));

        let _ = fs::remove_file(&gui64);
        let _ = fs::remove_file(&cui64);
        let _ = fs::remove_file(&dll64);
    }
}
