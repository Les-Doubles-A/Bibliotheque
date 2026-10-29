use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use egui::{
    Align, Color32, Context, CornerRadius, CursorIcon, FontId, Frame, Id, Key, Layout, Margin,
    Modifiers, Order, Panel, PointerButton, Pos2, Rect, RichText, ScrollArea, Sense, Stroke,
    TextEdit, TextStyle, Theme, Ui, Vec2, Visuals,
};
use rustc_hash::FxHashSet;

use crate::db::LibraryDatabase;
use crate::icons::IconManager;
use crate::index::InMemoryIndex;
use crate::scanner::{ScanEvent, spawn_scan};
use crate::updater::{
    CURRENT_VERSION, UpdateEvent, UpdateInfo, cleanup_old_executable, spawn_perform_update,
    spawn_update_check,
};
use crate::win_utils::{
    open_file, os_prefers_dark_mode, reveal_in_explorer, standard_locations,
};

#[derive(Clone, Copy)]
struct ThemeColors {
    window: Color32,
    base: Color32,
    alternate: Color32,
    text: Color32,
    subtitle: Color32,
    border: Color32,
    accent: Color32,
}

impl ThemeColors {
    fn for_mode(dark: bool) -> Self {
        if dark {
            Self {
                window: Color32::from_rgb(0x1f, 0x20, 0x23),
                base: Color32::from_rgb(0x29, 0x2b, 0x2f),
                alternate: Color32::from_rgb(0x24, 0x26, 0x2a),
                text: Color32::from_rgb(0xf0, 0xf0, 0xf0),
                subtitle: Color32::from_rgb(0x9a, 0x9e, 0xa7),
                border: Color32::from_rgb(0x41, 0x44, 0x4b),
                accent: Color32::from_rgb(0xe5, 0x50, 0x50),
            }
        } else {
            Self {
                window: Color32::from_rgb(0xf5, 0xf6, 0xf8),
                base: Color32::from_rgb(0xff, 0xff, 0xff),
                alternate: Color32::from_rgb(0xf0, 0xf2, 0xf5),
                text: Color32::from_rgb(0x20, 0x21, 0x24),
                subtitle: Color32::from_rgb(0x6e, 0x72, 0x78),
                border: Color32::from_rgb(0xd5, 0xd9, 0xdf),
                accent: Color32::from_rgb(0xc6, 0x28, 0x28),
            }
        }
    }
}

enum ModalDialog {
    None,
    AddCategory { input: String, focus_requested: bool },
    ConfirmDeleteCategory(String),
    ConfirmScanDisk,
    UpdatePrompt(UpdateInfo),
    ErrorMessage { title: String, message: String },
}

pub struct BibliothequeApp {
    db: LibraryDatabase,
    index: InMemoryIndex,
    icons: IconManager,
    categories: Vec<String>,
    selected_category: String,
    search_query: String,

    // Sélection de lignes (indices dans `index.filtered_count()`)
    selected_rows: FxHashSet<usize>,
    selection_anchor: Option<usize>,
    context_menu_pos: Option<Pos2>,

    // Largeurs interactives des colonnes (Nom, Type, Categorie)
    col_widths: [f32; 3],

    // Analyse en arrière-plan
    scanning: bool,
    scan_tx: Sender<ScanEvent>,
    scan_rx: Receiver<ScanEvent>,
    status_override: Option<(String, Option<Instant>)>,

    // Mise à jour automatique
    update_tx: Sender<UpdateEvent>,
    update_rx: Receiver<UpdateEvent>,
    available_update: Option<UpdateInfo>,
    updating: bool,

    // Thème OS
    dark_mode: bool,
    colors: ThemeColors,
    last_theme_check: Instant,

    // Fenêtres modales
    modal: ModalDialog,
}

