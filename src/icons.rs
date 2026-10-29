use std::ffi::c_void;
use std::mem::{MaybeUninit, size_of};
use std::path::Path;
use std::ptr;
use std::sync::Arc;
use std::thread;

use crossbeam_channel::{Receiver, unbounded};
use egui::{ColorImage, Context, TextureHandle, TextureOptions};
use parking_lot::{Condvar, Mutex};
use rustc_hash::{FxHashMap, FxHashSet};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile, STGM_READ,
};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
use windows::core::{Interface, PCWSTR};
use windows_sys::Win32::Foundation::SIZE;
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, GetObjectW, HBITMAP, ReleaseDC,
    SelectObject,
};
use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows_sys::Win32::UI::Shell::{
    SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_SMALLICON, SHGFI_USEFILEATTRIBUTES,
    SHGetFileInfoW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DI_NORMAL, DestroyIcon, DrawIconEx, GetIconInfo, HICON, ICONINFO,
};
use windows_sys::core::GUID;

use crate::db::ItemKind;
use crate::win_utils::to_wide_null;

#[repr(C)]
struct IUnknownVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

#[repr(C)]
struct IShellItemImageFactoryVtbl {
    base: IUnknownVtbl,
    get_image: unsafe extern "system" fn(*mut c_void, SIZE, i32, *mut HBITMAP) -> i32,
}

#[repr(C)]
struct IShellItemImageFactory {
    vtbl: *const IShellItemImageFactoryVtbl,
}

const IID_ISHELL_ITEM_IMAGE_FACTORY: GUID = GUID {
    data1: 0xbcc18b79,
    data2: 0xba16,
    data3: 0x442f,
    data4: [0x80, 0xc4, 0x8a, 0x59, 0xc3, 0x0c, 0x46, 0x3b],
};

const SIIGBF_BIGGERSIZEOK: i32 = 0x0000_0001;
const SIIGBF_ICONONLY: i32 = 0x0000_0004;

