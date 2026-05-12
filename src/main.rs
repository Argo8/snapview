#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

use eframe::egui;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

const SUPPORTED_EXTS: &[&str] = &["jpg", "jpeg", "png", "bmp", "gif", "webp", "tif", "tiff"];
const FAVORITES_FILE: &str = ".favorites.txt";
const PRELOAD_RADIUS: usize = 2;

fn main() -> Result<(), eframe::Error> {
    let args: Vec<String> = std::env::args().collect();
    let initial_path = args.get(1).map(PathBuf::from);

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1200.0, 800.0])
        .with_decorations(false)
        .with_transparent(true)
        .with_title("snapview");

    if let Some(icon) = load_app_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        vsync: true,
        ..Default::default()
    };

    eframe::run_native(
        "snapview",
        options,
        Box::new(|cc| Ok(Box::new(SnapView::new(cc, initial_path)))),
    )
}

#[derive(Clone)]
enum LoadedImage {
    Loading,
    Ready(Arc<egui::ColorImage>),
    Failed,
}

#[derive(Default)]
struct PendingActions {
    next: bool,
    prev: bool,
    rot_left: bool,
    rot_right: bool,
    toggle_fav: bool,
    open_folder: bool,
    toggle_max: bool,
    quit: bool,
    open_filter: bool,
    cycle_filter: bool,
    delete: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FilterMode {
    All,
    Favorites,
    NonFavorites,
}

impl FilterMode {
    fn next(self) -> Self {
        match self {
            FilterMode::All => FilterMode::Favorites,
            FilterMode::Favorites => FilterMode::NonFavorites,
            FilterMode::NonFavorites => FilterMode::All,
        }
    }
    fn label(self) -> &'static str {
        match self {
            FilterMode::All => "all",
            FilterMode::Favorites => "favs only",
            FilterMode::NonFavorites => "non-favs only",
        }
    }
}

struct SnapView {
    images: Vec<PathBuf>,
    current: usize,
    favorites: HashSet<PathBuf>,
    folder: Option<PathBuf>,

    cache: Arc<Mutex<HashMap<PathBuf, LoadedImage>>>,
    textures: HashMap<PathBuf, egui::TextureHandle>,
    rotation: HashMap<PathBuf, i32>,

    load_tx: mpsc::Sender<LoadJob>,
    result_rx: mpsc::Receiver<(PathBuf, LoadedImage)>,

    show_filter: bool,
    filter_selected: HashSet<PathBuf>,

    is_maximized: bool,
    actions: PendingActions,

    filter_mode: FilterMode,
    filter_msg: Option<(String, f32)>,

    touch_swipe: Option<TouchSwipe>,
}

#[derive(Clone, Copy)]
struct TouchSwipe {
    id: egui::TouchId,
    start: egui::Pos2,
    last: egui::Pos2,
    triggered: bool,
}

struct LoadJob {
    path: PathBuf,
}

impl SnapView {
    fn new(cc: &eframe::CreationContext<'_>, initial_path: Option<PathBuf>) -> Self {
        let (load_tx, load_rx) = mpsc::channel::<LoadJob>();
        let (result_tx, result_rx) = mpsc::channel::<(PathBuf, LoadedImage)>();
        let load_rx = Arc::new(Mutex::new(load_rx));

        let n_workers = num_workers();
        for _ in 0..n_workers {
            let rx = Arc::clone(&load_rx);
            let tx = result_tx.clone();
            let ctx = cc.egui_ctx.clone();
            thread::spawn(move || loop {
                let job = {
                    let lock = rx.lock().unwrap();
                    match lock.recv() {
                        Ok(j) => j,
                        Err(_) => break,
                    }
                };
                let result = decode_image(&job.path);
                let _ = tx.send((job.path, result));
                ctx.request_repaint();
            });
        }

        let mut app = Self {
            images: Vec::new(),
            current: 0,
            favorites: HashSet::new(),
            folder: None,
            cache: Arc::new(Mutex::new(HashMap::new())),
            textures: HashMap::new(),
            rotation: HashMap::new(),
            load_tx,
            result_rx,
            show_filter: false,
            filter_selected: HashSet::new(),
            is_maximized: false,
            actions: PendingActions::default(),
            filter_mode: FilterMode::All,
            filter_msg: None,
            touch_swipe: None,
        };

        if let Some(p) = initial_path {
            if p.is_file() {
                app.open_folder_with_file(&p);
            } else if p.is_dir() {
                app.open_folder(&p, None);
            }
        }

        app
    }

