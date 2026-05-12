#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

use eframe::egui;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

const SUPPORTED_EXTS: &[&str] = &["jpg", "jpeg", "png", "bmp", "gif", "webp", "tif", "tiff"];
const THUMB_MAX: u32 = 256;
const RAW_EXTS: &[&str] = &[
    "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "raf", "orf",
    "rw2", "pef", "ptx", "srw", "dng", "raw", "rwl", "3fr", "fff", "erf",
    "iiq", "mef", "mos", "mrw", "x3f", "x3i", "kdc", "dcr", "ari", "bay",
    "cap", "dcs", "drf", "k25", "mdc", "obm", "ori", "pxn", "r3d", "rwz",
    "xmp",
];
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
    confirm_delete: bool,
    cancel_delete: bool,
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
    sidecars: HashMap<PathBuf, Vec<PathBuf>>,

    thumb_cache: Arc<Mutex<HashMap<PathBuf, LoadedImage>>>,
    thumb_textures: HashMap<PathBuf, egui::TextureHandle>,
    full_dims: HashMap<PathBuf, [usize; 2]>,

    full_q: Arc<PrioQueue>,
    thumb_q: Arc<PrioQueue>,
    result_rx: mpsc::Receiver<JobResult>,

    show_filter: bool,
    filter_selected: HashSet<PathBuf>,

    is_fullscreen: bool,
    actions: PendingActions,

    filter_mode: FilterMode,
    filter_msg: Option<(String, f32)>,

    pending_delete: Option<PathBuf>,
    last_image_rect: Option<egui::Rect>,

    zoom: f32,
    target_zoom: f32,
    pan: egui::Vec2,
    target_pan: egui::Vec2,
    last_view_path: Option<PathBuf>,

    touch_swipe: Option<TouchSwipe>,
}

#[derive(Clone, Copy)]
struct TouchSwipe {
    id: egui::TouchId,
    start: egui::Pos2,
    last: egui::Pos2,
    triggered: bool,
}

enum JobResult {
    Full(PathBuf, LoadedImage),
    Thumb(PathBuf, LoadedImage, Option<[usize; 2]>),
}

struct PrioQueueInner {
    queue: VecDeque<PathBuf>,
    in_queue: HashSet<PathBuf>,
    closed: bool,
}

struct PrioQueue {
    inner: Mutex<PrioQueueInner>,
    cv: Condvar,
}

impl PrioQueue {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(PrioQueueInner {
                queue: VecDeque::new(),
                in_queue: HashSet::new(),
                closed: false,
            }),
            cv: Condvar::new(),
        })
    }

    fn enqueue_back(&self, paths: &[PathBuf]) {
        let mut g = self.inner.lock().unwrap();
        for p in paths {
            if g.in_queue.insert(p.clone()) {
                g.queue.push_back(p.clone());
            }
        }
        self.cv.notify_all();
    }

    fn prioritize(&self, paths: &[PathBuf]) {
        let mut g = self.inner.lock().unwrap();
        for p in paths.iter().rev() {
            if let Some(pos) = g.queue.iter().position(|x| x == p) {
                g.queue.remove(pos);
                g.queue.push_front(p.clone());
            } else if g.in_queue.insert(p.clone()) {
                g.queue.push_front(p.clone());
            }
        }
        self.cv.notify_all();
    }

    fn clear(&self) {
        let mut g = self.inner.lock().unwrap();
        g.queue.clear();
        g.in_queue.clear();
    }

    fn pop(&self) -> Option<PathBuf> {
        let mut g = self.inner.lock().unwrap();
        loop {
            if let Some(p) = g.queue.pop_front() {
                g.in_queue.remove(&p);
                return Some(p);
            }
            if g.closed { return None; }
            g = self.cv.wait(g).unwrap();
        }
    }
}

