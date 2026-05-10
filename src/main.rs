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

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_decorations(false)
            .with_transparent(true)
            .with_title("snapview"),
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

    fn next(&mut self) {
        if self.images.is_empty() { return; }
        self.current = (self.current + 1) % self.images.len();
        self.queue_preload();
    }

    fn prev(&mut self) {
        if self.images.is_empty() { return; }
        if self.current == 0 { self.current = self.images.len() - 1; }
        else { self.current -= 1; }
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
}

impl eframe::App for SnapView {
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        [0.05, 0.05, 0.05, 1.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_results(ctx);

        // Capture keyboard input
        ctx.input(|i| {
            if !self.show_filter {
                if i.key_pressed(egui::Key::ArrowRight) { self.actions.next = true; }
                if i.key_pressed(egui::Key::ArrowLeft) { self.actions.prev = true; }
                if i.key_pressed(egui::Key::Q) { self.actions.rot_left = true; }
                if i.key_pressed(egui::Key::W) { self.actions.rot_right = true; }
                if i.key_pressed(egui::Key::Space) { self.actions.toggle_fav = true; }
                if i.key_pressed(egui::Key::F) { self.actions.open_filter = true; }
                if i.key_pressed(egui::Key::Escape) { self.actions.quit = true; }
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

        let panel_frame = egui::Frame::none().fill(egui::Color32::from_rgb(13, 13, 13));
        egui::CentralPanel::default().frame(panel_frame).show(ctx, |ui| {
            self.render_image(ui, ctx);
            self.render_overlay(ui);
            self.handle_context_menu(ui);
            self.handle_window_drag(ui, ctx);
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
        let counter = format!("{} / {}", self.current + 1, self.images.len());
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
    }

    fn handle_context_menu(&mut self, ui: &mut egui::Ui) {
        let resp = ui.interact(
            ui.available_rect_before_wrap(),
            egui::Id::new("bg_interact"),
            egui::Sense::click_and_drag(),
        );
        let is_fav_now = self.current_path()
            .map(|p| self.favorites.contains(&p))
            .unwrap_or(false);
        let fav_total = self.favorites.len();

        resp.context_menu(|ui| {
            if ui.button("Open folder…  (Ctrl+O)").clicked() {
                self.actions.open_folder = true;
                ui.close_menu();
            }
            ui.separator();
            if ui.button("Next  (→)").clicked() { self.actions.next = true; ui.close_menu(); }
            if ui.button("Previous  (←)").clicked() { self.actions.prev = true; ui.close_menu(); }
            ui.separator();
            if ui.button("Rotate left  (Q)").clicked() { self.actions.rot_left = true; ui.close_menu(); }
            if ui.button("Rotate right  (W)").clicked() { self.actions.rot_right = true; ui.close_menu(); }
            ui.separator();
            let label = if is_fav_now { "Unmark favorite  (Space)" } else { "Mark favorite  (Space)" };
            if ui.button(label).clicked() { self.actions.toggle_fav = true; ui.close_menu(); }
            if ui.button(format!("Filter favorites…  (F)  [{}]", fav_total)).clicked() {
                self.actions.open_filter = true;
                ui.close_menu();
            }
            ui.separator();
            if ui.button("Toggle maximize  (F11)").clicked() { self.actions.toggle_max = true; ui.close_menu(); }
            if ui.button("Quit  (Esc)").clicked() { self.actions.quit = true; ui.close_menu(); }
        });
    }

    fn handle_window_drag(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let resp = ui.interact(
            ui.available_rect_before_wrap(),
            egui::Id::new("drag_zone"),
            egui::Sense::click_and_drag(),
        );
        if resp.drag_started_by(egui::PointerButton::Primary) {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        if resp.double_clicked() {
            self.actions.toggle_max = true;
        }
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
        let mut do_select_all = false;
        let mut do_select_none = false;

        egui::Window::new("Filter favorites & copy")
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

        if !open { self.show_filter = false; }
    }
}

// ---------- helpers ----------

fn is_image(p: &Path) -> bool {
    p.extension()
        .and_then(|s| s.to_str())
        .map(|s| SUPPORTED_EXTS.iter().any(|e| e.eq_ignore_ascii_case(s)))
        .unwrap_or(false)
}

fn decode_image(path: &Path) -> LoadedImage {
    match image::open(path) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            let pixels = rgba.into_raw();
            let ci = egui::ColorImage::from_rgba_unmultiplied(size, &pixels);
            LoadedImage::Ready(Arc::new(ci))
        }
        Err(_) => LoadedImage::Failed,
    }
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