    fn open_folder_with_file(&mut self, file: &Path) {
        if let Some(parent) = file.parent() {
            self.open_folder(parent, Some(file.to_path_buf()));
        }
    }

    fn open_folder(&mut self, folder: &Path, select: Option<PathBuf>) {
        let mut images: Vec<PathBuf> = std::fs::read_dir(folder)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| is_image(p))
                    .collect()
            })
            .unwrap_or_default();
        images.sort();

        self.folder = Some(folder.to_path_buf());
        self.favorites = load_favorites(folder, &images);
        self.images = images;
        self.cache.lock().unwrap().clear();
        self.textures.clear();
        self.rotation.clear();

        self.current = if let Some(sel) = select {
            self.images.iter().position(|p| p == &sel).unwrap_or(0)
        } else {
            0
        };

        self.queue_preload();
    }

    fn queue_preload(&self) {
        if self.images.is_empty() { return; }
        let n = self.images.len();
        let cur = self.current;

        let mut to_queue: Vec<PathBuf> = Vec::new();
        let mut cache = self.cache.lock().unwrap();

        let mut consider = |to_queue: &mut Vec<PathBuf>, cache: &mut HashMap<PathBuf, LoadedImage>, p: &PathBuf| {
            if !cache.contains_key(p) {
                cache.insert(p.clone(), LoadedImage::Loading);
                to_queue.push(p.clone());
            }
        };

        consider(&mut to_queue, &mut cache, &self.images[cur]);
        for d in 1..=PRELOAD_RADIUS {
            if cur + d < n { consider(&mut to_queue, &mut cache, &self.images[cur + d]); }
            if cur >= d { consider(&mut to_queue, &mut cache, &self.images[cur - d]); }
        }
        drop(cache);

        for p in to_queue {
            let _ = self.load_tx.send(LoadJob { path: p });
        }
    }

    fn drain_results(&mut self, ctx: &egui::Context) {
        while let Ok((path, result)) = self.result_rx.try_recv() {
            let mut cache = self.cache.lock().unwrap();
            cache.insert(path.clone(), result.clone());
            drop(cache);

            if let LoadedImage::Ready(ci) = result {
                if self.is_near_current(&path) {
                    let tex = ctx.load_texture(
                        path.to_string_lossy().to_string(),
                        (*ci).clone(),
                        egui::TextureOptions::LINEAR,
                    );
                    self.textures.insert(path, tex);
                }
            }
        }

        if !self.images.is_empty() {
            let near: HashSet<PathBuf> = self.near_current_paths();
            self.textures.retain(|p, _| near.contains(p));
        }
    }

    fn near_current_paths(&self) -> HashSet<PathBuf> {
        let mut s = HashSet::new();
        if self.images.is_empty() { return s; }
        let n = self.images.len();
        let cur = self.current;
        s.insert(self.images[cur].clone());
        for d in 1..=PRELOAD_RADIUS {
            if cur + d < n { s.insert(self.images[cur + d].clone()); }
            if cur >= d { s.insert(self.images[cur - d].clone()); }
        }
        s
    }

    fn is_near_current(&self, p: &Path) -> bool {
        self.near_current_paths().iter().any(|x| x == p)
    }

    fn ensure_texture(&mut self, ctx: &egui::Context, path: &Path) {
        if self.textures.contains_key(path) { return; }
        let cache = self.cache.lock().unwrap();
        if let Some(LoadedImage::Ready(ci)) = cache.get(path) {
            let ci = ci.clone();
            drop(cache);
            let tex = ctx.load_texture(
                path.to_string_lossy().to_string(),
                (*ci).clone(),
                egui::TextureOptions::LINEAR,
            );
            self.textures.insert(path.to_path_buf(), tex);
        }
    }

    fn matches_filter(&self, p: &Path) -> bool {
        match self.filter_mode {
            FilterMode::All => true,
            FilterMode::Favorites => self.favorites.contains(p),
            FilterMode::NonFavorites => !self.favorites.contains(p),
        }
    }

    fn visible_count(&self) -> usize {
        self.images.iter().filter(|p| self.matches_filter(p)).count()
    }

    fn visible_position(&self) -> Option<usize> {
        let cur = self.images.get(self.current)?;
        if !self.matches_filter(cur) { return None; }
        Some(
            self.images[..=self.current]
                .iter()
                .filter(|p| self.matches_filter(p))
                .count()
                - 1,
        )
    }

    fn step(&mut self, forward: bool) {
        if self.images.is_empty() { return; }
        let n = self.images.len();
        if self.visible_count() == 0 { return; }
        let mut idx = self.current;
        for _ in 0..n {
            idx = if forward {
                (idx + 1) % n
            } else if idx == 0 {
                n - 1
            } else {
                idx - 1
            };
            if self.matches_filter(&self.images[idx]) {
                self.current = idx;
                self.queue_preload();
                return;
            }
        }
    }

    fn next(&mut self) { self.step(true); }
    fn prev(&mut self) { self.step(false); }

    fn cycle_filter(&mut self) {
        self.filter_mode = self.filter_mode.next();
        let count = self.visible_count();
        self.filter_msg = Some((
            format!("Filter: {}  ({} images)", self.filter_mode.label(), count),
            1.5,
        ));
        if count == 0 { return; }
        let cur_path = self.images.get(self.current).cloned();
        let on_visible = cur_path.as_deref().map(|p| self.matches_filter(p)).unwrap_or(false);
        if !on_visible {
            // jump to nearest visible image (forward first)
            let n = self.images.len();
            let start = self.current;
            for d in 1..=n {
                let f = (start + d) % n;
                if self.matches_filter(&self.images[f]) {
                    self.current = f;
                    break;
                }
            }
        }
        self.queue_preload();
    }

    fn delete_current(&mut self) {
        let path = match self.current_path() {
            Some(p) => p,
            None => return,
        };
        let ok = trash::delete(&path).is_ok();
        if !ok {
            self.filter_msg = Some(("Delete failed".to_string(), 2.0));
            return;
        }
        let was_fav = self.favorites.remove(&path);
        if was_fav {
            if let Some(folder) = &self.folder {
                save_favorites(folder, &self.favorites);
            }
        }
        self.cache.lock().unwrap().remove(&path);
        self.textures.remove(&path);
        self.rotation.remove(&path);
        self.images.remove(self.current);

        if self.images.is_empty() {
            self.current = 0;
        } else {
            if self.current >= self.images.len() {
                self.current = self.images.len() - 1;
            }
            // ensure current points to a visible image (if any visible)
            if self.visible_count() > 0 && !self.matches_filter(&self.images[self.current]) {
                let n = self.images.len();
                let start = self.current;
                for d in 0..n {
                    let f = (start + d) % n;
                    if self.matches_filter(&self.images[f]) {
                        self.current = f;
                        break;
                    }
                }
            }
        }
        self.filter_msg = Some(("Moved to trash".to_string(), 1.2));
        self.queue_preload();
    }

    fn rotate(&mut self, delta: i32) {
        if let Some(p) = self.current_path() {
            let entry = self.rotation.entry(p).or_insert(0);
            *entry = (*entry + delta).rem_euclid(4);
        }
    }

    fn toggle_favorite(&mut self) {
        let p = match self.current_path() {
            Some(p) => p,
            None => return,
        };
        if self.favorites.contains(&p) { self.favorites.remove(&p); }
        else { self.favorites.insert(p); }
        if let Some(folder) = &self.folder {
            save_favorites(folder, &self.favorites);
        }
    }

    fn current_path(&self) -> Option<PathBuf> {
        self.images.get(self.current).cloned()
    }

    fn copy_filtered(&self, dest: &Path) -> std::io::Result<usize> {
        std::fs::create_dir_all(dest)?;
        let mut count = 0;
        for src in &self.filter_selected {
            if let Some(name) = src.file_name() {
                let target = dest.join(name);
                std::fs::copy(src, &target)?;
                count += 1;
            }
        }
        Ok(count)
    }

    fn cut_filtered(&mut self, dest: &Path) -> std::io::Result<usize> {
        std::fs::create_dir_all(dest)?;
        let mut moved: Vec<PathBuf> = Vec::new();
        for src in &self.filter_selected {
            let Some(name) = src.file_name() else { continue };
            let target = dest.join(name);
            if std::fs::rename(src, &target).is_err() {
                std::fs::copy(src, &target)?;
                std::fs::remove_file(src)?;
            }
            moved.push(src.clone());
        }
        let count = moved.len();
        if count == 0 {
            return Ok(0);
        }
        let cur_path = self.current_path();
        for p in &moved {
            self.favorites.remove(p);
            self.filter_selected.remove(p);
            self.cache.lock().unwrap().remove(p);
            self.textures.remove(p);
            self.rotation.remove(p);
        }
        self.images.retain(|p| !moved.contains(p));
        if let Some(folder) = &self.folder {
            save_favorites(folder, &self.favorites);
        }
        if self.images.is_empty() {
            self.current = 0;
        } else {
            self.current = cur_path
                .as_ref()
                .and_then(|p| self.images.iter().position(|x| x == p))
                .unwrap_or_else(|| self.current.min(self.images.len() - 1));
            if self.visible_count() > 0 && !self.matches_filter(&self.images[self.current]) {
                let n = self.images.len();
                let start = self.current;
                for d in 0..n {
                    let f = (start + d) % n;
                    if self.matches_filter(&self.images[f]) {
                        self.current = f;
                        break;
                    }
                }
            }
        }
        self.queue_preload();
        Ok(count)
    }
}