impl BibliothequeApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cleanup_old_executable();

        let db = LibraryDatabase::open().expect("Impossible d'initialiser library.db");
        let categories = db.categories();
        let mut index = InMemoryIndex::new();
        if let Ok(records) = db.load_all_items() {
            index.replace_all(records, "", "");
        }

        let dark_mode = os_prefers_dark_mode();
        let colors = ThemeColors::for_mode(dark_mode);
        apply_egui_theme(&cc.egui_ctx, dark_mode, &colors);

        let (scan_tx, scan_rx) = unbounded();
        let (update_tx, update_rx) = unbounded();
        spawn_update_check(update_tx.clone(), cc.egui_ctx.clone());

        let icons = IconManager::new(cc.egui_ctx.clone());

        Self {
            db,
            index,
            icons,
            categories,
            selected_category: String::new(),
            search_query: String::new(),
            selected_rows: FxHashSet::default(),
            selection_anchor: None,
            context_menu_pos: None,
            col_widths: [245.0, 155.0, 140.0],
            scanning: false,
            scan_tx,
            scan_rx,
            status_override: None,
            update_tx,
            update_rx,
            available_update: None,
            updating: false,
            dark_mode,
            colors,
            last_theme_check: Instant::now(),
            modal: ModalDialog::None,
        }
    }

    fn reload_from_db(&mut self) {
        if let Ok(records) = self.db.load_all_items() {
            self.index
                .replace_all(records, &self.search_query, &self.selected_category);
            self.selected_rows.clear();
            self.selection_anchor = None;
        }
    }

    fn refresh_categories(&mut self) {
        self.categories = self.db.categories();
        if !self.selected_category.is_empty()
            && !self.categories.iter().any(|c| c == &self.selected_category)
        {
            self.selected_category.clear();
        }
    }

    fn refilter(&mut self) {
        self.index.filter(&self.search_query, &self.selected_category);
        self.selected_rows.clear();
        self.selection_anchor = None;
    }

    fn start_scan(&mut self, folders: Vec<PathBuf>, ctx: &Context) {
        if self.scanning || folders.is_empty() {
            return;
        }
        self.scanning = true;
        self.status_override = Some(("Preparation de l'analyse...".to_string(), None));
        spawn_scan(folders, self.scan_tx.clone(), ctx.clone());
    }

    fn poll_background_events(&mut self, ctx: &Context) {
        self.icons.poll_loaded(ctx);

        while let Ok(ev) = self.scan_rx.try_recv() {
            match ev {
                ScanEvent::Progress(msg) => {
                    self.status_override = Some((msg, None));
                }
                ScanEvent::Completed(count) => {
                    self.scanning = false;
                    self.reload_from_db();
                    let expire = Instant::now() + Duration::from_secs(8);
                    self.status_override = Some((
                        format!("Analyse terminee : {count} element(s) ajoutes ou mis a jour."),
                        Some(expire),
                    ));
                }
                ScanEvent::Failed(err) => {
                    self.scanning = false;
                    self.status_override = None;
                    self.modal = ModalDialog::ErrorMessage {
                        title: "Analyse interrompue".to_string(),
                        message: err,
                    };
                }
            }
        }

        while let Ok(uev) = self.update_rx.try_recv() {
            match uev {
                UpdateEvent::UpdateAvailable(info) => {
                    self.available_update = Some(info.clone());
                    if matches!(self.modal, ModalDialog::None) {
                        self.modal = ModalDialog::UpdatePrompt(info);
                    }
                }
                UpdateEvent::UpToDate => {}
                UpdateEvent::DownloadProgress(msg) => {
                    self.status_override = Some((msg, None));
                }
                UpdateEvent::UpdateInstalledAndRestarting => {
                    self.updating = false;
                }
                UpdateEvent::UpdateFailed(err) => {
                    self.updating = false;
                    self.status_override = None;
                    self.modal = ModalDialog::ErrorMessage {
                        title: "Mise a jour".to_string(),
                        message: err,
                    };
                }
            }
        }

        if let Some((_, Some(deadline))) = &self.status_override
            && Instant::now() >= *deadline
        {
            self.status_override = None;
        }

        if self.last_theme_check.elapsed() >= Duration::from_secs(2) {
            self.last_theme_check = Instant::now();
            let current_dark = os_prefers_dark_mode();
            if current_dark != self.dark_mode {
                self.dark_mode = current_dark;
                self.colors = ThemeColors::for_mode(current_dark);
                apply_egui_theme(ctx, current_dark, &self.colors);
            }
        }
    }

    fn selected_paths(&self) -> Vec<String> {
        let mut rows: Vec<usize> = self.selected_rows.iter().copied().collect();
        rows.sort_unstable();
        rows.into_iter()
            .filter_map(|r| self.index.get_filtered(r).map(|item| item.path.clone()))
            .collect()
    }

    fn open_selected(&mut self) {
        let paths = self.selected_paths();
        if let Some(first) = paths.first()
            && let Err(err) = open_file(first)
        {
            self.modal = ModalDialog::ErrorMessage {
                title: "Ouverture impossible".to_string(),
                message: err,
            };
        }
    }

    fn assign_category(&mut self, paths: &[String], category: &str) {
        if let Err(err) = self.db.set_category(paths, category) {
            self.modal = ModalDialog::ErrorMessage {
                title: "Erreur SQLite".to_string(),
                message: err,
            };
            return;
        }
        self.index.set_category_in_memory(
            paths,
            category,
            &self.search_query,
            &self.selected_category,
        );
    }

    fn render_header(&mut self, ui: &mut Ui, ctx: &Context) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(
                    RichText::new("Bibliotheque")
                        .font(FontId::proportional(25.0))
                        .strong()
                        .color(self.colors.text),
                );
                ui.label(
                    RichText::new("Retrouvez rapidement vos applications et raccourcis Windows")
                        .font(FontId::proportional(13.0))
                        .color(self.colors.subtitle),
                );
            });

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;

                if ui
                    .add_enabled(
                        !self.scanning,
                        egui::Button::new("Analyser C:").min_size(Vec2::new(0.0, 32.0)),
                    )
                    .clicked()
                {
                    self.modal = ModalDialog::ConfirmScanDisk;
                }

                if ui
                    .add_enabled(
                        !self.scanning,
                        egui::Button::new("Choisir un dossier").min_size(Vec2::new(0.0, 32.0)),
                    )
                    .clicked()
                    && let Some(folder) = rfd::FileDialog::new()
                        .set_title("Choisir un dossier a analyser")
                        .pick_folder()
                {
                    self.start_scan(vec![folder], ctx);
                }

                if ui
                    .add_enabled(
                        !self.scanning,
                        egui::Button::new("Analyser les emplacements usuels")
                            .min_size(Vec2::new(0.0, 32.0)),
                    )
                    .clicked()
                {
                    self.start_scan(standard_locations(), ctx);
                }

                if let Some(ref upd) = self.available_update.clone() {
                    let btn = egui::Button::new(
                        RichText::new(format!("⬆ Mise a jour v{}", upd.version))
                            .color(Color32::WHITE)
                            .strong(),
                    )
                    .fill(self.colors.accent)
                    .min_size(Vec2::new(0.0, 32.0));
                    if ui.add_enabled(!self.updating, btn).clicked() {
                        self.modal = ModalDialog::UpdatePrompt(upd.clone());
                    }
                }
            });
        });
    }

    fn render_search_bar(&mut self, ui: &mut Ui) {
        let search_id = Id::unique("main_search_input");
        if ui.input(|i| i.modifiers.ctrl && i.key_pressed(Key::F)) {
            ui.memory_mut(|m| m.request_focus(search_id));
        }

        Frame::new()
            .fill(self.colors.base)
            .stroke(Stroke::new(1.0, self.colors.border))
            .corner_radius(CornerRadius::same(6))
            .inner_margin(Margin::symmetric(10, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let has_clear = !self.search_query.is_empty();
                    let avail_w = (ui.available_width() - if has_clear { 28.0 } else { 0.0 }).max(50.0);
                    let resp = ui.add_sized(
                        [avail_w, 24.0],
                        TextEdit::singleline(&mut self.search_query)
                            .id(search_id)
                            .frame(Frame::NONE)
                            .hint_text(
                                "Rechercher une application, un raccourci ou une categorie...",
                            ),
                    );
                    if resp.changed() {
                        self.refilter();
                    }
                    if has_clear
                        && ui
                            .add(egui::Button::new("✕").frame(false))
                            .on_hover_text("Effacer la recherche")
                            .clicked()
                    {
                        self.search_query.clear();
                        self.refilter();
                        ui.memory_mut(|m| m.request_focus(search_id));
                    }
                });
            });
    }

    fn render_categories_panel(&mut self, ui: &mut Ui) {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            ui.label(
                RichText::new("CATEGORIES")
                    .font(FontId::proportional(12.5))
                    .strong()
                    .color(self.colors.text),
            );

            let bottom_buttons_height = 38.0;
            let list_height = (ui.available_height() - bottom_buttons_height).max(80.0);

            Frame::new()
                .fill(self.colors.base)
                .stroke(Stroke::new(1.0, self.colors.border))
                .corner_radius(CornerRadius::same(6))
                .inner_margin(Margin::same(5))
                .show(ui, |ui| {
                    ui.set_min_height(list_height - 12.0);
                    ui.set_max_height(list_height - 12.0);
                    ScrollArea::vertical()
                        .id_salt("categories_scroll")
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            let all_selected = self.selected_category.is_empty();
                            if Self::category_row(
                                ui,
                                "Toutes les categories",
                                all_selected,
                                &self.colors,
                            ) && !all_selected
                            {
                                self.selected_category.clear();
                                self.refilter();
                            }

                            let cats = self.categories.clone();
                            for cat in cats {
                                let is_sel = self.selected_category == cat;
                                if Self::category_row(ui, &cat, is_sel, &self.colors) && !is_sel {
                                    self.selected_category = cat;
                                    self.refilter();
                                }
                            }
                        });
                });

            ui.horizontal(|ui| {
                let half_w = ((ui.available_width() - 6.0) * 0.5).max(40.0);
                if ui
                    .add_sized([half_w, 32.0], egui::Button::new("+ Categorie"))
                    .clicked()
                {
                    self.modal = ModalDialog::AddCategory {
                        input: String::new(),
                        focus_requested: false,
                    };
                }
                let can_delete = !self.selected_category.is_empty();
                if ui
                    .add_enabled_ui(can_delete, |ui| {
                        ui.add_sized([half_w, 32.0], egui::Button::new("Supprimer"))
                    })
                    .inner
                    .clicked()
                {
                    self.modal = ModalDialog::ConfirmDeleteCategory(self.selected_category.clone());
                }
            });
        });
    }

    fn category_row(ui: &mut Ui, label: &str, selected: bool, colors: &ThemeColors) -> bool {
        let desired = Vec2::new(ui.available_width(), 26.0);
        let (rect, resp) = ui.allocate_exact_size(desired, Sense::click());
        if ui.is_rect_visible(rect) {
            let bg = if selected {
                colors.accent
            } else if resp.hovered() {
                colors.alternate
            } else {
                Color32::TRANSPARENT
            };
            if bg != Color32::TRANSPARENT {
                ui.painter().rect_filled(rect, CornerRadius::same(4), bg);
            }
            let text_color = if selected { Color32::WHITE } else { colors.text };
            ui.painter().text(
                Pos2::new(rect.left() + 8.0, rect.center().y),
                egui::Align2::LEFT_CENTER,
                label,
                FontId::proportional(13.5),
                text_color,
            );
        }
        resp.clicked()
    }

    fn render_table(&mut self, ui: &mut Ui) {
        // Raccourcis clavier sur la table : Entrée pour ouvrir, Ctrl+A pour tout sélectionner
        let no_modal = matches!(self.modal, ModalDialog::None);
        if no_modal && !ui.ctx().egui_wants_keyboard_input() {
            if ui.input(|i| i.key_pressed(Key::Enter)) && !self.selected_rows.is_empty() {
                self.open_selected();
            }
            if ui.input(|i| i.modifiers.ctrl && i.key_pressed(Key::A)) {
                self.selected_rows = (0..self.index.filtered_count()).collect();
            }
        }

        Frame::new()
            .fill(self.colors.base)
            .stroke(Stroke::new(1.0, self.colors.border))
            .corner_radius(CornerRadius::same(6))
            .inner_margin(Margin::same(1))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let total_width = ui.available_width();
                let header_height = 32.0;

                // En-tête de table avec redimensionnement interactif des colonnes
                let (header_rect, _) = ui.allocate_exact_size(
                    Vec2::new(total_width, header_height),
                    Sense::hover(),
                );
                ui.painter()
                    .rect_filled(header_rect, CornerRadius::ZERO, self.colors.alternate);
                ui.painter().line_segment(
                    [
                        Pos2::new(header_rect.left(), header_rect.bottom()),
                        Pos2::new(header_rect.right(), header_rect.bottom()),
                    ],
                    Stroke::new(1.0, self.colors.border),
                );

                let headers = ["Nom", "Type", "Categorie", "Emplacement"];
                let mut x_offset = header_rect.left();
                let mut col_rects = [Rect::NOTHING; 4];

                for i in 0..4 {
                    let w = if i < 3 {
                        self.col_widths[i]
                    } else {
                        (header_rect.right() - x_offset).max(80.0)
                    };
                    let rect = Rect::from_min_size(
                        Pos2::new(x_offset, header_rect.top()),
                        Vec2::new(w, header_height),
                    );
                    col_rects[i] = rect;

                    ui.painter().with_clip_rect(rect).text(
                        Pos2::new(rect.left() + 8.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        headers[i],
                        FontId::proportional(13.5),
                        self.colors.text,
                    );

                    if i < 3 {
                        let sep_x = rect.right();
                        let sep_rect = Rect::from_center_size(
                            Pos2::new(sep_x, header_rect.center().y),
                            Vec2::new(8.0, header_height),
                        );
                        let sep_resp = ui.interact(
                            sep_rect,
                            Id::unique(("col_resize", i)),
                            Sense::drag(),
                        );
                        if sep_resp.hovered() || sep_resp.dragged() {
                            ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
                        }
                        if sep_resp.dragged() {
                            self.col_widths[i] =
                                (self.col_widths[i] + sep_resp.drag_delta().x).clamp(80.0, 600.0);
                        }
                        ui.painter().line_segment(
                            [
                                Pos2::new(sep_x, header_rect.top() + 6.0),
                                Pos2::new(sep_x, header_rect.bottom() - 6.0),
                            ],
                            Stroke::new(1.0, self.colors.border),
                        );
                    }

                    x_offset += w;
                }

                // Corps de table virtualisé : seules les ~25 lignes visibles sont dessinées
                let row_height = 28.0;
                let row_count = self.index.filtered_count();
                let modifiers = ui.input(|i| i.modifiers);

                let mut double_clicked = false;
                let mut right_clicked_pos: Option<(usize, Pos2)> = None;
                let mut clicked_row: Option<usize> = None;

                ScrollArea::vertical()
                    .id_salt("library_table_scroll")
                    .auto_shrink([false, false])
                    .show_rows(ui, row_height, row_count, |ui, row_range| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        let avail_w = ui.available_width();

                        for row_idx in row_range {
                            let (row_rect, resp) = ui.allocate_exact_size(
                                Vec2::new(avail_w, row_height),
                                Sense::click(),
                            );

                            let is_selected = self.selected_rows.contains(&row_idx);
                            let bg = if is_selected {
                                self.colors.accent
                            } else if resp.hovered() {
                                self.colors.alternate.gamma_multiply(1.05)
                            } else if row_idx % 2 == 1 {
                                self.colors.alternate
                            } else {
                                self.colors.base
                            };

                            ui.painter().rect_filled(row_rect, CornerRadius::ZERO, bg);

                            if let Some(item) = self.index.get_filtered(row_idx) {
                                let text_col = if is_selected {
                                    Color32::WHITE
                                } else {
                                    self.colors.text
                                };

                                let mut cx = row_rect.left();
                                for col_i in 0..4 {
                                    let cw = if col_i < 3 {
                                        self.col_widths[col_i]
                                    } else {
                                        (row_rect.right() - cx).max(60.0)
                                    };
                                    let cell_rect = Rect::from_min_size(
                                        Pos2::new(cx, row_rect.top()),
                                        Vec2::new(cw - 4.0, row_height),
                                    );
                                    let painter = ui.painter().with_clip_rect(cell_rect);

                                    match col_i {
                                        0 => {
                                            let mut text_x = cell_rect.left() + 8.0;
                                            if let Some(tex) =
                                                self.icons.get_or_request(&item.path, item.kind)
                                            {
                                                let icon_rect = Rect::from_min_size(
                                                    Pos2::new(text_x, cell_rect.center().y - 8.0),
                                                    Vec2::new(16.0, 16.0),
                                                );
                                                painter.image(
                                                    tex.id(),
                                                    icon_rect,
                                                    Rect::from_min_max(
                                                        Pos2::new(0.0, 0.0),
                                                        Pos2::new(1.0, 1.0),
                                                    ),
                                                    Color32::WHITE,
                                                );
                                                text_x += 22.0;
                                            }
                                            painter.text(
                                                Pos2::new(text_x, cell_rect.center().y),
                                                egui::Align2::LEFT_CENTER,
                                                &item.name,
                                                FontId::proportional(13.5),
                                                text_col,
                                            );
                                        }
                                        1 => {
                                            painter.text(
                                                Pos2::new(
                                                    cell_rect.left() + 8.0,
                                                    cell_rect.center().y,
                                                ),
                                                egui::Align2::LEFT_CENTER,
                                                item.kind.as_str(),
                                                FontId::proportional(13.5),
                                                text_col,
                                            );
                                        }
                                        2 => {
                                            painter.text(
                                                Pos2::new(
                                                    cell_rect.left() + 8.0,
                                                    cell_rect.center().y,
                                                ),
                                                egui::Align2::LEFT_CENTER,
                                                &item.category,
                                                FontId::proportional(13.5),
                                                text_col,
                                            );
                                        }
                                        _ => {
                                            painter.text(
                                                Pos2::new(
                                                    cell_rect.left() + 8.0,
                                                    cell_rect.center().y,
                                                ),
                                                egui::Align2::LEFT_CENTER,
                                                &item.path,
                                                FontId::proportional(13.5),
                                                text_col,
                                            );
                                        }
                                    }

                                    cx += cw;
                                }

                                if resp.hovered() {
                                    resp.clone().on_hover_text(&item.path);
                                }
                            }

                            if resp.double_clicked() {
                                self.selected_rows.clear();
                                self.selected_rows.insert(row_idx);
                                self.selection_anchor = Some(row_idx);
                                double_clicked = true;
                            } else if resp.clicked() {
                                clicked_row = Some(row_idx);
                            }

                            if resp.secondary_clicked()
                                && let Some(pos) = ui.input(|i| i.pointer.interact_pos())
                            {
                                right_clicked_pos = Some((row_idx, pos));
                            }
                        }
                    });

                if let Some(row_idx) = clicked_row {
                    self.handle_row_selection(row_idx, modifiers);
                    self.context_menu_pos = None;
                }

                if let Some((row_idx, pos)) = right_clicked_pos {
                    if !self.selected_rows.contains(&row_idx) {
                        self.selected_rows.clear();
                        self.selected_rows.insert(row_idx);
                        self.selection_anchor = Some(row_idx);
                    }
                    self.context_menu_pos = Some(pos);
                }

                if double_clicked {
                    self.open_selected();
                }
            });

        self.render_context_menu(ui.ctx());
    }

    fn handle_row_selection(&mut self, row_idx: usize, modifiers: Modifiers) {
        if modifiers.shift
            && let Some(anchor) = self.selection_anchor
        {
            if !modifiers.ctrl {
                self.selected_rows.clear();
            }
            let start = anchor.min(row_idx);
            let end = anchor.max(row_idx);
            for r in start..=end {
                self.selected_rows.insert(r);
            }
        } else if modifiers.ctrl {
            if !self.selected_rows.remove(&row_idx) {
                self.selected_rows.insert(row_idx);
            }
            self.selection_anchor = Some(row_idx);
        } else {
            self.selected_rows.clear();
            self.selected_rows.insert(row_idx);
            self.selection_anchor = Some(row_idx);
        }
    }

    fn render_context_menu(&mut self, ctx: &Context) {
        let Some(pos) = self.context_menu_pos else {
            return;
        };
        if self.selected_rows.is_empty() {
            self.context_menu_pos = None;
            return;
        }

        let mut close_menu = false;
        let area_resp = egui::Area::new(Id::unique("table_item_context_menu"))
            .order(Order::Foreground)
            .fixed_pos(pos)
            .show(ctx, |ui| {
                Frame::popup(ui.style())
                    .fill(self.colors.base)
                    .stroke(Stroke::new(1.0, self.colors.border))
                    .corner_radius(CornerRadius::same(6))
                    .inner_margin(Margin::same(6))
                    .show(ui, |ui| {
                        ui.set_min_width(210.0);
                        if ui
                            .add_sized(
                                [ui.available_width(), 26.0],
                                egui::Button::new("Ouvrir").frame(false),
                            )
                            .clicked()
                        {
                            self.open_selected();
                            close_menu = true;
                        }

                        if ui
                            .add_sized(
                                [ui.available_width(), 26.0],
                                egui::Button::new("Ouvrir l'emplacement du fichier").frame(false),
                            )
                            .clicked()
                        {
                            if let Some(first) = self.selected_paths().first() {
                                reveal_in_explorer(first);
                            }
                            close_menu = true;
                        }

                        ui.separator();
                        ui.label(
                            RichText::new("Placer dans une categorie")
                                .font(FontId::proportional(12.0))
                                .color(self.colors.subtitle),
                        );

                        if ui
                            .add_sized(
                                [ui.available_width(), 25.0],
                                egui::Button::new("Aucune categorie").frame(false),
                            )
                            .clicked()
                        {
                            let paths = self.selected_paths();
                            self.assign_category(&paths, "");
                            close_menu = true;
                        }

                        let cats = self.categories.clone();
                        for cat in cats {
                            if ui
                                .add_sized(
                                    [ui.available_width(), 25.0],
                                    egui::Button::new(&cat).frame(false),
                                )
                                .clicked()
                            {
                                let paths = self.selected_paths();
                                self.assign_category(&paths, &cat);
                                close_menu = true;
                            }
                        }
                    });
            });

        if ctx.input(|i| {
            i.key_pressed(Key::Escape)
                || (i.pointer.any_pressed()
                    && !i.pointer.button_down(PointerButton::Secondary)
                    && i.pointer
                        .interact_pos()
                        .is_some_and(|p| !area_resp.response.rect.contains(p)))
        }) {
            close_menu = true;
        }

        if close_menu {
            self.context_menu_pos = None;
        }
    }

    fn render_modals(&mut self, ctx: &Context) {
        if matches!(self.modal, ModalDialog::None) {
            return;
        }

        // Fond assombri semi-transparent
        let screen_rect = ctx.content_rect();
        egui::Area::new(Id::unique("modal_backdrop"))
            .order(Order::Middle)
            .fixed_pos(screen_rect.min)
            .show(ctx, |ui| {
                let (rect, _) = ui.allocate_exact_size(screen_rect.size(), Sense::click());
                ui.painter()
                    .rect_filled(rect, CornerRadius::ZERO, Color32::from_black_alpha(120));
            });

        let mut next_modal: Option<ModalDialog> = None;
        let mut category_to_add: Option<String> = None;
        let mut category_to_delete: Option<String> = None;
        let mut confirm_scan_disk = false;
        let mut trigger_update: Option<UpdateInfo> = None;

        match &mut self.modal {
            ModalDialog::None => {}
            ModalDialog::AddCategory {
                input,
                focus_requested,
            } => {
                egui::Window::new("Nouvelle categorie")
                    .order(Order::Foreground)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.set_min_width(320.0);
                        ui.label("Nom de la categorie :");
                        ui.add_space(4.0);
                        let resp = ui.add_sized(
                            [ui.available_width(), 28.0],
                            TextEdit::singleline(input),
                        );
                        if !*focus_requested {
                            resp.request_focus();
                            *focus_requested = true;
                        }
                        ui.add_space(10.0);

                        let enter_pressed = ui.input(|i| i.key_pressed(Key::Enter));
                        let esc_pressed = ui.input(|i| i.key_pressed(Key::Escape));

                        ui.horizontal(|ui| {
                            if ui.button("OK").clicked() || enter_pressed {
                                let name = input.trim().to_string();
                                if !name.is_empty() {
                                    category_to_add = Some(name);
                                }
                                next_modal = Some(ModalDialog::None);
                            }
                            if ui.button("Annuler").clicked() || esc_pressed {
                                next_modal = Some(ModalDialog::None);
                            }
                        });
                    });
            }
            ModalDialog::ConfirmDeleteCategory(cat_name) => {
                let cat_clone = cat_name.clone();
                egui::Window::new("Supprimer")
                    .order(Order::Foreground)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.set_min_width(320.0);
                        ui.label(format!("Supprimer la categorie « {cat_clone} » ?"));
                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            if ui.button("Oui").clicked() {
                                category_to_delete = Some(cat_clone);
                                next_modal = Some(ModalDialog::None);
                            }
                            if ui.button("Non").clicked()
                                || ui.input(|i| i.key_pressed(Key::Escape))
                            {
                                next_modal = Some(ModalDialog::None);
                            }
                        });
                    });
            }
            ModalDialog::ConfirmScanDisk => {
                egui::Window::new("Analyser C:")
                    .order(Order::Foreground)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.set_min_width(360.0);
                        ui.label("L'analyse complete peut prendre plusieurs minutes. Continuer ?");
                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            if ui.button("Oui").clicked() {
                                confirm_scan_disk = true;
                                next_modal = Some(ModalDialog::None);
                            }
                            if ui.button("Non").clicked()
                                || ui.input(|i| i.key_pressed(Key::Escape))
                            {
                                next_modal = Some(ModalDialog::None);
                            }
                        });
                    });
            }
            ModalDialog::UpdatePrompt(info) => {
                let info_clone = info.clone();
                egui::Window::new("Nouvelle version disponible")
                    .order(Order::Foreground)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.set_min_width(370.0);
                        ui.label(format!(
                            "Une nouvelle version de Bibliotheque (v{}) est disponible (version actuelle : v{}).",
                            info_clone.version, CURRENT_VERSION
                        ));
                        ui.add_space(4.0);
                        ui.label("Voulez-vous telecharger et installer la mise a jour maintenant ?");
                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            if ui.button("Mettre a jour maintenant").clicked() {
                                trigger_update = Some(info_clone);
                                next_modal = Some(ModalDialog::None);
                            }
                            if ui.button("Plus tard").clicked()
                                || ui.input(|i| i.key_pressed(Key::Escape))
                            {
                                next_modal = Some(ModalDialog::None);
                            }
                        });
                    });
            }
            ModalDialog::ErrorMessage { title, message } => {
                let title_clone = title.clone();
                let msg_clone = message.clone();
                egui::Window::new(title_clone)
                    .order(Order::Foreground)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.set_min_width(340.0);
                        ui.label(msg_clone);
                        ui.add_space(12.0);
                        if ui.button("OK").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                            next_modal = Some(ModalDialog::None);
                        }
                    });
            }
        }

        if let Some(name) = category_to_add {
            let _ = self.db.add_category(&name);
            self.refresh_categories();
        }
        if let Some(cat_clone) = category_to_delete {
            let _ = self.db.delete_category(&cat_clone);
            self.refresh_categories();
            self.index.clear_category_in_memory(
                &cat_clone,
                &self.search_query,
                &self.selected_category,
            );
        }
        if confirm_scan_disk {
            self.start_scan(vec![PathBuf::from(r"C:\")], ctx);
        }
        if let Some(info) = trigger_update {
            self.updating = true;
            spawn_perform_update(info, self.update_tx.clone(), ctx.clone());
        }
        if let Some(m) = next_modal {
            self.modal = m;
        }
    }
}

impl eframe::App for BibliothequeApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_background_events(&ctx);

        // Barre d'état en bas
        Panel::bottom("status_bar")
            .exact_size(28.0)
            .show_separator_line(false)
            .frame(
                Frame::new()
                    .fill(self.colors.window)
                    .inner_margin(Margin::symmetric(18, 4)),
            )
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    let msg = if let Some((text, _)) = &self.status_override {
                        text.clone()
                    } else {
                        format!(
                            "{} resultat(s) affiches — {} element(s) indexes",
                            self.index.filtered_count(),
                            self.index.total_count()
                        )
                    };
                    ui.label(
                        RichText::new(msg)
                            .font(FontId::proportional(12.5))
                            .color(self.colors.subtitle),
                    );
                });
            });

        // En-tête + Barre de recherche en haut
        Panel::top("top_header")
            .show_separator_line(false)
            .frame(
                Frame::new()
                    .fill(self.colors.window)
                    .inner_margin(Margin {
                        left: 18,
                        right: 18,
                        top: 16,
                        bottom: 10,
                    }),
            )
            .show(ui, |ui| {
                self.render_header(ui, &ctx);
                ui.add_space(10.0);
                self.render_search_bar(ui);
            });

        // Panneau latéral redimensionnable des catégories (Splitter)
        Panel::left("categories_side_panel")
            .resizable(true)
            .default_size(210.0)
            .size_range(160.0..=380.0)
            .show_separator_line(false)
            .frame(
                Frame::new()
                    .fill(self.colors.window)
                    .inner_margin(Margin {
                        left: 18,
                        right: 8,
                        top: 0,
                        bottom: 8,
                    }),
            )
            .show(ui, |ui| {
                self.render_categories_panel(ui);
            });

        // Table principale au centre
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(self.colors.window)
                    .inner_margin(Margin {
                        left: 6,
                        right: 18,
                        top: 0,
                        bottom: 8,
                    }),
            )
            .show(ui, |ui| {
                self.render_table(ui);
            });

        self.render_modals(&ctx);
    }
}