impl SnapView {
    fn new(cc: &eframe::CreationContext<'_>, initial_path: Option<PathBuf>) -> Self {
        let (result_tx, result_rx) = mpsc::channel::<JobResult>();
        let full_q = PrioQueue::new();
        let thumb_q = PrioQueue::new();

        let total = num_workers();
        let n_full = (total / 2).max(2);
        let n_thumb = (total - n_full).max(1);

        for _ in 0..n_full {
            let q = Arc::clone(&full_q);
            let tx = result_tx.clone();
            let ctx = cc.egui_ctx.clone();
            thread::spawn(move || loop {
                let path = match q.pop() { Some(p) => p, None => break };
                let r = decode_image(&path);
                let _ = tx.send(JobResult::Full(path, r));
                ctx.request_repaint();
            });
        }
        for _ in 0..n_thumb {
            let q = Arc::clone(&thumb_q);
            let tx = result_tx.clone();
            let ctx = cc.egui_ctx.clone();
            thread::spawn(move || loop {
                let path = match q.pop() { Some(p) => p, None => break };
                let (r, dims) = decode_thumb(&path);
                let _ = tx.send(JobResult::Thumb(path, r, dims));
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
            sidecars: HashMap::new(),
            thumb_cache: Arc::new(Mutex::new(HashMap::new())),
            thumb_textures: HashMap::new(),
            full_dims: HashMap::new(),
            full_q,
            thumb_q,
            result_rx,
            show_filter: false,
            filter_selected: HashSet::new(),
            is_fullscreen: false,
            actions: PendingActions::default(),
            filter_mode: FilterMode::All,
            filter_msg: None,
            pending_delete: None,
            last_image_rect: None,
            zoom: 1.0,
            target_zoom: 1.0,
            pan: egui::Vec2::ZERO,
            target_pan: egui::Vec2::ZERO,
            last_view_path: None,
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
        let all: Vec<PathBuf> = std::fs::read_dir(folder)
            .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
            .unwrap_or_default();

        let mut images: Vec<PathBuf> = all.iter().filter(|p| is_image(p)).cloned().collect();
        images.sort();

        // Group raw sidecars by stem
        let mut by_stem: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for p in &all {
            if !is_raw_sidecar(p) { continue; }
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                by_stem.entry(stem.to_lowercase()).or_default().push(p.clone());
            }
        }
        let mut sidecars: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        for img in &images {
            if let Some(stem) = img.file_stem().and_then(|s| s.to_str()) {
                if let Some(list) = by_stem.get(&stem.to_lowercase()) {
                    sidecars.insert(img.clone(), list.clone());
                }
            }
        }

        self.folder = Some(folder.to_path_buf());
        self.favorites = load_favorites(folder, &images);
        self.images = images;
        self.sidecars = sidecars;
        self.cache.lock().unwrap().clear();
        self.thumb_cache.lock().unwrap().clear();
        self.textures.clear();
        self.thumb_textures.clear();
        self.full_dims.clear();
        self.rotation.clear();

        self.thumb_q.clear();
        self.full_q.clear();

        let n = self.images.len();
        let cur = if let Some(sel) = &select {
            self.images.iter().position(|p| p == sel).unwrap_or(0)
        } else {
            0
        };
        // Bulk fill: every image, in order radiating outward from `cur`.
        let mut order: Vec<PathBuf> = Vec::with_capacity(n);
        if n > 0 { order.push(self.images[cur].clone()); }
        for d in 1..n {
            if cur + d < n { order.push(self.images[cur + d].clone()); }
            if cur >= d { order.push(self.images[cur - d].clone()); }
        }
        self.thumb_q.enqueue_back(&order);

        self.current = if let Some(sel) = select {
            self.images.iter().position(|p| p == &sel).unwrap_or(0)
        } else {
            0
        };

        self.queue_preload();
        self.prioritize_thumbs();
    }

    fn prioritize_thumbs(&self) {
        if self.images.is_empty() { return; }
        const RADIUS: usize = 12;
        let n = self.images.len();
        let cur = self.current;
        let mut paths: Vec<PathBuf> = Vec::with_capacity(RADIUS * 2 + 1);
        paths.push(self.images[cur].clone());
        for d in 1..=RADIUS {
            if cur + d < n { paths.push(self.images[cur + d].clone()); }
            if cur >= d { paths.push(self.images[cur - d].clone()); }
        }
        // Drop already-loaded ones from the priority list.
        let cache = self.thumb_cache.lock().unwrap();
        let pending: Vec<PathBuf> = paths
            .into_iter()
            .filter(|p| !matches!(cache.get(p), Some(LoadedImage::Ready(_)) | Some(LoadedImage::Failed)))
            .collect();
        drop(cache);
        if !pending.is_empty() {
            self.thumb_q.prioritize(&pending);
        }
    }

    fn queue_preload(&self) {
        if self.images.is_empty() { return; }
        let n = self.images.len();
        let cur = self.current;

        let mut paths: Vec<PathBuf> = Vec::new();
        paths.push(self.images[cur].clone());
        for d in 1..=PRELOAD_RADIUS {
            if cur + d < n { paths.push(self.images[cur + d].clone()); }
            if cur >= d { paths.push(self.images[cur - d].clone()); }
        }
        let cache = self.cache.lock().unwrap();
        let pending: Vec<PathBuf> = paths
            .into_iter()
            .filter(|p| !matches!(cache.get(p), Some(LoadedImage::Ready(_)) | Some(LoadedImage::Failed)))
            .collect();
        drop(cache);
        if !pending.is_empty() {
            self.full_q.prioritize(&pending);
        }
    }

    fn drain_results(&mut self, ctx: &egui::Context) {
        while let Ok(res) = self.result_rx.try_recv() {
            match res {
                JobResult::Full(path, result) => {
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
                JobResult::Thumb(path, result, dims) => {
                    let mut tc = self.thumb_cache.lock().unwrap();
                    tc.insert(path.clone(), result.clone());
                    drop(tc);
                    if let Some(d) = dims { self.full_dims.insert(path.clone(), d); }
                    if let LoadedImage::Ready(ci) = result {
                        let tex = ctx.load_texture(
                            format!("thumb:{}", path.to_string_lossy()),
                            (*ci).clone(),
                            egui::TextureOptions::LINEAR,
                        );
                        self.thumb_textures.insert(path, tex);
                    }
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
                self.prioritize_thumbs();
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
        self.prioritize_thumbs();
    }

    fn request_delete(&mut self) {
        if let Some(p) = self.current_path() {
            self.pending_delete = Some(p);
        }
    }

    fn confirm_delete(&mut self) {
        let path = match self.pending_delete.take() {
            Some(p) => p,
            None => return,
        };
        let idx = match self.images.iter().position(|p| p == &path) {
            Some(i) => i,
            None => return,
        };
        let mut to_trash: Vec<PathBuf> = vec![path.clone()];
        if let Some(side) = self.sidecars.get(&path) {
            to_trash.extend(side.iter().cloned());
        }
        if trash::delete_all(&to_trash).is_err() {
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
        self.thumb_cache.lock().unwrap().remove(&path);
        self.textures.remove(&path);
        self.thumb_textures.remove(&path);
        self.rotation.remove(&path);
        self.sidecars.remove(&path);
        self.images.remove(idx);
        if idx <= self.current && self.current > 0 {
            self.current -= 1;
        }

        if self.images.is_empty() {
            self.current = 0;
        } else {
            if self.current >= self.images.len() {
                self.current = self.images.len() - 1;
            }
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
        let extra = to_trash.len() - 1;
        let msg = if extra > 0 {
            format!("Moved to trash (+ {} sidecar{})", extra, if extra == 1 { "" } else { "s" })
        } else {
            "Moved to trash".to_string()
        };
        self.filter_msg = Some((msg, 1.2));
        self.queue_preload();
        self.prioritize_thumbs();
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

    fn reset_view(&mut self) {
        self.zoom = 1.0;
        self.target_zoom = 1.0;
        self.pan = egui::Vec2::ZERO;
        self.target_pan = egui::Vec2::ZERO;
    }

    fn apply_zoom(&mut self, factor: f32, hover: Option<egui::Pos2>, ctx: &egui::Context) {
        let new_target = (self.target_zoom * factor).clamp(1.0, 32.0);
        if (new_target - self.target_zoom).abs() < 0.0001 { return; }
        let k = new_target / self.target_zoom;
        let center = ctx.screen_rect().center();
        let s = hover.unwrap_or(center);
        self.target_pan = (s - center) * (1.0 - k) + self.target_pan * k;
        self.target_zoom = new_target;
        if self.target_zoom <= 1.0001 {
            self.target_pan = egui::Vec2::ZERO;
        }
        ctx.request_repaint();
    }

    fn animate_view(&mut self, dt: f32) -> bool {
        let zoom_diff = self.target_zoom - self.zoom;
        let pan_diff = self.target_pan - self.pan;
        let any = zoom_diff.abs() > 0.0005 || pan_diff.length() > 0.05;
        if !any {
            self.zoom = self.target_zoom;
            self.pan = self.target_pan;
            return false;
        }
        let t = (dt * 22.0).clamp(0.0, 1.0);
        self.zoom += zoom_diff * t;
        self.pan += pan_diff * t;
        true
    }

    fn current_is_portrait(&self) -> bool {
        let path = match self.current_path() { Some(p) => p, None => return false };
        let cache = self.cache.lock().unwrap();
        if let Some(LoadedImage::Ready(ci)) = cache.get(&path) {
            let rot = *self.rotation.get(&path).unwrap_or(&0);
            let (w, h) = (ci.size[0], ci.size[1]);
            let (w, h) = if rot.rem_euclid(2) == 0 { (w, h) } else { (h, w) };
            return h > w;
        }
        false
    }

    fn copy_filtered(&self, dest: &Path) -> std::io::Result<(usize, usize)> {
        std::fs::create_dir_all(dest)?;
        let mut count = 0;
        let mut side_count = 0;
        for src in &self.filter_selected {
            if let Some(name) = src.file_name() {
                std::fs::copy(src, dest.join(name))?;
                count += 1;
            }
            if let Some(sides) = self.sidecars.get(src) {
                for s in sides {
                    if let Some(name) = s.file_name() {
                        std::fs::copy(s, dest.join(name))?;
                        side_count += 1;
                    }
                }
            }
        }
        Ok((count, side_count))
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

        // Reset view when current image changes
        let cur_path = self.current_path();
        if cur_path != self.last_view_path {
            self.reset_view();
            self.last_view_path = cur_path;
        }

        let dt = ctx.input(|i| i.unstable_dt);
        if self.animate_view(dt) { ctx.request_repaint(); }

        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));

        let modal_open = self.pending_delete.is_some();

        // Capture keyboard input
        ctx.input(|i| {
            if modal_open {
                if i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Y) {
                    self.actions.confirm_delete = true;
                }
                if i.key_pressed(egui::Key::Escape) || i.key_pressed(egui::Key::N) {
                    self.actions.cancel_delete = true;
                }
                return;
            }
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
                    if self.is_fullscreen { self.actions.toggle_max = true; }
                    else { self.actions.quit = true; }
                }
                if i.key_pressed(egui::Key::O) && i.modifiers.ctrl { self.actions.open_folder = true; }
                if i.key_pressed(egui::Key::F11) { self.actions.toggle_max = true; }
                if i.key_pressed(egui::Key::Enter) && i.modifiers.alt { self.actions.toggle_max = true; }
            }
        });

        // Scroll / pinch -> zoom (cursor-anchored)
        if !self.show_filter && !modal_open {
            let (scroll_y, zoom_pinch, hover) = ctx.input(|i| {
                (i.raw_scroll_delta.y, i.zoom_delta(), i.pointer.hover_pos())
            });
            let mut factor = 1.0_f32;
            if scroll_y.abs() > 0.5 { factor *= (scroll_y * 0.0018).exp(); }
            if (zoom_pinch - 1.0).abs() > 0.0005 { factor *= zoom_pinch; }
            if (factor - 1.0).abs() > 0.0001 {
                self.apply_zoom(factor, hover, ctx);
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Num0)) {
                self.target_zoom = 1.0;
                self.target_pan = egui::Vec2::ZERO;
            }
        }

        let portrait = self.current_is_portrait();
        let suppress_dim = portrait && !self.is_fullscreen && self.target_zoom <= 1.001;
        let bg_alpha: u8 = if focused && !suppress_dim { 235 } else { 0 };
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

        if self.pending_delete.is_some() {
            self.render_delete_confirm(ctx);
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
            self.is_fullscreen = !self.is_fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.is_fullscreen));
        }
        if actions.cycle_filter { self.cycle_filter(); }
        if actions.delete { self.request_delete(); }
        if actions.confirm_delete { self.confirm_delete(); }
        if actions.cancel_delete { self.pending_delete = None; }

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

        let full_tex = self.textures.get(&path).cloned();
        let tex_opt = full_tex.clone().or_else(|| self.thumb_textures.get(&path).cloned());

        if let Some(tex) = tex_opt {
            // Prefer full texture's own size; otherwise use stored full dims; finally fall back to texture size.
            let img_size = if let Some(t) = &full_tex {
                t.size_vec2()
            } else if let Some(d) = self.full_dims.get(&path) {
                egui::vec2(d[0] as f32, d[1] as f32)
            } else {
                tex.size_vec2()
            };
            let (fit_w, fit_h) = if rotation_quarter % 2 == 0 {
                (img_size.x, img_size.y)
            } else {
                (img_size.y, img_size.x)
            };
            let base_scale = (avail.x / fit_w).min(avail.y / fit_h).min(1.0).max(0.01);
            let scale = base_scale * self.zoom;
            let draw_w = img_size.x * scale;
            let draw_h = img_size.y * scale;

            let center = ui.available_rect_before_wrap().center() + self.pan;
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

            self.last_image_rect = Some(egui::Rect::from_center_size(
                center,
                egui::vec2(fit_w * scale, fit_h * scale),
            ));
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
        let mut counter = if self.filter_mode == FilterMode::All {
            format!("{} / {}", self.current + 1, self.images.len())
        } else {
            let pos = self.visible_position().map(|p| p + 1).unwrap_or(0);
            format!("{} / {}  ({})", pos, self.visible_count(), self.filter_mode.label())
        };
        if let Some(sides) = self.sidecars.get(&path) {
            if !sides.is_empty() {
                counter.push_str(&format!("   ·   +{} RAW", sides.len()));
            }
        }
        let fav_count = self.favorites.len();

        let painter = ui.painter();
        let rect = self.last_image_rect.unwrap_or_else(|| ui.available_rect_before_wrap());

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
        if self.is_fullscreen { return; }
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

        let zoomed = self.target_zoom > 1.001;
        if zoomed {
            if resp.dragged_by(egui::PointerButton::Primary) {
                let d = resp.drag_delta();
                self.pan += d;
                self.target_pan += d;
            }
        } else if resp.drag_started_by(egui::PointerButton::Primary) {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        if resp.double_clicked() {
            if zoomed {
                self.target_zoom = 1.0;
                self.target_pan = egui::Vec2::ZERO;
            } else {
                self.actions.toggle_max = true;
            }
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
            if ui.button("Toggle fullscreen  (F11)").clicked() { self.actions.toggle_max = true; ui.close_menu(); }
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

    fn render_delete_confirm(&mut self, ctx: &egui::Context) {
        let path = match &self.pending_delete { Some(p) => p.clone(), None => return };
        let name = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let side_count = self.sidecars.get(&path).map(|v| v.len()).unwrap_or(0);
        let side_names: Vec<String> = self.sidecars.get(&path)
            .map(|v| v.iter().filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).collect())
            .unwrap_or_default();

        let screen = ctx.screen_rect();
        egui::Area::new(egui::Id::new("delete_dim"))
            .fixed_pos(screen.min)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.painter().rect_filled(
                    screen,
                    0.0,
                    egui::Color32::from_rgba_premultiplied(0, 0, 0, 140),
                );
                ui.allocate_rect(screen, egui::Sense::click());
            });

        let mut do_confirm = false;
        let mut do_cancel = false;
        egui::Window::new("Delete?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .order(egui::Order::Tooltip)
            .show(ctx, |ui| {
                ui.set_min_width(360.0);
                ui.add_space(4.0);
                let header = if side_count > 0 {
                    format!("Move this image and {} RAW/sidecar file{} to trash?", side_count, if side_count == 1 { "" } else { "s" })
                } else {
                    "Move this image to trash?".to_string()
                };
                ui.label(egui::RichText::new(header).size(15.0));
                ui.add_space(6.0);
                ui.label(egui::RichText::new(&name).color(egui::Color32::from_gray(200)).italics());
                for sn in &side_names {
                    ui.label(egui::RichText::new(format!("+ {}", sn)).color(egui::Color32::from_gray(160)).italics().size(12.0));
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new("Delete  (Enter)")
                        .fill(egui::Color32::from_rgb(160, 50, 50)))
                        .clicked()
                    {
                        do_confirm = true;
                    }
                    if ui.button("Cancel  (Esc)").clicked() {
                        do_cancel = true;
                    }
                });
                ui.add_space(2.0);
            });

        if do_confirm { self.actions.confirm_delete = true; }
        if do_cancel { self.actions.cancel_delete = true; }
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
                    Ok((n, s)) => {
                        let desc = if s > 0 {
                            format!("Copied {} images + {} RAW/sidecar files.", n, s)
                        } else {
                            format!("Copied {} files.", n)
                        };
                        rfd::MessageDialog::new()
                            .set_title("Done")
                            .set_description(desc)
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

fn is_raw_sidecar(p: &Path) -> bool {
    p.extension()
        .and_then(|s| s.to_str())
        .map(|s| RAW_EXTS.iter().any(|e| e.eq_ignore_ascii_case(s)))
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

fn decode_thumb(path: &Path) -> (LoadedImage, Option<[usize; 2]>) {
    match image::open(path) {
        Ok(mut img) => {
            let orient = read_exif_orientation(path).unwrap_or(1);
            img = apply_exif_orientation(img, orient);
            let full_dims = [img.width() as usize, img.height() as usize];
            let small = img.thumbnail(THUMB_MAX, THUMB_MAX);
            let rgba = small.to_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            let pixels = rgba.into_raw();
            let ci = egui::ColorImage::from_rgba_unmultiplied(size, &pixels);
            (LoadedImage::Ready(Arc::new(ci)), Some(full_dims))
        }
        Err(_) => (LoadedImage::Failed, None),
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