#[link(name = "shell32", kind = "raw-dylib")]
unsafe extern "system" {
    fn SHCreateItemFromParsingName(
        psz_path: *const u16,
        pbc: *mut c_void,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> i32;
}

enum CacheState {
    Loaded(TextureHandle),
    Failed,
}

struct LoadedIconData {
    path: String,
    rgba: Option<(usize, usize, Vec<u8>)>,
}

struct ViewportQueue {
    /// Pile LIFO des chemins visibles à l'écran (les plus récents en haut de pile).
    stack: Vec<String>,
    /// Chemins actuellement en cours d'extraction par un worker.
    in_flight: FxHashSet<String>,
}

pub struct IconManager {
    cache: FxHashMap<String, CacheState>,
    queue: Arc<(Mutex<ViewportQueue>, Condvar)>,
    res_rx: Receiver<LoadedIconData>,
    fallback_app: Option<TextureHandle>,
    fallback_shortcut: Option<TextureHandle>,
}

impl IconManager {
    pub fn new(ctx: Context) -> Self {
        let queue = Arc::new((
            Mutex::new(ViewportQueue {
                stack: Vec::with_capacity(64),
                in_flight: FxHashSet::default(),
            }),
            Condvar::new(),
        ));
        let (res_tx, res_rx) = unbounded::<LoadedIconData>();

        // Pool de 4 workers COM STA dépilant en LIFO (priorité absolue aux lignes visibles)
        for idx in 0..4 {
            let queue_clone = Arc::clone(&queue);
            let tx = res_tx.clone();
            let ctx_clone = ctx.clone();
            thread::Builder::new()
                .name(format!("shell-icon-worker-{idx}"))
                .spawn(move || {
                    unsafe {
                        CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32);
                    }
                    loop {
                        let path = {
                            let (lock, cvar) = &*queue_clone;
                            let mut q = lock.lock();
                            while q.stack.is_empty() {
                                cvar.wait(&mut q);
                            }
                            let p = q.stack.pop().unwrap();
                            q.in_flight.insert(p.clone());
                            p
                        };

                        let rgba = extract_shell_icon_rgba(&path);
                        let _ = tx.send(LoadedIconData { path, rgba });
                        ctx_clone.request_repaint();
                    }
                })
                .ok();
        }

        Self {
            cache: FxHashMap::with_capacity_and_hasher(1024, Default::default()),
            queue,
            res_rx,
            fallback_app: None,
            fallback_shortcut: None,
        }
    }

    /// Intègre les icônes extraites en arrière-plan vers des textures GPU egui.
    pub fn poll_loaded(&mut self, ctx: &Context) {
        if self.fallback_app.is_none() {
            self.fallback_app = Some(ctx.load_texture(
                "fallback_app_icon",
                build_fallback_icon(false),
                TextureOptions::LINEAR,
            ));
            self.fallback_shortcut = Some(ctx.load_texture(
                "fallback_shortcut_icon",
                build_fallback_icon(true),
                TextureOptions::LINEAR,
            ));
        }

        let mut processed = 0usize;
        while let Ok(msg) = self.res_rx.try_recv() {
            {
                let (lock, _) = &*self.queue;
                lock.lock().in_flight.remove(&msg.path);
            }
            if let Some((w, h, pixels)) = msg.rgba {
                let image = ColorImage::from_rgba_unmultiplied([w, h], &pixels);
                let tex = ctx.load_texture(&msg.path, image, TextureOptions::LINEAR);
                self.cache.insert(msg.path, CacheState::Loaded(tex));
            } else {
                self.cache.insert(msg.path, CacheState::Failed);
            }
            processed += 1;
            if processed >= 64 {
                break;
            }
        }

        if !self.res_rx.is_empty() {
            ctx.request_repaint();
        }
    }

    /// Retourne la texture de l'icône du fichier si prête, ou place le chemin en tête de la pile LIFO
    /// des lignes visibles et retourne l'icône générique du type en attendant.
    pub fn get_or_request(&mut self, path: &str, kind: ItemKind) -> Option<&TextureHandle> {
        if let Some(state) = self.cache.get(path) {
            if let CacheState::Loaded(tex) = state {
                return Some(tex);
            }
            return match kind {
                ItemKind::Application => self.fallback_app.as_ref(),
                ItemKind::Shortcut => self.fallback_shortcut.as_ref(),
            };
        }

        {
            let (lock, cvar) = &*self.queue;
            let mut q = lock.lock();
            if !q.in_flight.contains(path) {
                if let Some(pos) = q.stack.iter().position(|p| p == path) {
                    q.stack.remove(pos);
                }
                q.stack.push(path.to_string());
                // Garde uniquement les 64 demandes les plus récentes (fenêtre visible)
                if q.stack.len() > 64 {
                    let excess = q.stack.len() - 64;
                    q.stack.drain(0..excess);
                }
                cvar.notify_one();
            }
        }

        match kind {
            ItemKind::Application => self.fallback_app.as_ref(),
            ItemKind::Shortcut => self.fallback_shortcut.as_ref(),
        }
    }
}

fn build_fallback_icon(is_shortcut: bool) -> ColorImage {
    let w = 16usize;
    let h = 16usize;
    let mut rgba = vec![0u8; w * h * 4];
    for y in 1..15 {
        for x in 2..14 {
            let idx = (y * w + x) * 4;
            if y <= 3 {
                rgba[idx] = if is_shortcut { 70 } else { 215 };
                rgba[idx + 1] = if is_shortcut { 140 } else { 75 };
                rgba[idx + 2] = if is_shortcut { 230 } else { 75 };
                rgba[idx + 3] = 255;
            } else if x == 2 || x == 13 || y == 14 {
                rgba[idx] = 140;
                rgba[idx + 1] = 145;
                rgba[idx + 2] = 155;
                rgba[idx + 3] = 230;
            } else {
                rgba[idx] = 235;
                rgba[idx + 1] = 238;
                rgba[idx + 2] = 242;
                rgba[idx + 3] = 215;
            }
        }
    }
    ColorImage::from_rgba_unmultiplied([w, h], &rgba)
}