fn apply_egui_theme(ctx: &Context, dark: bool, colors: &ThemeColors) {
    let theme = if dark { Theme::Dark } else { Theme::Light };
    ctx.set_theme(theme);
    let mut style = (*ctx.style_of(theme)).clone();
    let mut visuals = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };

    visuals.panel_fill = colors.window;
    visuals.window_fill = colors.window;
    visuals.extreme_bg_color = colors.base;
    visuals.faint_bg_color = colors.alternate;
    visuals.override_text_color = Some(colors.text);

    visuals.selection.bg_fill = colors.accent;
    visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);

    let corner = CornerRadius::same(6);
    visuals.widgets.noninteractive.bg_fill = colors.base;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, colors.border);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, colors.text);
    visuals.widgets.noninteractive.corner_radius = corner;

    visuals.widgets.inactive.bg_fill = colors.base;
    visuals.widgets.inactive.weak_bg_fill = colors.base;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, colors.border);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, colors.text);
    visuals.widgets.inactive.corner_radius = corner;

    visuals.widgets.hovered.bg_fill = colors.base;
    visuals.widgets.hovered.weak_bg_fill = colors.base;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.5, colors.accent);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, colors.text);
    visuals.widgets.hovered.corner_radius = corner;

    visuals.widgets.active.bg_fill = colors.accent;
    visuals.widgets.active.weak_bg_fill = colors.accent;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, colors.accent);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    visuals.widgets.active.corner_radius = corner;

    style.visuals = visuals;
    style.spacing.button_padding = Vec2::new(10.0, 7.0);
    style.text_styles.insert(
        TextStyle::Body,
        FontId::proportional(13.5),
    );
    style.text_styles.insert(
        TextStyle::Button,
        FontId::proportional(13.5),
    );
    ctx.set_style_of(theme, style);
}