impl eframe::App for SnapView {
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_results(ctx);

        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));

        // Capture keyboard input
        ctx.input(|i| {
            if !self.show_filter {
                if i.key_pressed(egui::Key::ArrowRight) { self.actions.next = true; }
                if i.key_pressed(egui::Key::ArrowLeft) { self.actions.prev = true; }
                if i.key_pressed(egui::Key::Q) { self.actions.rot_left = true; }
                if i.key_pressed(egui::Key::W) { self.actions.rot_right = true; }
                if i.key_pressed(egui::Key::Space) { self.actions.toggle_fav = true; }
                if i.key_pressed(egui::Key::F) {
                    if i.modifiers.shift { self.actions.open_filter = true; }
                    else { self.actions.cycle_filter = true; }
                }
                if i.key_pressed(egui::Key::Delete) { self.actions.delete = true; }
                if i.key_pressed(egui::Key::Escape) {
                    if self.is_maximized { self.actions.toggle_max = true; }
                    else { self.actions.quit = true; }
                }
                if i.key_pressed(egui::Key::O) && i.modifiers.ctrl { self.actions.open_folder = true; }
                if i.key_pressed(egui::Key::F11) { self.actions.toggle_max = true; }
                if i.key_pressed(egui::Key::Enter) && i.modifiers.alt { self.actions.toggle_max = true; }
            }
        });

        // Mouse wheel navigation
        let scroll = ctx.input(|i| i.raw_scroll_delta.y);
        if scroll.abs() > 1.0 && !self.show_filter {
            if scroll < 0.0 { self.actions.next = true; }
            else { self.actions.prev = true; }
        }

        let bg_alpha: u8 = if focused { 235 } else { 0 };
        let panel_frame = egui::Frame::none()
            .fill(egui::Color32::from_rgba_unmultiplied(13, 13, 13, bg_alpha));
        egui::CentralPanel::default().frame(panel_frame).show(ctx, |ui| {
            self.render_image(ui, ctx);
            self.render_overlay(ui);
            self.handle_background_interaction(ui, ctx);
            self.render_close_button(ui);
            self.render_nav_chevrons(ui);
            self.handle_touch_swipe(ui);
        });

        if self.show_filter {
            self.render_filter_window(ctx);
        }

        // Apply queued actions
        let actions = std::mem::take(&mut self.actions);
        if actions.next { self.next(); }
        if actions.prev { self.prev(); }
        if actions.rot_left { self.rotate(-1); }
        if actions.rot_right { self.rotate(1); }
        if actions.toggle_fav { self.toggle_favorite(); }
        if actions.open_filter {
            self.show_filter = true;
            self.filter_selected = self.favorites.clone();
        }
        if actions.quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if actions.open_folder {
            if let Some(d) = rfd::FileDialog::new().pick_folder() {
                self.open_folder(&d, None);
            }
        }
        if actions.toggle_max {
            self.is_maximized = !self.is_maximized;
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(self.is_maximized));
        }
        if actions.cycle_filter { self.cycle_filter(); }
        if actions.delete { self.delete_current(); }

        // Decay transient filter message
        if let Some((_, ref mut t)) = self.filter_msg {
            let dt = ctx.input(|i| i.unstable_dt);
            *t -= dt;
            if *t > 0.0 { ctx.request_repaint(); }
        }
        if matches!(&self.filter_msg, Some((_, t)) if *t <= 0.0) {
            self.filter_msg = None;
        }
    }
}