/// Normalise le chemin Windows (`/` -> `\`, suppression du préfixe `\\?\`).
fn normalize_windows_path(path: &str) -> String {
    let stripped = path.strip_prefix(r"\\?\").unwrap_or(path);
    if stripped.contains('/') {
        stripped.replace('/', r"\")
    } else {
        stripped.to_string()
    }
}

fn expand_env_vars(raw: &str) -> String {
    if !raw.contains('%') {
        return raw.to_string();
    }
    let wide_in = to_wide_null(raw);
    let mut buf = [0u16; 1024];
    let len = unsafe {
        ExpandEnvironmentStringsW(wide_in.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) as usize
    };
    if len > 1 && len <= buf.len() {
        String::from_utf16_lossy(&buf[..len - 1])
    } else {
        raw.to_string()
    }
}

/// Si un raccourci `.lnk` pointe vers un `IconLocation` supprimé/inexistant (ex: désinstallation partielle),
/// mais que sa cible `TargetPath` (`.exe`) existe, retourne le chemin de l'exécutable cible.
fn resolve_lnk_fallback_target(wide_lnk_path: &[u16]) -> Option<String> {
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let persist: IPersistFile = link.cast().ok()?;
        persist
            .Load(PCWSTR(wide_lnk_path.as_ptr()), STGM_READ)
            .ok()?;

        let mut icon_buf = [0u16; 520];
        let mut icon_idx: i32 = 0;
        let icon_custom = if link.GetIconLocation(&mut icon_buf, &mut icon_idx).is_ok() {
            let nul_pos = icon_buf
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(icon_buf.len());
            if nul_pos > 0 {
                let raw_icon = String::from_utf16_lossy(&icon_buf[..nul_pos]);
                Some(expand_env_vars(raw_icon.trim_matches('"')))
            } else {
                None
            }
        } else {
            None
        };

        // Si un fichier d'icône personnalisé est défini et existe, inutile de détourner vers la cible
        if let Some(ref ic) = icon_custom
            && Path::new(ic).exists()
        {
            return None;
        }

        let mut target_buf = [0u16; 520];
        if link
            .GetPath(&mut target_buf, ptr::null_mut(), 0)
            .is_ok()
        {
            let nul_pos = target_buf
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(target_buf.len());
            if nul_pos > 0 {
                let raw_target = String::from_utf16_lossy(&target_buf[..nul_pos]);
                let expanded = expand_env_vars(raw_target.trim_matches('"'));
                // Si l'IconLocation personnalisé était manquant (ou absent) et que la cible existe :
                if icon_custom.is_some() && Path::new(&expanded).exists() {
                    return Some(expanded);
                }
            }
        }
        None
    }
}

pub fn extract_shell_icon_rgba(raw_path: &str) -> Option<(usize, usize, Vec<u8>)> {
    let clean_path = normalize_windows_path(raw_path);
    let wide = to_wide_null(&clean_path);

    // Si c'est un .lnk dont l'icône personnalisée (.ico) a été supprimée mais dont l'exécutable cible existe,
    // on extrait l'icône de l'exécutable cible plutôt qu'une feuille blanche générique.
    if clean_path
        .as_bytes()
        .get(clean_path.len().saturating_sub(4)..)
        .is_some_and(|ext| ext.eq_ignore_ascii_case(b".lnk"))
        && let Some(target_exe) = resolve_lnk_fallback_target(&wide)
    {
        let target_wide = to_wide_null(&target_exe);
        if let Some(res) = extract_from_wide_path(&target_wide) {
            return Some(res);
        }
    }

    extract_from_wide_path(&wide)
}

fn extract_from_wide_path(wide: &[u16]) -> Option<(usize, usize, Vec<u8>)> {
    // 1. Pipeline moderne Windows Vista/10/11 : IShellItemImageFactory::GetImage (32x32 ARGB)
    // Gère nativement les apps Microsoft Store / UWP, les raccourcis MSI, et les icônes PNG 256x256.
    if let Some(res) = extract_via_shell_item_factory(wide) {
        return Some(res);
    }

    // 2. Fallback SHGetFileInfoW avec composition complète (Canal Alpha 32-bit + Masque 1-bit + DrawIconEx)
    for flags in [
        SHGFI_ICON | SHGFI_LARGEICON,
        SHGFI_ICON | SHGFI_SMALLICON,
        SHGFI_ICON | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES,
    ] {
        if let Some(res) = extract_via_shgetfileinfo(wide, flags) {
            return Some(res);
        }
    }

    None
}

fn extract_via_shell_item_factory(wide_path: &[u16]) -> Option<(usize, usize, Vec<u8>)> {
    let mut factory_ptr: *mut c_void = ptr::null_mut();
    let hr = unsafe {
        SHCreateItemFromParsingName(
            wide_path.as_ptr(),
            ptr::null_mut(),
            &IID_ISHELL_ITEM_IMAGE_FACTORY,
            &mut factory_ptr,
        )
    };
    if hr < 0 || factory_ptr.is_null() {
        return None;
    }

    let factory = factory_ptr as *mut IShellItemImageFactory;
    let size = SIZE { cx: 32, cy: 32 };
    let mut hbm: HBITMAP = ptr::null_mut();

    for flags in [
        SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK,
        SIIGBF_BIGGERSIZEOK,
    ] {
        let get_hr = unsafe { ((*(*factory).vtbl).get_image)(factory_ptr, size, flags, &mut hbm) };
        if get_hr >= 0 && !hbm.is_null() {
            break;
        }
        hbm = ptr::null_mut();
    }

    unsafe {
        ((*(*factory).vtbl).base.release)(factory_ptr);
    }

    if hbm.is_null() {
        return None;
    }

    let result = hbitmap_to_rgba_with_fallback_alpha(hbm);
    unsafe {
        DeleteObject(hbm);
    }
    result
}

fn extract_via_shgetfileinfo(wide_path: &[u16], flags: u32) -> Option<(usize, usize, Vec<u8>)> {
    let mut shfi = unsafe { MaybeUninit::<SHFILEINFOW>::zeroed().assume_init() };
    let res = unsafe {
        SHGetFileInfoW(
            wide_path.as_ptr(),
            0,
            &mut shfi,
            size_of::<SHFILEINFOW>() as u32,
            flags,
        )
    };
    if res == 0 || shfi.hIcon.is_null() {
        return None;
    }

    let hicon = shfi.hIcon;
    let extracted = hicon_to_rgba(hicon);
    unsafe {
        DestroyIcon(hicon);
    }
    extracted
}

fn hicon_to_rgba(hicon: HICON) -> Option<(usize, usize, Vec<u8>)> {
    let mut icon_info = unsafe { MaybeUninit::<ICONINFO>::zeroed().assume_init() };
    if unsafe { GetIconInfo(hicon, &mut icon_info) } == 0 {
        return render_hicon_via_dib(hicon, 32, 32);
    }

    let mut width = 32usize;
    let mut height = 32usize;
    let mut result = None;

    if !icon_info.hbmColor.is_null() {
        if let Some((w, h, mut color_bgra)) = read_bitmap_bgra(icon_info.hbmColor) {
            width = w;
            height = h;
            let has_alpha = color_bgra.chunks_exact(4).any(|c| c[3] != 0);

            if has_alpha {
                for chunk in color_bgra.chunks_exact_mut(4) {
                    chunk.swap(0, 2);
                }
                result = Some((width, height, color_bgra));
            } else if !icon_info.hbmMask.is_null()
                && let Some((mw, mh, mask_bgra)) = read_bitmap_bgra(icon_info.hbmMask)
                && mw == width
                && mh >= height
            {
                let mut any_opaque = false;
                for (c_px, m_px) in color_bgra
                    .chunks_exact_mut(4)
                    .zip(mask_bgra.chunks_exact(4))
                {
                    c_px.swap(0, 2);
                    if m_px[0] == 0 {
                        c_px[3] = 255;
                        any_opaque = true;
                    } else {
                        c_px[0] = 0;
                        c_px[1] = 0;
                        c_px[2] = 0;
                        c_px[3] = 0;
                    }
                }
                if any_opaque {
                    result = Some((width, height, color_bgra));
                }
            }
        }
    }

    unsafe {
        if !icon_info.hbmColor.is_null() {
            DeleteObject(icon_info.hbmColor);
        }
        if !icon_info.hbmMask.is_null() {
            DeleteObject(icon_info.hbmMask);
        }
    }

    result.or_else(|| render_hicon_via_dib(hicon, width, height))
}

fn read_bitmap_bgra(hbm: HBITMAP) -> Option<(usize, usize, Vec<u8>)> {
    let mut bmp = unsafe { MaybeUninit::<BITMAP>::zeroed().assume_init() };
    let got_obj =
        unsafe { GetObjectW(hbm, size_of::<BITMAP>() as i32, (&mut bmp as *mut BITMAP).cast()) };
    if got_obj == 0 || bmp.bmWidth <= 0 || bmp.bmHeight == 0 {
        return None;
    }

    let width = bmp.bmWidth as usize;
    let height = bmp.bmHeight.unsigned_abs() as usize;
    let mut pixels = vec![0u8; width * height * 4];

    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        bmiColors: unsafe { MaybeUninit::zeroed().assume_init() },
    };

    let hdc = unsafe { GetDC(ptr::null_mut()) };
    let scanlines = unsafe {
        GetDIBits(
            hdc,
            hbm,
            0,
            height as u32,
            pixels.as_mut_ptr().cast(),
            &mut bmi,
            DIB_RGB_COLORS,
        )
    };
    unsafe {
        ReleaseDC(ptr::null_mut(), hdc);
    }

    if scanlines == 0 {
        return None;
    }
    Some((width, height, pixels))
}

