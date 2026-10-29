use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use crossbeam_channel::{Sender, unbounded};
use crossbeam_deque::{Injector, Steal, Worker};

use crate::db::{ItemKind, LibraryDatabase, ScannedEntry};

pub enum ScanEvent {
    Progress(String),
    Completed(usize),
    Failed(String),
}

/// Vérifie sans allocation si le nom de dossier fait partie de `SKIP_NAMES`.
/// SKIP_NAMES = {"winsxs", "installer", "$recycle.bin", "system volume information", ".git", "node_modules"}
#[inline]
fn should_skip_dir(name: &OsStr) -> bool {
    let bytes = name.as_encoded_bytes();
    match bytes.len() {
        4 => bytes.eq_ignore_ascii_case(b".git"),
        6 => bytes.eq_ignore_ascii_case(b"winsxs"),
        9 => bytes.eq_ignore_ascii_case(b"installer"),
        12 => {
            bytes.eq_ignore_ascii_case(b"$recycle.bin")
                || bytes.eq_ignore_ascii_case(b"node_modules")
        }
        25 => bytes.eq_ignore_ascii_case(b"system volume information"),
        _ => false,
    }
}

/// Détecte `.exe` ou `.lnk` directement sur les octets du nom de fichier sans allocation.
/// Retourne `(longueur_du_stem, ItemKind)`.
#[inline]
fn classify_file(name: &OsStr) -> Option<(usize, ItemKind)> {
    let bytes = name.as_encoded_bytes();
    if bytes.len() <= 4 {
        return None;
    }
    let ext = &bytes[bytes.len() - 4..];
    if ext.eq_ignore_ascii_case(b".exe") {
        Some((bytes.len() - 4, ItemKind::Application))
    } else if ext.eq_ignore_ascii_case(b".lnk") {
        Some((bytes.len() - 4, ItemKind::Shortcut))
    } else {
        None
    }
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
                                        if !should_skip_dir(&file_name) {
                                            active_tasks.fetch_add(1, Ordering::Relaxed);
                                            worker.push(entry.path());
                                        }
                                    } else if ft.is_file()
                                        && let Some((stem_len, kind)) = classify_file(&file_name)
                                    {
                                        let full_path = entry.path();
                                        let path_str = full_path.to_string_lossy().into_owned();
                                        let name_full = file_name.to_string_lossy();
                                        let stem = name_full
                                            .get(..stem_len)
                                            .unwrap_or(&name_full)
                                            .to_string();

                                        local_batch.push(ScannedEntry {
                                            path: path_str,
                                            name: stem,
                                            kind,
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