impl SnapView {
    fn render_image(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let avail = ui.available_size();
        let path = match self.current_path() {
            Some(p) => p,
            None => {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        egui::RichText::new("Drop a folder or image here\nor press Ctrl+O")
                            .color(egui::Color32::from_gray(140))
                            .size(20.0),
                    );
                });
                self.handle_drop(ctx);
                return;
            }
        };

        self.ensure_texture(ctx, &path);

        let rotation_quarter = *self.rotation.get(&path).unwrap_or(&0);

        if let Some(tex) = self.textures.get(&path) {
            let img_size = tex.size_vec2();
            let (fit_w, fit_h) = if rotation_quarter % 2 == 0 {
                (img_size.x, img_size.y)
            } else {
                (img_size.y, img_size.x)
            };
            let scale = (avail.x / fit_w).min(avail.y / fit_h).min(1.0).max(0.01);
            let draw_w = img_size.x * scale;
            let draw_h = img_size.y * scale;

            let center = ui.available_rect_before_wrap().center();
            let angle = rotation_quarter as f32 * std::f32::consts::FRAC_PI_2;

            let mut mesh = egui::Mesh::with_texture(tex.id());
            mesh.add_rect_with_uv(
                egui::Rect::from_center_size(egui::Pos2::ZERO, egui::vec2(draw_w, draw_h)),
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
            mesh.rotate(egui::emath::Rot2::from_angle(angle), egui::Pos2::ZERO);
            mesh.translate(center.to_vec2());
            ui.painter().add(egui::Shape::mesh(mesh));
        } else {
            ui.centered_and_justified(|ui| {
                ui.label(
                    egui::RichText::new("…")
                        .color(egui::Color32::from_gray(80))
                        .size(40.0),
                );
            });
        }

        self.handle_drop(ctx);
    }

    fn render_overlay(&self, ui: &mut egui::Ui) {
        if self.images.is_empty() { return; }
        let path = match self.current_path() {
            Some(p) => p,
            None => return,
        };
        let is_fav = self.favorites.contains(&path);
        let name = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let counter = if self.filter_mode == FilterMode::All {
            format!("{} / {}", self.current + 1, self.images.len())
        } else {
            let pos = self.visible_position().map(|p| p + 1).unwrap_or(0);
            format!("{} / {}  ({})", pos, self.visible_count(), self.filter_mode.label())
        };
        let fav_count = self.favorites.len();

        let painter = ui.painter();
        let rect = ui.available_rect_before_wrap();

        let text = format!("{}   ·   {}", name, counter);
        painter.text(
            egui::pos2(rect.left() + 14.0, rect.bottom() - 14.0),
            egui::Align2::LEFT_BOTTOM,
            text,
            egui::FontId::proportional(13.0),
            egui::Color32::from_rgba_premultiplied(220, 220, 220, 180),
        );

        let fav_text = if is_fav {
            format!("★  ({} favs)", fav_count)
        } else {
            format!("☆  ({} favs)", fav_count)
        };
        painter.text(
            egui::pos2(rect.right() - 14.0, rect.bottom() - 14.0),
            egui::Align2::RIGHT_BOTTOM,
            fav_text,
            egui::FontId::proportional(13.0),
            if is_fav {
                egui::Color32::from_rgb(255, 200, 60)
            } else {
                egui::Color32::from_rgba_premultiplied(220, 220, 220, 180)
            },
        );

        if let Some((msg, t)) = &self.filter_msg {
            let alpha = (t.min(1.0) * 230.0).clamp(0.0, 230.0) as u8;
            painter.text(
                egui::pos2(rect.center().x, rect.top() + 24.0),
                egui::Align2::CENTER_TOP,
                msg,
                egui::FontId::proportional(16.0),
                egui::Color32::from_rgba_premultiplied(255, 255, 255, alpha),
            );
        }
    }

    fn render_close_button(&mut self, ui: &mut egui::Ui) {
        if self.is_maximized { return; }
        let rect = ui.available_rect_before_wrap();
        let hover_zone = egui::Rect::from_min_size(
            egui::pos2(rect.right() - 90.0, rect.top()),
            egui::vec2(90.0, 90.0),
        );
        let pointer = ui.input(|i| i.pointer.hover_pos());
        let in_zone = pointer.map(|p| hover_zone.contains(p)).unwrap_or(false);

        let size = 22.0;
        let btn_rect = egui::Rect::from_min_size(
            egui::pos2(rect.right() - size - 14.0, rect.top() + 14.0),
            egui::vec2(size, size),
        );
        let resp = ui.interact(btn_rect, egui::Id::new("close_btn"), egui::Sense::click());
        if !in_zone && !resp.hovered() { return; }

        let color = if resp.hovered() {
            egui::Color32::from_rgb(255, 90, 90)
        } else {
            egui::Color32::from_rgba_premultiplied(230, 230, 230, 220)
        };
        let stroke = egui::Stroke::new(2.5, color);
        let pad = 5.0;
        let painter = ui.painter();
        painter.line_segment(
            [
                egui::pos2(btn_rect.left() + pad, btn_rect.top() + pad),
                egui::pos2(btn_rect.right() - pad, btn_rect.bottom() - pad),
            ],
            stroke,
        );
        painter.line_segment(
            [
                egui::pos2(btn_rect.right() - pad, btn_rect.top() + pad),
                egui::pos2(btn_rect.left() + pad, btn_rect.bottom() - pad),
            ],
            stroke,
        );
        if resp.clicked() {
            self.actions.quit = true;
        }
    }

    fn render_nav_chevrons(&mut self, ui: &mut egui::Ui) {
        if self.images.is_empty() { return; }
        let rect = ui.available_rect_before_wrap();
        let pointer = ui.input(|i| i.pointer.hover_pos());
        let zone_w = 110.0;
        let btn_w = 36.0;
        let btn_h = 56.0;

        let left_zone = egui::Rect::from_min_size(
            egui::pos2(rect.left(), rect.center().y - 100.0),
            egui::vec2(zone_w, 200.0),
        );
        let right_zone = egui::Rect::from_min_size(
            egui::pos2(rect.right() - zone_w, rect.center().y - 100.0),
            egui::vec2(zone_w, 200.0),
        );
        let in_left = pointer.map(|p| left_zone.contains(p)).unwrap_or(false);
        let in_right = pointer.map(|p| right_zone.contains(p)).unwrap_or(false);

        let left_rect = egui::Rect::from_min_size(
            egui::pos2(rect.left() + 14.0, rect.center().y - btn_h / 2.0),
            egui::vec2(btn_w, btn_h),
        );
        let right_rect = egui::Rect::from_min_size(
            egui::pos2(rect.right() - btn_w - 14.0, rect.center().y - btn_h / 2.0),
            egui::vec2(btn_w, btn_h),
        );

        let left_resp = ui.interact(left_rect, egui::Id::new("nav_left"), egui::Sense::click());
        let right_resp = ui.interact(right_rect, egui::Id::new("nav_right"), egui::Sense::click());

        if in_left || left_resp.hovered() {
            draw_chevron(ui.painter(), left_rect, left_resp.hovered(), false);
        }
        if in_right || right_resp.hovered() {
            draw_chevron(ui.painter(), right_rect, right_resp.hovered(), true);
        }
        if left_resp.clicked() { self.actions.prev = true; }
        if right_resp.clicked() { self.actions.next = true; }
    }

    fn handle_touch_swipe(&mut self, ui: &mut egui::Ui) {
        let events = ui.input(|i| i.events.clone());
        for ev in events {
            if let egui::Event::Touch { id, phase, pos, .. } = ev {
                match phase {
                    egui::TouchPhase::Start => {
                        self.touch_swipe = Some(TouchSwipe {
                            id,
                            start: pos,
                            last: pos,
                            triggered: false,
                        });
                    }
                    egui::TouchPhase::Move => {
                        if let Some(s) = self.touch_swipe.as_mut() {
                            if s.id == id { s.last = pos; }
                        }
                    }
                    egui::TouchPhase::End | egui::TouchPhase::Cancel => {
                        if let Some(s) = self.touch_swipe {
                            if s.id == id && !s.triggered {
                                let dx = pos.x - s.start.x;
                                let dy = pos.y - s.start.y;
                                if dx.abs() > 60.0 && dx.abs() > dy.abs() * 1.5 {
                                    if dx < 0.0 { self.actions.next = true; }
                                    else { self.actions.prev = true; }
                                }
                            }
                            if self.touch_swipe.map(|t| t.id == id).unwrap_or(false) {
                                self.touch_swipe = None;
                            }
                        }
                    }
                }
            }
        }
    }

    fn handle_background_interaction(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let resp = ui.interact(
            ui.available_rect_before_wrap(),
            egui::Id::new("bg_interact"),
            egui::Sense::click_and_drag(),
        );

        if resp.drag_started_by(egui::PointerButton::Primary) {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        if resp.double_clicked() {
            self.actions.toggle_max = true;
        }

        let is_fav_now = self.current_path()
            .map(|p| self.favorites.contains(&p))
            .unwrap_or(false);
        let fav_total = self.favorites.len();
        let filter_label = self.filter_mode.label();

        resp.context_menu(|ui| {
            if ui.button("Open folder…  (Ctrl+O)").clicked() {
                self.actions.open_folder = true;
                ui.close_menu();
            }
            ui.separator();
            if ui.button("Rotate left  (Q)").clicked() { self.actions.rot_left = true; ui.close_menu(); }
            if ui.button("Rotate right  (W)").clicked() { self.actions.rot_right = true; ui.close_menu(); }
            ui.separator();
            let label = if is_fav_now { "Unmark favorite  (Space)" } else { "Mark favorite  (Space)" };
            if ui.button(label).clicked() { self.actions.toggle_fav = true; ui.close_menu(); }
            if ui.button(format!("Cycle filter  (F)  [{}]", filter_label)).clicked() {
                self.actions.cycle_filter = true;
                ui.close_menu();
            }
            if ui.button(format!("Filter favorites window…  (Shift+F)  [{}]", fav_total)).clicked() {
                self.actions.open_filter = true;
                ui.close_menu();
            }
            ui.separator();
            if ui.button("Move to trash  (Delete)").clicked() { self.actions.delete = true; ui.close_menu(); }
            ui.separator();
            if ui.button("Toggle maximize  (F11)").clicked() { self.actions.toggle_max = true; ui.close_menu(); }
            if ui.button("Quit  (Esc)").clicked() { self.actions.quit = true; ui.close_menu(); }
        });
    }

    fn handle_drop(&mut self, ctx: &egui::Context) {
        let dropped: Vec<egui::DroppedFile> = ctx.input(|i| i.raw.dropped_files.clone());
        if dropped.is_empty() { return; }
        for f in dropped {
            if let Some(path) = f.path {
                if path.is_dir() {
                    self.open_folder(&path, None);
                    return;
                } else if path.is_file() && is_image(&path) {
                    self.open_folder_with_file(&path);
                    return;
                }
            }
        }
    }

    fn render_filter_window(&mut self, ctx: &egui::Context) {
        let mut open = true;
        let mut do_copy = false;
        let mut do_cut = false;
        let mut do_select_all = false;
        let mut do_select_none = false;

        egui::Window::new("Filter favorites & copy/cut")
            .open(&mut open)
            .resizable(true)
            .default_size([520.0, 600.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Select all").clicked() { do_select_all = true; }
                    if ui.button("Select none").clicked() { do_select_none = true; }
                    ui.label(format!(
                        "{} of {} selected",
                        self.filter_selected.len(),
                        self.favorites.len()
                    ));
                });
                ui.separator();
                egui::ScrollArea::vertical().max_height(440.0).show(ui, |ui| {
                    let mut favs: Vec<PathBuf> = self.favorites.iter().cloned().collect();
                    favs.sort();
                    for p in &favs {
                        let mut checked = self.filter_selected.contains(p);
                        let label = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                        if ui.checkbox(&mut checked, label).changed() {
                            if checked { self.filter_selected.insert(p.clone()); }
                            else { self.filter_selected.remove(p); }
                        }
                    }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.add_enabled(
                        !self.filter_selected.is_empty(),
                        egui::Button::new(format!(
                            "Copy {} selected to folder…",
                            self.filter_selected.len()
                        )),
                    ).clicked() {
                        do_copy = true;
                    }
                    if ui.add_enabled(
                        !self.filter_selected.is_empty(),
                        egui::Button::new(format!(
                            "Cut {} selected to folder…",
                            self.filter_selected.len()
                        )),
                    ).clicked() {
                        do_cut = true;
                    }
                });
            });

        if do_select_all { self.filter_selected = self.favorites.clone(); }
        if do_select_none { self.filter_selected.clear(); }
        if do_copy {
            if let Some(dest) = rfd::FileDialog::new().pick_folder() {
                match self.copy_filtered(&dest) {
                    Ok(n) => {
                        rfd::MessageDialog::new()
                            .set_title("Done")
                            .set_description(format!("Copied {} files.", n))
                            .show();
                    }
                    Err(e) => {
                        rfd::MessageDialog::new()
                            .set_title("Error")
                            .set_description(format!("Copy failed: {}", e))
                            .show();
                    }
                }
            }
        }
        if do_cut {
            if let Some(dest) = rfd::FileDialog::new().pick_folder() {
                match self.cut_filtered(&dest) {
                    Ok(n) => {
                        rfd::MessageDialog::new()
                            .set_title("Done")
                            .set_description(format!("Moved {} files.", n))
                            .show();
                    }
                    Err(e) => {
                        rfd::MessageDialog::new()
                            .set_title("Error")
                            .set_description(format!("Cut failed: {}", e))
                            .show();
                    }
                }
            }
        }

        if !open { self.show_filter = false; }
    }
}