fn hbitmap_to_rgba_with_fallback_alpha(hbm: HBITMAP) -> Option<(usize, usize, Vec<u8>)> {
    let (width, height, mut pixels) = read_bitmap_bgra(hbm)?;
    let mut has_alpha = false;
    let mut has_color = false;
    for chunk in pixels.chunks_exact_mut(4) {
        chunk.swap(0, 2);
        if chunk[3] != 0 {
            has_alpha = true;
        }
        if chunk[0] != 0 || chunk[1] != 0 || chunk[2] != 0 {
            has_color = true;
        }
    }
    if !has_alpha && !has_color {
        return None;
    }
    if !has_alpha {
        for chunk in pixels.chunks_exact_mut(4) {
            if chunk[0] != 0 || chunk[1] != 0 || chunk[2] != 0 {
                chunk[3] = 255;
            }
        }
    }
    Some((width, height, pixels))
}

fn render_hicon_via_dib(
    hicon: HICON,
    width: usize,
    height: usize,
) -> Option<(usize, usize, Vec<u8>)> {
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        bmiColors: unsafe { MaybeUninit::zeroed().assume_init() },
    };

    let screen_dc = unsafe { GetDC(ptr::null_mut()) };
    let mem_dc = unsafe { CreateCompatibleDC(screen_dc) };
    let mut bits_ptr: *mut c_void = ptr::null_mut();
    let hdib = unsafe {
        CreateDIBSection(
            mem_dc,
            &bmi,
            DIB_RGB_COLORS,
            &mut bits_ptr,
            ptr::null_mut(),
            0,
        )
    };
    unsafe {
        ReleaseDC(ptr::null_mut(), screen_dc);
    }

    if hdib.is_null() || bits_ptr.is_null() {
        unsafe {
            if !hdib.is_null() {
                DeleteObject(hdib);
            }
            DeleteDC(mem_dc);
        }
        return None;
    }

    let old_obj = unsafe { SelectObject(mem_dc, hdib) };
    let drawn = unsafe {
        DrawIconEx(
            mem_dc,
            0,
            0,
            hicon,
            width as i32,
            height as i32,
            0,
            ptr::null_mut(),
            DI_NORMAL,
        )
    };

    let byte_len = width * height * 4;
    let mut pixels = vec![0u8; byte_len];
    if drawn != 0 {
        unsafe {
            ptr::copy_nonoverlapping(bits_ptr.cast::<u8>(), pixels.as_mut_ptr(), byte_len);
        }
    }

    unsafe {
        SelectObject(mem_dc, old_obj);
        DeleteObject(hdib);
        DeleteDC(mem_dc);
    }

    if drawn == 0 {
        return None;
    }

    let mut has_alpha = false;
    let mut has_color = false;
    for chunk in pixels.chunks_exact_mut(4) {
        chunk.swap(0, 2);
        if chunk[3] != 0 {
            has_alpha = true;
        }
        if chunk[0] != 0 || chunk[1] != 0 || chunk[2] != 0 {
            has_color = true;
        }
    }
    if !has_alpha && !has_color {
        return None;
    }
    if !has_alpha {
        for chunk in pixels.chunks_exact_mut(4) {
            if chunk[0] != 0 || chunk[1] != 0 || chunk[2] != 0 {
                chunk[3] = 255;
            }
        }
    }
    Some((width, height, pixels))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lnk_fallback_and_icons() {
        unsafe {
            CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32);
        }
        let broken_icon_lnk = r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs\Miniconda3 (64-bit)\Anaconda Powershell Prompt (MiniConda3).lnk";
        if Path::new(broken_icon_lnk).exists() {
            let wide = to_wide_null(broken_icon_lnk);
            let target = resolve_lnk_fallback_target(&wide);
            println!("Resolved fallback target for Anaconda Powershell: {:?}", target);
            assert!(target.is_some());
            assert!(extract_shell_icon_rgba(broken_icon_lnk).is_some());
        }
    }
}