// ---------- helpers ----------

fn draw_chevron(painter: &egui::Painter, rect: egui::Rect, hovered: bool, points_right: bool) {
    let color = if hovered {
        egui::Color32::from_rgba_premultiplied(255, 255, 255, 240)
    } else {
        egui::Color32::from_rgba_premultiplied(220, 220, 220, 180)
    };
    let stroke = egui::Stroke::new(3.0, color);
    let pad_x = 10.0;
    let pad_y = 12.0;
    let mid_y = rect.center().y;
    let (tip_x, base_x) = if points_right {
        (rect.right() - pad_x, rect.left() + pad_x)
    } else {
        (rect.left() + pad_x, rect.right() - pad_x)
    };
    let top = egui::pos2(base_x, rect.top() + pad_y);
    let bot = egui::pos2(base_x, rect.bottom() - pad_y);
    let tip = egui::pos2(tip_x, mid_y);
    painter.line_segment([top, tip], stroke);
    painter.line_segment([bot, tip], stroke);
}

fn is_image(p: &Path) -> bool {
    p.extension()
        .and_then(|s| s.to_str())
        .map(|s| SUPPORTED_EXTS.iter().any(|e| e.eq_ignore_ascii_case(s)))
        .unwrap_or(false)
}

fn decode_image(path: &Path) -> LoadedImage {
    match image::open(path) {
        Ok(mut img) => {
            let orient = read_exif_orientation(path).unwrap_or(1);
            img = apply_exif_orientation(img, orient);
            let rgba = img.to_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            let pixels = rgba.into_raw();
            let ci = egui::ColorImage::from_rgba_unmultiplied(size, &pixels);
            LoadedImage::Ready(Arc::new(ci))
        }
        Err(_) => LoadedImage::Failed,
    }
}

fn read_exif_orientation(path: &Path) -> Option<u32> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let exif = exif::Reader::new().read_from_container(&mut reader).ok()?;
    let field = exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?;
    field.value.get_uint(0)
}

fn apply_exif_orientation(img: image::DynamicImage, orientation: u32) -> image::DynamicImage {
    use image::DynamicImage;
    match orientation {
        2 => DynamicImage::ImageRgba8(image::imageops::flip_horizontal(&img)),
        3 => img.rotate180(),
        4 => DynamicImage::ImageRgba8(image::imageops::flip_vertical(&img)),
        5 => {
            let r = img.rotate90();
            DynamicImage::ImageRgba8(image::imageops::flip_horizontal(&r))
        }
        6 => img.rotate90(),
        7 => {
            let r = img.rotate270();
            DynamicImage::ImageRgba8(image::imageops::flip_horizontal(&r))
        }
        8 => img.rotate270(),
        _ => img,
    }
}

fn load_app_icon() -> Option<egui::IconData> {
    let bytes = include_bytes!("../assets/icon.png");
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = (img.width(), img.height());
    Some(egui::IconData {
        rgba: img.into_raw(),
        width: w,
        height: h,
    })
}

fn favorites_path(folder: &Path) -> PathBuf {
    folder.join(FAVORITES_FILE)
}

fn load_favorites(folder: &Path, available: &[PathBuf]) -> HashSet<PathBuf> {
    let path = favorites_path(folder);
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return HashSet::new(),
    };
    let mut set = HashSet::new();
    let avail_names: HashMap<String, PathBuf> = available
        .iter()
        .filter_map(|p| {
            p.file_name().map(|n| (n.to_string_lossy().to_string(), p.clone()))
        })
        .collect();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        if let Some(p) = avail_names.get(line) { set.insert(p.clone()); }
    }
    set
}

fn save_favorites(folder: &Path, favs: &HashSet<PathBuf>) {
    let path = favorites_path(folder);
    let mut names: Vec<String> = favs
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .collect();
    names.sort();
    let content = format!(
        "# snapview favorites — one filename per line\n{}\n",
        names.join("\n")
    );
    let _ = std::fs::write(&path, content);
}

fn num_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().min(8).max(2))
        .unwrap_or(4)
}
