#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

use eframe::egui;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

const SUPPORTED_EXTS: &[&str] = &["jpg", "jpeg", "png", "bmp", "gif", "webp", "tif", "tiff"];
const THUMB_MAX: u32 = 768;
/// Default target dimension for the full-resolution decode. 2400 covers
/// 1440p displays at 1:1 and leaves headroom for zoom; for JPEGs the
/// decoder picks the closest native DCT scale that's >= this value (so
/// 4000-8000 px camera shots decode at 2000-4000 px in ~60-120 ms).
const FULL_MAX_DIM: u32 = 2400;
/// Target for the on-demand high-quality re-decode (triggered with Z).
const HQ_MAX_DIM: u32 = 16384;
const RAW_EXTS: &[&str] = &[
    "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "raf", "orf",
    "rw2", "pef", "ptx", "srw", "dng", "raw", "rwl", "3fr", "fff", "erf",
    "iiq", "mef", "mos", "mrw", "x3f", "x3i", "kdc", "dcr", "ari", "bay",
    "cap", "dcs", "drf", "k25", "mdc", "obm", "ori", "pxn", "r3d", "rwz",
    "xmp",
];
const FAVORITES_FILE: &str = ".favorites.txt";
const PRELOAD_RADIUS: usize = 3;
const TEXTURE_CACHE_MAX: usize = 60;

fn main() -> Result<(), eframe::Error> {
    #[cfg(windows)]
    set_app_user_model_id("FilipKozina.Snapview");

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

/// Set an AppUserModelID so Windows can group our windows correctly in the
/// taskbar and so 'Pin to taskbar' / jump lists / recent-files work. Must be
/// called before any window is shown.
#[cfg(windows)]
fn set_app_user_model_id(aumid: &str) {
    extern "system" {
        fn SetCurrentProcessExplicitAppUserModelID(app_id: *const u16) -> i32;
    }
    let wide: Vec<u16> = aumid.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let _ = SetCurrentProcessExplicitAppUserModelID(wide.as_ptr());
    }
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
    show_prefs: bool,
    request_hq: bool,
    toggle_help: bool,
    toggle_metadata: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ThemeMode {
    Auto,
    Light,
    Dark,
}

impl ThemeMode {
    fn as_str(self) -> &'static str {
        match self {
            ThemeMode::Auto => "auto",
            ThemeMode::Light => "light",
            ThemeMode::Dark => "dark",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "auto" => Some(ThemeMode::Auto),
            "light" => Some(ThemeMode::Light),
            "dark" => Some(ThemeMode::Dark),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
struct Preferences {
    theme: ThemeMode,
}

impl Default for Preferences {
    fn default() -> Self {
        Self { theme: ThemeMode::Auto }
    }
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
    exif_quarter: HashMap<PathBuf, i32>,

    full_q: Arc<PrioQueue>,
    thumb_q: Arc<PrioQueue>,
    hq_mode: Arc<AtomicBool>,
    /// Bumped on every folder open. Decoder threads read it at the start of
    /// each job and tag their JobResult with the value; drain_results drops
    /// results whose generation no longer matches, so leftover work from a
    /// previously-open folder can't poison the new folder's cache.
    current_generation: Arc<AtomicU64>,
    /// Recently zoom-promoted images. Bounded FIFO (most recent at the back);
    /// workers consult this in addition to hq_mode to decide whether to decode
    /// at HQ. Small cap keeps the GPU-resident HQ texture footprint sane.
    zoom_hq_paths: Arc<Mutex<VecDeque<PathBuf>>>,
    result_rx: mpsc::Receiver<JobResult>,

    show_filter: bool,
    show_prefs: bool,
    show_help: bool,
    show_metadata: bool,
    metadata_cache: HashMap<PathBuf, Option<ImageMetadata>>,
    filter_selected: HashSet<PathBuf>,

    prefs: Preferences,
    applied_light: Option<bool>,

    is_fullscreen: bool,
    last_resized_path: Option<PathBuf>,
    last_aspect_class: Option<i8>,
    actions: PendingActions,

    filter_mode: FilterMode,
    filter_msg: Option<(String, f32)>,

    pending_delete: Option<PathBuf>,
    last_image_rect: Option<egui::Rect>,
    displayed_path: Option<PathBuf>,
    /// +1 when the user was last navigating forward, -1 backward. Used by
    /// queue_preload / prioritize_thumbs to skew the preload window in the
    /// direction the user is most likely to keep flicking.
    nav_direction: i8,

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
    Full(u64, PathBuf, LoadedImage, i32),
    Thumb(u64, PathBuf, LoadedImage, Option<[usize; 2]>, i32),
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
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for p in paths {
            if g.in_queue.insert(p.clone()) {
                g.queue.push_back(p.clone());
            }
        }
        self.cv.notify_all();
    }

    fn prioritize(&self, paths: &[PathBuf]) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
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
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.queue.clear();
        g.in_queue.clear();
    }

    /// Mark the queue as closed and wake every waiter so workers can exit
    /// their pop loop. Used from Drop to unwind decoder threads cleanly
    /// instead of leaving them parked on the Condvar at process exit.
    fn close(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.closed = true;
        self.cv.notify_all();
    }

    fn pop(&self) -> Option<PathBuf> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(p) = g.queue.pop_front() {
                g.in_queue.remove(&p);
                return Some(p);
            }
            if g.closed { return None; }
            g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }
}

impl SnapView {
    fn new(cc: &eframe::CreationContext<'_>, initial_path: Option<PathBuf>) -> Self {
        let (result_tx, result_rx) = mpsc::channel::<JobResult>();
        let full_q = PrioQueue::new();
        let thumb_q = PrioQueue::new();
        let hq_mode = Arc::new(AtomicBool::new(false));
        let zoom_hq_paths: Arc<Mutex<VecDeque<PathBuf>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let current_generation = Arc::new(AtomicU64::new(0));

        let total = num_workers();
        let n_full = (total / 2).max(2);
        let n_thumb = (total - n_full).max(1);

        for _ in 0..n_full {
            let q = Arc::clone(&full_q);
            let tx = result_tx.clone();
            let ctx = cc.egui_ctx.clone();
            let hq = Arc::clone(&hq_mode);
            let zoom_hq = Arc::clone(&zoom_hq_paths);
            let gen = Arc::clone(&current_generation);
            thread::spawn(move || loop {
                let path = match q.pop() { Some(p) => p, None => break };
                let job_gen = gen.load(Ordering::Relaxed);
                let need_hq = hq.load(Ordering::Relaxed)
                    || zoom_hq.lock().unwrap_or_else(|e| e.into_inner()).contains(&path);
                let target = if need_hq { HQ_MAX_DIM } else { FULL_MAX_DIM };
                let (r, q) = decode_image_to(&path, target);
                let _ = tx.send(JobResult::Full(job_gen, path, r, q));
                ctx.request_repaint();
            });
        }
        for _ in 0..n_thumb {
            let q = Arc::clone(&thumb_q);
            let tx = result_tx.clone();
            let ctx = cc.egui_ctx.clone();
            let gen = Arc::clone(&current_generation);
            thread::spawn(move || loop {
                let path = match q.pop() { Some(p) => p, None => break };
                let job_gen = gen.load(Ordering::Relaxed);
                let (r, dims, qrt) = decode_thumb(&path);
                let _ = tx.send(JobResult::Thumb(job_gen, path, r, dims, qrt));
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
            exif_quarter: HashMap::new(),
            full_q,
            thumb_q,
            hq_mode,
            current_generation,
            zoom_hq_paths,
            result_rx,
            show_filter: false,
            show_prefs: false,
            show_help: false,
            show_metadata: false,
            metadata_cache: HashMap::new(),
            filter_selected: HashSet::new(),

            prefs: load_preferences().unwrap_or_default(),
            applied_light: None,
            is_fullscreen: false,
            last_resized_path: None,
            last_aspect_class: None,
            actions: PendingActions::default(),
            filter_mode: FilterMode::All,
            filter_msg: None,
            pending_delete: None,
            last_image_rect: None,
            displayed_path: None,
            nav_direction: 1,
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
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.thumb_cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.textures.clear();
        self.thumb_textures.clear();
        self.full_dims.clear();
        self.exif_quarter.clear();
        self.metadata_cache.clear();
        self.rotation.clear();
        self.zoom_hq_paths.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.displayed_path = None;
        self.nav_direction = 1;

        self.thumb_q.clear();
        self.full_q.clear();
        // Bump the generation so any in-flight decodes from the previous
        // folder are dropped on arrival in drain_results.
        self.current_generation.fetch_add(1, Ordering::Relaxed);

        let n = self.images.len();
        let cur = if let Some(sel) = &select {
            self.images.iter().position(|p| p == sel).unwrap_or(0)
        } else {
            0
        };
        // Lazy thumb fill: only seed the immediate vicinity. The rest is added
        // on demand by prioritize_thumbs() as the user navigates, so we don't
        // saturate cores decoding thumbs the user may never see.
        const THUMB_INITIAL_RADIUS: usize = 30;
        let mut order: Vec<PathBuf> = Vec::with_capacity(THUMB_INITIAL_RADIUS * 2 + 1);
        if n > 0 { order.push(self.images[cur].clone()); }
        for d in 1..=THUMB_INITIAL_RADIUS.min(n.saturating_sub(1)) {
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

    /// Build a list of nearby images ordered by directional priority. The
    /// returned vec starts with the current image, then biases ahead in the
    /// user's nav_direction: it interleaves the forward half first, slips a
    /// single backward image in, finishes the rest of the forward window,
    /// then the deeper backward window. Net effect: the preload pipeline
    /// always reads ahead in the direction the user is moving, so flicking
    /// past 3-4 photos in a row never stalls on a decode, while a sudden
    /// reverse still has one image ready.
    fn directional_neighbors(&self, radius: usize) -> Vec<PathBuf> {
        let n = self.images.len();
        let cur = self.current as i64;
        let dir = self.nav_direction as i64;
        let half = (radius + 1) / 2;
        let mut offsets: Vec<i64> = Vec::with_capacity(radius * 2 + 1);
        offsets.push(0);
        for d in 1..=half {
            offsets.push(d as i64);
        }
        if radius >= 1 {
            offsets.push(-1);
        }
        for d in (half + 1)..=radius {
            offsets.push(d as i64);
        }
        for d in 2..=radius {
            offsets.push(-(d as i64));
        }
        let mut paths: Vec<PathBuf> = Vec::with_capacity(offsets.len());
        for off in offsets {
            let idx = cur + off * dir;
            if idx >= 0 && (idx as usize) < n {
                paths.push(self.images[idx as usize].clone());
            }
        }
        paths
    }

    fn prioritize_thumbs(&self) {
        if self.images.is_empty() { return; }
        const RADIUS: usize = 12;
        let paths = self.directional_neighbors(RADIUS);
        // Drop already-loaded ones from the priority list.
        let cache = self.thumb_cache.lock().unwrap_or_else(|e| e.into_inner());
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
        let paths = self.directional_neighbors(PRELOAD_RADIUS);
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
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
        let cur_gen = self.current_generation.load(Ordering::Relaxed);
        while let Ok(res) = self.result_rx.try_recv() {
            // Drop anything that was decoded for a previous folder open. The
            // PathBuf coming back may collide with a same-named file in the
            // current folder, so without this guard a stale decode would
            // silently overwrite the new folder's cache and the wrong pixels
            // would flash on screen.
            let job_gen = match &res {
                JobResult::Full(g, ..) => *g,
                JobResult::Thumb(g, ..) => *g,
            };
            if job_gen != cur_gen {
                continue;
            }
            match res {
                JobResult::Full(_, path, result, exif_q) => {
                    self.exif_quarter.insert(path.clone(), exif_q);
                    let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
                    cache.insert(path.clone(), result.clone());
                    drop(cache);
                    if let LoadedImage::Ready(ci) = result {
                        // Skip GPU upload for results that have drifted far from
                        // current — keep in cache (cheap), upload only if/when
                        // the user navigates back.
                        let upload = self
                            .images
                            .iter()
                            .position(|p| p == &path)
                            .map(|i| (i as isize - self.current as isize).unsigned_abs() <= TEXTURE_CACHE_MAX / 2)
                            .unwrap_or(true);
                        if upload {
                            let tex = ctx.load_texture(
                                path.to_string_lossy().to_string(),
                                (*ci).clone(),
                                full_texture_options(),
                            );
                            self.textures.insert(path, tex);
                        }
                    }
                }
                JobResult::Thumb(_, path, result, dims, exif_q) => {
                    self.exif_quarter.entry(path.clone()).or_insert(exif_q);
                    let mut tc = self.thumb_cache.lock().unwrap_or_else(|e| e.into_inner());
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

        if self.textures.len() > TEXTURE_CACHE_MAX && !self.images.is_empty() {
            let cur = self.current;
            let pos: HashMap<PathBuf, usize> = self
                .images
                .iter()
                .enumerate()
                .map(|(i, p)| (p.clone(), i))
                .collect();
            let mut keys: Vec<PathBuf> = self.textures.keys().cloned().collect();
            keys.sort_by_key(|p| {
                pos.get(p)
                    .map(|i| (*i as isize - cur as isize).abs())
                    .unwrap_or(isize::MAX)
            });
            for p in keys.into_iter().skip(TEXTURE_CACHE_MAX) {
                self.textures.remove(&p);
            }
        }
    }

    fn ensure_texture(&mut self, ctx: &egui::Context, path: &Path) {
        if self.textures.contains_key(path) { return; }
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(LoadedImage::Ready(ci)) = cache.get(path) {
            let ci = ci.clone();
            drop(cache);
            let tex = ctx.load_texture(
                path.to_string_lossy().to_string(),
                (*ci).clone(),
                full_texture_options(),
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
        self.nav_direction = if forward { 1 } else { -1 };
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
            self.persist_favorites();
        }
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).remove(&path);
        self.thumb_cache.lock().unwrap_or_else(|e| e.into_inner()).remove(&path);
        self.textures.remove(&path);
        self.thumb_textures.remove(&path);
        self.rotation.remove(&path);
        self.exif_quarter.remove(&path);
        self.full_dims.remove(&path);
        self.sidecars.remove(&path);
        self.images.remove(idx);
        // Index fixup after removing `idx`:
        //   idx <  current  -> shift current down by one (same image stays selected)
        //   idx == current  -> leave current alone; the slot now holds what was
        //                      next (so the viewer advances forward, matching
        //                      most viewers' "delete then go to the following
        //                      image" behavior). Clamped below.
        //   idx >  current  -> nothing to do.
        if idx < self.current {
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
        self.persist_favorites();
    }

    /// Save favorites to disk, surfacing any I/O error as a transient toast
    /// instead of silently dropping it (read-only volumes, full disk, locked
    /// file on Windows, …).
    fn persist_favorites(&mut self) {
        let Some(folder) = self.folder.clone() else { return };
        if let Err(e) = save_favorites(&folder, &self.favorites) {
            self.filter_msg = Some((format!("Couldn't save favorites: {}", e), 3.0));
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
        // First zoom past ~1.05 on the current image promotes it to a native
        // re-decode in the background. Promotions are kept as a bounded FIFO
        // (most recent at the back) so that long browsing sessions don't
        // accumulate dozens of native-resolution textures.
        if self.target_zoom > 1.05 {
            if let Some(p) = self.current_path() {
                const ZOOM_HQ_MAX: usize = 20;
                let mut set = self.zoom_hq_paths.lock().unwrap_or_else(|e| e.into_inner());
                if !set.iter().any(|x| x == &p) {
                    if set.len() >= ZOOM_HQ_MAX {
                        set.pop_front();
                    }
                    set.push_back(p.clone());
                    drop(set);
                    self.cache.lock().unwrap_or_else(|e| e.into_inner()).remove(&p);
                    self.textures.remove(&p);
                    self.full_q.prioritize(&[p]);
                }
            }
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

    /// When windowed, only on a portrait ↔ landscape transition: rescale the
    /// window's width to the new image's aspect, leave height untouched, and
    /// reposition so the window stays centered around its previous middle.
    /// Goal is the visual effect "the picture shrunk into portrait" rather
    /// than "the window jumped sideways".
    fn maybe_resize_window_to_image(&mut self, ctx: &egui::Context) {
        let path = match self.current_path() { Some(p) => p, None => return };
        if self.last_resized_path.as_ref() == Some(&path) { return; }
        // Only resize once the new image actually has something to render —
        // otherwise the OS resizes the window before the new content lands
        // and the user sees the desktop behind for a frame or two.
        let new_ready = self.textures.contains_key(&path)
            || self.thumb_textures.contains_key(&path);
        if !new_ready { return; }
        let (iw, ih) = match self.display_dims(&path) { Some(d) => d, None => return };
        if iw == 0 || ih == 0 { return; }
        let aspect = iw as f32 / ih as f32;
        let class: i8 = if aspect < 0.95 { 1 } else { 0 };

        // Skip when aspect class hasn't changed: keep current window unchanged.
        if Some(class) == self.last_aspect_class {
            self.last_resized_path = Some(path);
            return;
        }

        // Need actual window geometry on screen to recenter. egui makes this
        // available via ViewportInfo; if it's not populated yet, defer to
        // next frame.
        let outer = match ctx.input(|i| i.viewport().outer_rect) {
            Some(r) => r,
            None => return,
        };
        let new_h = outer.height();
        let new_w = (new_h * aspect).max(200.0);
        let new_x = outer.left() + (outer.width() - new_w) / 2.0;
        let new_y = outer.top();

        if (outer.width() - new_w).abs() < 2.0 {
            self.last_aspect_class = Some(class);
            self.last_resized_path = Some(path);
            return;
        }

        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(new_w, new_h)));
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(new_x, new_y)));

        self.last_aspect_class = Some(class);
        self.last_resized_path = Some(path);
    }

    fn current_is_portrait(&self) -> bool {
        let path = match self.current_path() { Some(p) => p, None => return false };
        let (w, h) = match self.display_dims(&path) {
            Some(d) => d,
            None => return false,
        };
        h > w
    }

    /// Returns the on-screen (post-rotation) dimensions of the image, using
    /// any cached info available — full cache, thumb full_dims, or the texture
    /// itself. EXIF and user rotation both factored in.
    fn display_dims(&self, path: &Path) -> Option<(usize, usize)> {
        let raw = if let Some(LoadedImage::Ready(ci)) = self.cache.lock().unwrap_or_else(|e| e.into_inner()).get(path) {
            Some((ci.size[0], ci.size[1]))
        } else if let Some(d) = self.full_dims.get(path) {
            Some((d[0], d[1]))
        } else if let Some(tex) = self.textures.get(path).or_else(|| self.thumb_textures.get(path)) {
            let s = tex.size_vec2();
            Some((s.x as usize, s.y as usize))
        } else {
            None
        }?;
        let rot = self.rotation.get(path).copied().unwrap_or(0)
            + self.exif_quarter.get(path).copied().unwrap_or(0);
        if rot.rem_euclid(2) == 0 { Some(raw) } else { Some((raw.1, raw.0)) }
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
            self.cache.lock().unwrap_or_else(|e| e.into_inner()).remove(p);
            self.textures.remove(p);
            self.thumb_textures.remove(p);
            self.rotation.remove(p);
            self.exif_quarter.remove(p);
            self.full_dims.remove(p);
        }
        self.images.retain(|p| !moved.contains(p));
        self.persist_favorites();
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

impl Drop for SnapView {
    fn drop(&mut self) {
        // Wake the decoder threads so they can exit their blocking pop()
        // loop. The threads detach naturally with the process, but closing
        // explicitly avoids the zombie-on-restart case and quiets shutdown
        // diagnostics from sanitizers.
        self.full_q.close();
        self.thumb_q.close();
    }
}

impl eframe::App for SnapView {
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.apply_theme(ctx);
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
                if i.key_pressed(egui::Key::Z) { self.actions.request_hq = true; }
                if i.key_pressed(egui::Key::F) {
                    if i.modifiers.shift { self.actions.open_filter = true; }
                    else { self.actions.cycle_filter = true; }
                }
                if i.key_pressed(egui::Key::Delete) { self.actions.delete = true; }
                if i.key_pressed(egui::Key::Escape) {
                    if self.show_help { self.show_help = false; }
                    else if self.is_fullscreen { self.actions.toggle_max = true; }
                    else { self.actions.quit = true; }
                }
                if i.key_pressed(egui::Key::O) && i.modifiers.ctrl { self.actions.open_folder = true; }
                if i.key_pressed(egui::Key::F11) { self.actions.toggle_max = true; }
                if i.key_pressed(egui::Key::Enter) && i.modifiers.alt { self.actions.toggle_max = true; }
                if i.key_pressed(egui::Key::F1) { self.actions.toggle_help = true; }
                if i.key_pressed(egui::Key::I) { self.actions.toggle_metadata = true; }
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
        if !self.is_fullscreen {
            self.maybe_resize_window_to_image(ctx);
        }
        // During an OS resize the previous frame's content gets stretched into
        let bg_alpha: u8 = if focused && !suppress_dim { 235 } else { 0 };
        let panel_frame = egui::Frame::none()
            .fill(egui::Color32::from_rgba_unmultiplied(13, 13, 13, bg_alpha));
        egui::CentralPanel::default().frame(panel_frame).show(ctx, |ui| {
            self.render_image(ui, ctx);
            self.render_overlay(ui);
            self.render_metadata_overlay(ui);
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

        if self.show_prefs {
            self.render_prefs(ctx);
        }

        if self.show_help {
            self.render_help(ctx);
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
            // Re-evaluate window sizing on next frame when leaving fullscreen.
            self.last_resized_path = None;
            self.last_aspect_class = None;
        }
        if actions.cycle_filter { self.cycle_filter(); }
        if actions.delete { self.request_delete(); }
        if actions.confirm_delete { self.confirm_delete(); }
        if actions.cancel_delete { self.pending_delete = None; }
        if actions.show_prefs { self.show_prefs = true; }
        if actions.toggle_help { self.show_help = !self.show_help; }
        if actions.toggle_metadata { self.show_metadata = !self.show_metadata; }
        if actions.request_hq {
            // Global HQ mode toggle. We keep the existing low-res textures on
            // the GPU so the screen stays full while HQ decodes land in the
            // background; the CPU-side ColorImage cache *is* cleared so that
            // ensure_texture / re-uploads can't pull a stale low-res copy.
            let new_mode = !self.hq_mode.load(Ordering::Relaxed);
            self.hq_mode.store(new_mode, Ordering::Relaxed);
            self.cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
            self.full_q.clear();
            self.queue_preload();
            self.filter_msg = Some((
                if new_mode { "HQ mode" } else { "Snappy mode" }.to_string(),
                1.2,
            ));
        }

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

        // Choose what to actually paint. Preference order:
        //   1. new path has a full texture                -> paint new (full).
        //   2. previously displayed path has a full       -> keep painting prev,
        //      so we never downgrade from a sharp image to a blurry thumb
        //      while the user waits for the next full / HQ decode.
        //   3. new path has a thumb (EXIF preview)        -> paint new (thumb).
        //   4. previously displayed path has anything     -> paint prev.
        //   5. fall through to the "..." placeholder.
        let new_has_full = self.textures.contains_key(&path);
        let new_has_thumb = self.thumb_textures.contains_key(&path);
        let prev_has_full = self
            .displayed_path
            .as_ref()
            .map(|p| self.textures.contains_key(p))
            .unwrap_or(false);
        let prev_has_anything = self
            .displayed_path
            .as_ref()
            .map(|p| self.textures.contains_key(p) || self.thumb_textures.contains_key(p))
            .unwrap_or(false);

        let draw_path: PathBuf = if new_has_full {
            self.displayed_path = Some(path.clone());
            path.clone()
        } else if prev_has_full {
            self.displayed_path.clone().unwrap()
        } else if new_has_thumb {
            self.displayed_path = Some(path.clone());
            path.clone()
        } else if prev_has_anything {
            self.displayed_path.clone().unwrap()
        } else {
            path.clone()
        };

        let user_quarter = *self.rotation.get(&draw_path).unwrap_or(&0);
        let exif_q = *self.exif_quarter.get(&draw_path).unwrap_or(&0);
        let rotation_quarter = (user_quarter + exif_q).rem_euclid(4);

        let full_tex = self.textures.get(&draw_path).cloned();
        let tex_opt = full_tex.clone().or_else(|| self.thumb_textures.get(&draw_path).cloned());

        if let Some(tex) = tex_opt {
            // Prefer full texture's own size; otherwise use stored full dims; finally fall back to texture size.
            let img_size = if let Some(t) = &full_tex {
                t.size_vec2()
            } else if let Some(d) = self.full_dims.get(&draw_path) {
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
            let local_rect =
                egui::Rect::from_center_size(egui::Pos2::ZERO, egui::vec2(draw_w, draw_h));
            let uv_rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
            // macOS-style continuous-ish corner rounding when windowed. In
            // fullscreen the image bleeds to the display edges (no panel
            // around it to round against).
            let corner_radius = if self.is_fullscreen { 0.0 } else { 12.0 };
            if corner_radius > 0.5 {
                add_rounded_rect_with_uv(
                    &mut mesh,
                    local_rect,
                    uv_rect,
                    corner_radius,
                    egui::Color32::WHITE,
                );
            } else {
                mesh.add_rect_with_uv(local_rect, uv_rect, egui::Color32::WHITE);
            }
            mesh.rotate(egui::emath::Rot2::from_angle(angle), egui::Pos2::ZERO);
            mesh.translate(center.to_vec2());
            ui.painter().add(egui::Shape::mesh(mesh));

            self.last_image_rect = Some(egui::Rect::from_center_size(
                center,
                egui::vec2(fit_w * scale, fit_h * scale),
            ));
        } else {
            // No texture yet, but keep last_image_rect in sync with the
            // image's known aspect so overlays/chevrons sit in the right
            // place during the brief decode window.
            if let Some((w, h)) = self.display_dims(&draw_path) {
                let fit_w = w as f32;
                let fit_h = h as f32;
                let base_scale = (avail.x / fit_w).min(avail.y / fit_h).min(1.0).max(0.01);
                let scale = base_scale * self.zoom;
                let center = ui.available_rect_before_wrap().center() + self.pan;
                self.last_image_rect = Some(egui::Rect::from_center_size(
                    center,
                    egui::vec2(fit_w * scale, fit_h * scale),
                ));
            }
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
        if self.hq_mode.load(Ordering::Relaxed) {
            counter.push_str("   ·   HQ");
        }
        let fav_count = self.favorites.len();

        let painter = ui.painter();
        let rect = if self.is_fullscreen {
            ui.available_rect_before_wrap()
        } else {
            self.last_image_rect.unwrap_or_else(|| ui.available_rect_before_wrap())
        };

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

    fn render_metadata_overlay(&mut self, ui: &mut egui::Ui) {
        if !self.show_metadata { return; }
        let path = match self.current_path() { Some(p) => p, None => return };
        // Decode-on-demand cache. The EXIF read is fast (kamadak parses just
        // the header) so doing it on the UI thread is fine — but we still
        // cache to avoid repeating it every frame while the HUD is up.
        if !self.metadata_cache.contains_key(&path) {
            let md = read_image_metadata(&path);
            self.metadata_cache.insert(path.clone(), if md.is_empty() { None } else { Some(md) });
        }
        let md = match self.metadata_cache.get(&path).cloned().flatten() {
            Some(m) => m,
            None => return,
        };
        let mut lines: Vec<String> = Vec::new();
        if let Some(s) = &md.date_taken { lines.push(s.clone()); }
        if let Some(s) = &md.camera { lines.push(s.clone()); }
        if let Some(s) = &md.lens { lines.push(s.clone()); }
        let exposure: Vec<&str> = [&md.shutter, &md.aperture, &md.iso, &md.focal]
            .iter()
            .filter_map(|o| o.as_deref())
            .collect();
        if !exposure.is_empty() { lines.push(exposure.join("   ·   ")); }
        let dims_size: Vec<&str> = [&md.dimensions, &md.file_size]
            .iter()
            .filter_map(|o| o.as_deref())
            .collect();
        if !dims_size.is_empty() { lines.push(dims_size.join("   ·   ")); }
        if lines.is_empty() { return; }

        let rect = if self.is_fullscreen {
            ui.available_rect_before_wrap()
        } else {
            self.last_image_rect.unwrap_or_else(|| ui.available_rect_before_wrap())
        };
        let painter = ui.painter();
        let font = egui::FontId::proportional(13.0);
        let pad = 12.0;
        let line_h = 18.0;
        let max_w = lines
            .iter()
            .map(|l| painter.layout_no_wrap(l.clone(), font.clone(), egui::Color32::WHITE).rect.width())
            .fold(0.0_f32, f32::max);
        let box_w = max_w + pad * 2.0;
        let box_h = lines.len() as f32 * line_h + pad * 2.0;
        let origin = egui::pos2(rect.left() + 14.0, rect.top() + 14.0);
        let box_rect = egui::Rect::from_min_size(origin, egui::vec2(box_w, box_h));
        painter.rect_filled(
            box_rect,
            8.0,
            egui::Color32::from_rgba_premultiplied(0, 0, 0, 170),
        );
        for (i, line) in lines.iter().enumerate() {
            painter.text(
                egui::pos2(origin.x + pad, origin.y + pad + i as f32 * line_h),
                egui::Align2::LEFT_TOP,
                line,
                font.clone(),
                egui::Color32::from_rgba_premultiplied(240, 240, 240, 230),
            );
        }
    }

    fn render_close_button(&mut self, ui: &mut egui::Ui) {
        if self.is_fullscreen { return; }
        let rect = self.last_image_rect.unwrap_or_else(|| ui.available_rect_before_wrap());
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
        let rect = if self.is_fullscreen {
            ui.available_rect_before_wrap()
        } else {
            self.last_image_rect.unwrap_or_else(|| ui.available_rect_before_wrap())
        };
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
            ui.separator();
            let hq_label = if self.hq_mode.load(Ordering::Relaxed) {
                "HQ mode  (Z)  [on]"
            } else {
                "HQ mode  (Z)"
            };
            if ui.button(hq_label).clicked() { self.actions.request_hq = true; ui.close_menu(); }
            ui.separator();
            let info_label = if self.show_metadata { "Hide image info  (I)" } else { "Show image info  (I)" };
            if ui.button(info_label).clicked() { self.actions.toggle_metadata = true; ui.close_menu(); }
            if ui.button("Keyboard shortcuts  (F1)").clicked() { self.actions.toggle_help = true; ui.close_menu(); }
            if ui.button("Preferences…").clicked() { self.actions.show_prefs = true; ui.close_menu(); }
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

    fn render_help(&mut self, ctx: &egui::Context) {
        let screen = ctx.screen_rect();
        egui::Area::new(egui::Id::new("help_dim"))
            .fixed_pos(screen.min)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.painter().rect_filled(
                    screen,
                    0.0,
                    egui::Color32::from_rgba_premultiplied(0, 0, 0, 180),
                );
                if ui.allocate_rect(screen, egui::Sense::click()).clicked() {
                    self.show_help = false;
                }
            });

        let pairs_left: &[(&str, &str)] = &[
            ("Next image", "→  /  scroll down"),
            ("Previous image", "←  /  scroll up"),
            ("Zoom in / out", "Ctrl + scroll"),
            ("Reset zoom", "Num 0"),
            ("HQ mode (native)", "Z"),
            ("Show / hide image info", "I"),
            ("Open folder…", "Ctrl + O"),
        ];
        let pairs_right: &[(&str, &str)] = &[
            ("Mark / unmark favorite", "Space"),
            ("Cycle filter (all / favs / non-favs)", "F"),
            ("Filter favorites window…", "Shift + F"),
            ("Move to trash", "Delete"),
            ("Rotate left / right", "Q  /  W"),
            ("Toggle fullscreen", "F11  /  Alt + Enter"),
            ("Show / hide this help", "F1"),
            ("Quit (or exit fullscreen first)", "Esc"),
        ];

        egui::Window::new("Keyboard shortcuts")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .order(egui::Order::Tooltip)
            .show(ctx, |ui| {
                ui.set_min_width(560.0);
                ui.add_space(4.0);
                egui::Grid::new("help_grid")
                    .num_columns(4)
                    .spacing([24.0, 6.0])
                    .show(ui, |ui| {
                        let rows = pairs_left.len().max(pairs_right.len());
                        for i in 0..rows {
                            if let Some((desc, key)) = pairs_left.get(i) {
                                ui.label(egui::RichText::new(*desc).color(egui::Color32::from_gray(220)));
                                ui.label(egui::RichText::new(*key).strong().color(egui::Color32::from_rgb(180, 200, 255)));
                            } else {
                                ui.label("");
                                ui.label("");
                            }
                            if let Some((desc, key)) = pairs_right.get(i) {
                                ui.label(egui::RichText::new(*desc).color(egui::Color32::from_gray(220)));
                                ui.label(egui::RichText::new(*key).strong().color(egui::Color32::from_rgb(180, 200, 255)));
                            } else {
                                ui.label("");
                                ui.label("");
                            }
                            ui.end_row();
                        }
                    });
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Mouse:").strong());
                    ui.label("drag = move window (or pan when zoomed) · double-click = fullscreen · right-click = menu");
                });
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Touch:").strong());
                    ui.label("swipe horizontally to flick between images");
                });
                ui.add_space(6.0);
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new("Press F1 or Esc to dismiss")
                        .color(egui::Color32::from_gray(150)).italics());
                });
                ui.add_space(2.0);
            });
    }

    fn apply_theme(&mut self, ctx: &egui::Context) {
        let light = self.effective_light(ctx);
        if self.applied_light != Some(light) {
            ctx.set_visuals(if light {
                egui::Visuals::light()
            } else {
                egui::Visuals::dark()
            });
            self.applied_light = Some(light);
        }
    }

    fn effective_light(&self, ctx: &egui::Context) -> bool {
        match self.prefs.theme {
            ThemeMode::Light => true,
            ThemeMode::Dark => false,
            ThemeMode::Auto => {
                // egui populates viewport.theme from the OS (winit reads the
                // Windows AppsUseLightTheme registry / NSAppearance / etc.).
                // Default to dark when the system doesn't expose a preference.
                match ctx.system_theme() {
                    Some(egui::Theme::Light) => true,
                    Some(egui::Theme::Dark) => false,
                    None => false,
                }
            }
        }
    }

    fn render_prefs(&mut self, ctx: &egui::Context) {
        let screen = ctx.screen_rect();
        egui::Area::new(egui::Id::new("prefs_dim"))
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

        let mut close = false;
        let mut theme_changed: Option<ThemeMode> = None;
        egui::Window::new("Preferences")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .order(egui::Order::Tooltip)
            .show(ctx, |ui| {
                ui.set_min_width(360.0);
                ui.add_space(6.0);
                ui.label(egui::RichText::new("Appearance").strong());
                ui.add_space(4.0);
                let mut t = self.prefs.theme;
                ui.horizontal(|ui| {
                    if ui.radio_value(&mut t, ThemeMode::Auto, "Follow system").clicked() {}
                    if ui.radio_value(&mut t, ThemeMode::Light, "Light").clicked() {}
                    if ui.radio_value(&mut t, ThemeMode::Dark, "Dark").clicked() {}
                });
                if t != self.prefs.theme {
                    theme_changed = Some(t);
                }

                ui.add_space(14.0);
                ui.separator();
                ui.add_space(10.0);

                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new("snapview").size(20.0).strong());
                    ui.label(
                        egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                            .color(egui::Color32::from_gray(160)),
                    );
                    ui.add_space(6.0);
                    ui.label("Fast, minimal image viewer");
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new("by Filip Kozina")
                            .color(egui::Color32::from_gray(180)),
                    );
                    ui.add_space(14.0);
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
                ui.add_space(2.0);
            });

        if let Some(t) = theme_changed {
            self.prefs.theme = t;
            let _ = save_preferences(&self.prefs);
        }
        if close || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.show_prefs = false;
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
    if let Some(ext) = p.extension().and_then(|s| s.to_str()) {
        return SUPPORTED_EXTS.iter().any(|e| e.eq_ignore_ascii_case(ext));
    }
    // Some cameras (and renames) drop the extension entirely. Peek a few
    // bytes and accept files whose header matches one of our supported
    // formats. Files with any extension (even an unknown one) are NOT
    // sniffed — that keeps folder-scan I/O bounded and avoids false
    // positives on .txt/.json that happen to start with binary noise.
    has_supported_magic(p)
}

fn has_supported_magic(p: &Path) -> bool {
    use std::io::Read;
    let f = match std::fs::File::open(p) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut head = [0u8; 12];
    let n = std::io::BufReader::new(f).read(&mut head).unwrap_or(0);
    if n < 4 {
        return false;
    }
    // JPEG: FF D8 FF
    if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return true;
    }
    // PNG: 89 50 4E 47 0D 0A 1A 0A
    if head.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        return true;
    }
    // GIF: GIF87a / GIF89a
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return true;
    }
    // BMP: BM
    if head.starts_with(b"BM") {
        return true;
    }
    // WebP: RIFF....WEBP
    if n >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        return true;
    }
    // TIFF: II*\0 (LE) or MM\0* (BE)
    if head.starts_with(&[0x49, 0x49, 0x2A, 0x00]) || head.starts_with(&[0x4D, 0x4D, 0x00, 0x2A]) {
        return true;
    }
    false
}

fn is_raw_sidecar(p: &Path) -> bool {
    p.extension()
        .and_then(|s| s.to_str())
        .map(|s| RAW_EXTS.iter().any(|e| e.eq_ignore_ascii_case(s)))
        .unwrap_or(false)
}

fn is_jpeg(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case("jpg") || s.eq_ignore_ascii_case("jpeg"))
        .unwrap_or(false)
}

fn decode_jpeg_scaled(path: &Path, target_max_dim: u32) -> Option<(image::DynamicImage, Option<Vec<u8>>)> {
    let f = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(f);
    let mut decoder = jpeg_decoder::Decoder::new(reader);
    // Must be called before read_info / decode so the JPEG IDCT runs at the
    // smaller scale natively (1/2, 1/4 or 1/8 of the original size).
    let target = target_max_dim.max(1).min(u16::MAX as u32) as u16;
    let _ = decoder.scale(target, target);
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let icc = decoder.icc_profile();
    let w = info.width as u32;
    let h = info.height as u32;
    // Guard against gigapixel JPEGs that would overflow u32 byte counts
    // (anything past ~32_767 px on either axis). Done with usize arithmetic
    // and checked_mul so the multiplication can't silently wrap.
    let byte_count = (w as usize)
        .checked_mul(h as usize)
        .and_then(|wh| wh.checked_mul(4))?;
    let rgba: Vec<u8> = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => {
            let mut out = vec![0u8; byte_count];
            for (i, c) in pixels.chunks_exact(3).enumerate() {
                let o = i * 4;
                out[o] = c[0];
                out[o + 1] = c[1];
                out[o + 2] = c[2];
                out[o + 3] = 255;
            }
            out
        }
        jpeg_decoder::PixelFormat::L8 => {
            let mut out = vec![0u8; byte_count];
            for (i, &v) in pixels.iter().enumerate() {
                let o = i * 4;
                out[o] = v;
                out[o + 1] = v;
                out[o + 2] = v;
                out[o + 3] = 255;
            }
            out
        }
        _ => return None,
    };
    let rgba_img = image::RgbaImage::from_raw(w, h, rgba)?;
    Some((image::DynamicImage::ImageRgba8(rgba_img), icc))
}

/// Open a non-JPEG via the image crate, returning the decoded DynamicImage
/// alongside its embedded ICC profile (PNG iCCP, TIFF ICCProfile, etc).
fn load_image_with_icc(path: &Path) -> Option<(image::DynamicImage, Option<Vec<u8>>)> {
    use image::ImageDecoder;
    let f = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(f);
    let reader = image::ImageReader::new(reader).with_guessed_format().ok()?;
    let mut decoder = reader.into_decoder().ok()?;
    let icc = decoder.icc_profile().ok().flatten();
    let img = image::DynamicImage::from_decoder(decoder).ok()?;
    Some((img, icc))
}

/// Convert an RGBA buffer from its embedded ICC profile to sRGB in place.
/// Returns true if a conversion was actually performed. The common case
/// (sRGB or untagged) short-circuits to no-op.
fn apply_icc_to_srgb(rgba: &mut [u8], icc_bytes: &[u8]) -> bool {
    use lcms2::*;
    let src = match Profile::new_icc(icc_bytes) {
        Ok(p) => p,
        Err(_) => return false,
    };
    if let Some(desc) = src.info(InfoType::Description, Locale::none()) {
        let d = desc.to_lowercase();
        // sRGB and the various "untagged"/"display" aliases all map to our
        // assumed output space — skip the per-pixel transform.
        if d.contains("srgb") || d.contains("iec61966") {
            return false;
        }
    }
    let dst = Profile::new_srgb();
    let transform = match Transform::new(
        &src,
        PixelFormat::RGBA_8,
        &dst,
        PixelFormat::RGBA_8,
        Intent::Perceptual,
    ) {
        Ok(t) => t,
        Err(_) => return false,
    };
    transform.transform_in_place(rgba);
    true
}

fn decode_image_to(path: &Path, target_dim: u32) -> (LoadedImage, i32) {
    let (img, did_jpeg_scale, icc) = if is_jpeg(path) {
        match decode_jpeg_scaled(path, target_dim) {
            Some((i, icc)) => (Some(i), true, icc),
            None => {
                let pair = load_image_with_icc(path);
                let icc = pair.as_ref().and_then(|p| p.1.clone());
                (pair.map(|p| p.0), false, icc)
            }
        }
    } else {
        let pair = load_image_with_icc(path);
        let icc = pair.as_ref().and_then(|p| p.1.clone());
        (pair.map(|p| p.0), false, icc)
    };
    let Some(mut img) = img else { return (LoadedImage::Failed, 0) };
    let orient = read_exif_orientation(path).unwrap_or(1);
    let display_quarter;
    (img, display_quarter) = apply_exif_orientation_lazy(img, orient);
    if !did_jpeg_scale {
        let max_dim = img.width().max(img.height());
        if max_dim > target_dim {
            img = img.resize(target_dim, target_dim, image::imageops::FilterType::Triangle);
        }
    }
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let mut pixels = rgba.into_raw();
    if let Some(icc_bytes) = &icc {
        apply_icc_to_srgb(&mut pixels, icc_bytes);
    }
    let ci = color_image_from_rgba(size, pixels);
    (LoadedImage::Ready(Arc::new(ci)), display_quarter)
}

fn decode_thumb(path: &Path) -> (LoadedImage, Option<[usize; 2]>, i32) {
    // Fast path for JPEGs: lift the camera-embedded EXIF thumbnail (typically
    // 160x120, 5-15 KB). Parsing + decoding it is ~5 ms; the GPU will upscale
    // it to whatever the viewport wants. Yes, the result is blurry, but the
    // user explicitly wants this "low-res but full size" preview tier.
    if is_jpeg(path) {
        if let Some(tup) = decode_jpeg_exif_thumb(path) {
            return tup;
        }
    }
    let (img, icc) = if is_jpeg(path) {
        match decode_jpeg_scaled(path, THUMB_MAX) {
            Some((i, icc)) => (Some(i), icc),
            None => {
                let pair = load_image_with_icc(path);
                let icc = pair.as_ref().and_then(|p| p.1.clone());
                (pair.map(|p| p.0), icc)
            }
        }
    } else {
        let pair = load_image_with_icc(path);
        let icc = pair.as_ref().and_then(|p| p.1.clone());
        (pair.map(|p| p.0), icc)
    };
    let Some(mut img) = img else { return (LoadedImage::Failed, None, 0) };
    let orient = read_exif_orientation(path).unwrap_or(1);
    let display_quarter;
    (img, display_quarter) = apply_exif_orientation_lazy(img, orient);
    let full_dims = [img.width() as usize, img.height() as usize];
    let small = img.thumbnail(THUMB_MAX, THUMB_MAX);
    let rgba = small.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let mut pixels = rgba.into_raw();
    if let Some(icc_bytes) = &icc {
        apply_icc_to_srgb(&mut pixels, icc_bytes);
    }
    let ci = color_image_from_rgba(size, pixels);
    (LoadedImage::Ready(Arc::new(ci)), Some(full_dims), display_quarter)
}

/// Read the embedded thumbnail (typically 160x120, 5-15 KB) from a JPEG's
/// EXIF APP1 segment. Cameras and phones embed this for instant preview;
/// decoding it is ~5 ms vs ~100 ms for a scaled JPEG decode of a 20 MB file.
fn decode_jpeg_exif_thumb(path: &Path) -> Option<(LoadedImage, Option<[usize; 2]>, i32)> {
    let (thumb_bytes, full_w, full_h, orient) = read_jpeg_exif_metadata(path)?;
    let thumb_bytes = thumb_bytes?;
    let dyn_img = image::load_from_memory_with_format(&thumb_bytes, image::ImageFormat::Jpeg).ok()?;
    let (img, display_quarter) = apply_exif_orientation_lazy(dyn_img, orient);
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let pixels = rgba.into_raw();
    let ci = color_image_from_rgba(size, pixels);
    let full_dims = if full_w > 0 && full_h > 0 {
        if (5..=8).contains(&orient) {
            Some([full_h as usize, full_w as usize])
        } else {
            Some([full_w as usize, full_h as usize])
        }
    } else {
        None
    };
    Some((LoadedImage::Ready(Arc::new(ci)), full_dims, display_quarter))
}

/// Minimal EXIF scanner: extracts (thumb_bytes?, full_width, full_height,
/// orientation). Reads at most a few KB of header before seeking directly
/// to the thumbnail bytes; doesn't depend on a full EXIF parser.
/// Minimal EXIF + JPEG SOF scanner: extracts (thumb_bytes?, image_width,
/// image_height, orientation). The width/height come from the JPEG's SOFn
/// frame header (always present) rather than EXIF IFD0's ImageWidth/Length
/// tags, which many encoders simply omit.
fn read_jpeg_exif_metadata(path: &Path) -> Option<(Option<Vec<u8>>, u32, u32, u32)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let mut soi = [0u8; 2];
    f.read_exact(&mut soi).ok()?;
    if soi != [0xFF, 0xD8] { return None; }

    let mut exif: Option<(Option<Vec<u8>>, u32, u32, u32)> = None;
    let mut sof: Option<(u32, u32)> = None;

    for _ in 0..64 {
        // A marker is 0xFF followed by a non-zero / non-0xFF byte; encoders
        // may emit any number of 0xFF fill bytes between segments. Walk past
        // them and any stray non-marker bytes (corruption resilience).
        let mut b = [0u8; 1];
        if f.read_exact(&mut b).is_err() { break; }
        if b[0] != 0xFF { continue; }
        let m = loop {
            let mut x = [0u8; 1];
            if f.read_exact(&mut x).is_err() { return finalize(exif, sof); }
            if x[0] != 0xFF { break x[0]; }
        };
        // SOI/EOI/RSTn/TEM are standalone (no length, no payload).
        if m == 0x00 || m == 0xD8 || m == 0xD9 || m == 0x01 || (0xD0..=0xD7).contains(&m) {
            if m == 0xD9 { break; } // EOI
            continue;
        }
        // SOS (Start of Scan): compressed entropy data follows; we'd have to
        // scan for the next non-stuffed 0xFFxx marker. Anything we care about
        // appears before SOS, so just stop.
        if m == 0xDA { break; }
        let mut len_bytes = [0u8; 2];
        if f.read_exact(&mut len_bytes).is_err() { break; }
        let seg_len = u16::from_be_bytes(len_bytes) as u64;
        // A valid length is >= 2 (it includes the two length bytes themselves).
        if seg_len < 2 { break; }
        let payload_len = seg_len - 2;
        let seg_data_start = f.stream_position().ok()?;
        let seg_end = seg_data_start + payload_len;

        // SOFn family (frame header). Excludes DHT (C4), JPG (C8), DAC (CC).
        if (0xC0..=0xCF).contains(&m) && m != 0xC4 && m != 0xC8 && m != 0xCC {
            let mut frame = [0u8; 5];
            if payload_len >= 5 && f.read_exact(&mut frame).is_ok() {
                let h = u16::from_be_bytes([frame[1], frame[2]]) as u32;
                let w = u16::from_be_bytes([frame[3], frame[4]]) as u32;
                sof = Some((w, h));
            }
            f.seek(SeekFrom::Start(seg_end)).ok()?;
        } else if m == 0xE1 && exif.is_none() {
            let mut id = [0u8; 6];
            if payload_len < 6 || f.read_exact(&mut id).is_err() {
                f.seek(SeekFrom::Start(seg_end)).ok()?;
                continue;
            }
            if &id == b"Exif\0\0" {
                exif = parse_tiff_for_metadata(&mut f);
                let _ = f.seek(SeekFrom::Start(seg_end));
            } else {
                f.seek(SeekFrom::Start(seg_end)).ok()?;
            }
        } else {
            f.seek(SeekFrom::Start(seg_end)).ok()?;
        }
        if exif.is_some() && sof.is_some() { break; }
    }

    finalize(exif, sof)
}

fn finalize(
    exif: Option<(Option<Vec<u8>>, u32, u32, u32)>,
    sof: Option<(u32, u32)>,
) -> Option<(Option<Vec<u8>>, u32, u32, u32)> {
    let (thumb, ew, eh, orient) = exif.unwrap_or((None, 0, 0, 1));
    let (w, h) = match (sof, (ew != 0 && eh != 0).then_some((ew, eh))) {
        (Some(s), _) => s,
        (None, Some(e)) => e,
        _ => (0, 0),
    };
    if w == 0 && h == 0 && thumb.is_none() { return None; }
    Some((thumb, w, h, orient))
}

fn parse_tiff_for_metadata(f: &mut std::fs::File) -> Option<(Option<Vec<u8>>, u32, u32, u32)> {
    use std::io::{Read, Seek, SeekFrom};
    let tiff_start = f.stream_position().ok()?;
    let mut tiff_header = [0u8; 8];
    f.read_exact(&mut tiff_header).ok()?;
    let le = &tiff_header[0..2] == b"II";
    let r16 = |b: &[u8]| if le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) };
    let r32 = |b: &[u8]| if le { u32::from_le_bytes([b[0], b[1], b[2], b[3]]) } else { u32::from_be_bytes([b[0], b[1], b[2], b[3]]) };
    let ifd0_offset = r32(&tiff_header[4..8]) as u64;
    f.seek(SeekFrom::Start(tiff_start + ifd0_offset)).ok()?;
    let mut ne_buf = [0u8; 2];
    f.read_exact(&mut ne_buf).ok()?;
    let n_entries = r16(&ne_buf);
    let mut orient: u32 = 1;
    for _ in 0..n_entries {
        let mut e = [0u8; 12];
        f.read_exact(&mut e).ok()?;
        let tag = r16(&e[0..2]);
        if tag == 0x0112 {
            orient = r16(&e[8..10]) as u32;
        }
    }
    let mut next_off = [0u8; 4];
    f.read_exact(&mut next_off).ok()?;
    let ifd1_offset = r32(&next_off) as u64;
    if ifd1_offset == 0 {
        return Some((None, 0, 0, orient));
    }
    f.seek(SeekFrom::Start(tiff_start + ifd1_offset)).ok()?;
    let mut ne_buf = [0u8; 2];
    f.read_exact(&mut ne_buf).ok()?;
    let n_entries = r16(&ne_buf);
    let mut thumb_offset: Option<u32> = None;
    let mut thumb_length: Option<u32> = None;
    let mut img_w: u32 = 0;
    let mut img_h: u32 = 0;
    for _ in 0..n_entries {
        let mut e = [0u8; 12];
        f.read_exact(&mut e).ok()?;
        let tag = r16(&e[0..2]);
        let value = r32(&e[8..12]);
        match tag {
            0x0201 => thumb_offset = Some(value),
            0x0202 => thumb_length = Some(value),
            0x0100 => img_w = value,
            0x0101 => img_h = value,
            _ => {}
        }
    }
    let bytes = match (thumb_offset, thumb_length) {
        (Some(off), Some(len)) if len > 0 && len < 1_000_000 => {
            f.seek(SeekFrom::Start(tiff_start + off as u64)).ok()?;
            let mut data = vec![0u8; len as usize];
            f.read_exact(&mut data).ok()?;
            Some(data)
        }
        _ => None,
    };
    Some((bytes, img_w, img_h, orient))
}

#[derive(Clone, Default)]
struct ImageMetadata {
    date_taken: Option<String>,
    camera: Option<String>,
    lens: Option<String>,
    shutter: Option<String>,
    aperture: Option<String>,
    iso: Option<String>,
    focal: Option<String>,
    dimensions: Option<String>,
    file_size: Option<String>,
}

impl ImageMetadata {
    fn is_empty(&self) -> bool {
        self.date_taken.is_none()
            && self.camera.is_none()
            && self.lens.is_none()
            && self.shutter.is_none()
            && self.aperture.is_none()
            && self.iso.is_none()
            && self.focal.is_none()
            && self.dimensions.is_none()
            && self.file_size.is_none()
    }
}

fn read_image_metadata(path: &Path) -> ImageMetadata {
    let mut m = ImageMetadata::default();
    if let Ok(meta) = std::fs::metadata(path) {
        m.file_size = Some(format_file_size(meta.len()));
    }
    if let Ok(f) = std::fs::File::open(path) {
        let mut reader = std::io::BufReader::new(f);
        if let Ok(exif) = exif::Reader::new().read_from_container(&mut reader) {
            use exif::{In, Tag};
            let get_str = |tag: Tag| -> Option<String> {
                exif.get_field(tag, In::PRIMARY).and_then(|f| {
                    let s = f.display_value().to_string();
                    let s = s.trim().trim_matches('"').to_string();
                    if s.is_empty() { None } else { Some(s) }
                })
            };
            let get_rational = |tag: Tag| -> Option<(u32, u32)> {
                let f = exif.get_field(tag, In::PRIMARY)?;
                if let exif::Value::Rational(ref v) = f.value {
                    v.first().map(|r| (r.num as u32, r.denom as u32))
                } else {
                    None
                }
            };
            // Date taken — prefer DateTimeOriginal, fall back to DateTime.
            m.date_taken = get_str(Tag::DateTimeOriginal)
                .or_else(|| get_str(Tag::DateTime))
                .map(prettify_exif_datetime);
            // Camera body — "Make Model" if both, else either.
            let make = get_str(Tag::Make);
            let model = get_str(Tag::Model);
            m.camera = match (make, model) {
                (Some(mk), Some(md)) if md.starts_with(&mk) => Some(md),
                (Some(mk), Some(md)) => Some(format!("{} {}", mk, md)),
                (Some(mk), None) => Some(mk),
                (None, Some(md)) => Some(md),
                _ => None,
            };
            m.lens = get_str(Tag::LensModel);
            // Shutter speed (ExposureTime) as a fraction "1/x" when num=1.
            if let Some((n, d)) = get_rational(Tag::ExposureTime) {
                if d != 0 {
                    m.shutter = Some(if n == 1 {
                        format!("1/{} s", d)
                    } else if n > 0 && (n as f64) / (d as f64) < 1.0 {
                        format!("1/{} s", (d as f64 / n as f64).round() as u64)
                    } else {
                        format!("{:.1} s", n as f64 / d as f64)
                    });
                }
            }
            if let Some((n, d)) = get_rational(Tag::FNumber) {
                if d != 0 {
                    m.aperture = Some(format!("f/{:.1}", n as f64 / d as f64));
                }
            }
            if let Some(iso) = get_str(Tag::PhotographicSensitivity).or_else(|| get_str(Tag::ISOSpeed)) {
                m.iso = Some(format!("ISO {}", iso));
            }
            if let Some((n, d)) = get_rational(Tag::FocalLength) {
                if d != 0 {
                    m.focal = Some(format!("{} mm", (n as f64 / d as f64).round() as u64));
                }
            }
            // Pixel dimensions from EXIF if present.
            let pw = exif
                .get_field(Tag::PixelXDimension, In::PRIMARY)
                .and_then(|f| f.value.get_uint(0));
            let ph = exif
                .get_field(Tag::PixelYDimension, In::PRIMARY)
                .and_then(|f| f.value.get_uint(0));
            if let (Some(w), Some(h)) = (pw, ph) {
                m.dimensions = Some(format!("{} × {}", w, h));
            }
        }
    }
    // Fallback dimensions from JPEG SOF when EXIF didn't carry them.
    if m.dimensions.is_none() {
        if let Some((_, w, h, _)) = read_jpeg_exif_metadata(path) {
            if w > 0 && h > 0 {
                m.dimensions = Some(format!("{} × {}", w, h));
            }
        }
    }
    m
}

fn prettify_exif_datetime(s: String) -> String {
    // EXIF uses "YYYY:MM:DD HH:MM:SS". Replace the date colons with dashes
    // so it reads more naturally; leave the time portion alone.
    let bytes = s.as_bytes();
    if bytes.len() >= 19 && bytes[4] == b':' && bytes[7] == b':' && bytes[10] == b' ' {
        format!(
            "{}-{}-{} {}",
            &s[0..4],
            &s[5..7],
            &s[8..10],
            &s[11..]
        )
    } else {
        s
    }
}

fn format_file_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB { format!("{:.2} GB", b / GB) }
    else if b >= MB { format!("{:.1} MB", b / MB) }
    else if b >= KB { format!("{:.0} KB", b / KB) }
    else { format!("{} B", bytes) }
}

fn read_exif_orientation(path: &Path) -> Option<u32> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let exif = exif::Reader::new().read_from_container(&mut reader).ok()?;
    let field = exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?;
    field.value.get_uint(0)
}

/// EXIF orientation handling: rotation-only orientations (1/3/6/8) are
/// deferred to display-time mesh rotation (free). Mirror orientations
/// (2/4/5/7) are rare and still applied in pixels here.
/// Returns (transformed_image, display_quarter_to_add).
fn apply_exif_orientation_lazy(img: image::DynamicImage, orientation: u32) -> (image::DynamicImage, i32) {
    use image::DynamicImage;
    match orientation {
        1 => (img, 0),
        3 => (img, 2),
        6 => (img, 1),
        8 => (img, 3),
        2 => (DynamicImage::ImageRgba8(image::imageops::flip_horizontal(&img)), 0),
        4 => (DynamicImage::ImageRgba8(image::imageops::flip_vertical(&img)), 0),
        5 => {
            let r = img.rotate90();
            (DynamicImage::ImageRgba8(image::imageops::flip_horizontal(&r)), 0)
        }
        7 => {
            let r = img.rotate270();
            (DynamicImage::ImageRgba8(image::imageops::flip_horizontal(&r)), 0)
        }
        _ => (img, 0),
    }
}

/// Texture options used for full-resolution image textures: bilinear
/// minification + magnification, plus mipmaps so fit-to-window downscaling
/// stays crisp without moire. Mipmaps cost ~33% extra texture memory and
/// a one-shot generation on upload.
fn full_texture_options() -> egui::TextureOptions {
    egui::TextureOptions {
        magnification: egui::TextureFilter::Linear,
        minification: egui::TextureFilter::Linear,
        wrap_mode: egui::TextureWrapMode::ClampToEdge,
        mipmap_mode: Some(egui::TextureFilter::Linear),
    }
}

/// Builds a textured rounded rect into an existing Mesh. Per-vertex UV is
/// interpolated linearly from the position inside `rect`, so the caller can
/// freely rotate/translate the resulting Mesh and the texture sticks with
/// the geometry (rounded corners rotate together with the image content).
fn add_rounded_rect_with_uv(
    mesh: &mut egui::Mesh,
    rect: egui::Rect,
    uv: egui::Rect,
    radius: f32,
    color: egui::Color32,
) {
    let r = radius.clamp(0.0, rect.width().min(rect.height()) * 0.5);
    if r < 0.5 {
        mesh.add_rect_with_uv(rect, uv, color);
        return;
    }
    use std::f32::consts::{FRAC_PI_2, PI};
    let segments: usize = 10;
    // Corner arcs walked clockwise: top-left, top-right, bottom-right, bottom-left.
    // Angles use egui's coord system (y grows downward).
    let corners = [
        (egui::pos2(rect.min.x + r, rect.min.y + r), PI, 1.5 * PI),
        (egui::pos2(rect.max.x - r, rect.min.y + r), 1.5 * PI, 2.0 * PI),
        (egui::pos2(rect.max.x - r, rect.max.y - r), 0.0, FRAC_PI_2),
        (egui::pos2(rect.min.x + r, rect.max.y - r), FRAC_PI_2, PI),
    ];
    let center = rect.center();
    let center_uv = egui::pos2(uv.min.x + uv.width() * 0.5, uv.min.y + uv.height() * 0.5);
    let base = mesh.vertices.len() as u32;
    mesh.vertices.push(egui::epaint::Vertex {
        pos: center,
        uv: center_uv,
        color,
    });
    let mut perim_count: u32 = 0;
    for &(c, a0, a1) in &corners {
        for i in 0..=segments {
            let t = i as f32 / segments as f32;
            let a = a0 + (a1 - a0) * t;
            let p = egui::pos2(c.x + r * a.cos(), c.y + r * a.sin());
            let u = uv.min.x + (p.x - rect.min.x) / rect.width() * uv.width();
            let v = uv.min.y + (p.y - rect.min.y) / rect.height() * uv.height();
            mesh.vertices.push(egui::epaint::Vertex {
                pos: p,
                uv: egui::pos2(u, v),
                color,
            });
            perim_count += 1;
        }
    }
    for i in 0..perim_count {
        let next = (i + 1) % perim_count;
        mesh.indices.push(base);
        mesh.indices.push(base + 1 + i);
        mesh.indices.push(base + 1 + next);
    }
}

fn color_image_from_rgba(size: [usize; 2], rgba: Vec<u8>) -> egui::ColorImage {
    debug_assert_eq!(size[0] * size[1] * 4, rgba.len());
    let pixel_count = rgba.len() / 4;
    // Pre-fill with zeroed Color32 so capacity == len exactly (avoids the
    // Vec::with_capacity-then-set_len trap where capacity may exceed the
    // requested count and a future Vec::shrink_to_fit / drop relies on the
    // allocator-reported capacity matching the typed length).
    let mut pixels: Vec<egui::Color32> = vec![egui::Color32::TRANSPARENT; pixel_count];
    // Safety: Color32 is `#[repr(C)] struct(u8, u8, u8, u8)` with alignment 1
    // (alignment of u8) and size 4. The byte-level layout is therefore
    // identical to a packed RGBA tuple, and rgba.len() == pixel_count * 4.
    // We copy through *mut u8 so the source pointer's 1-byte alignment is
    // sufficient on every platform regardless of any future Color32 layout
    // tightening.
    const _: () = {
        assert!(std::mem::size_of::<egui::Color32>() == 4);
        assert!(std::mem::align_of::<egui::Color32>() == 1);
    };
    unsafe {
        std::ptr::copy_nonoverlapping(
            rgba.as_ptr(),
            pixels.as_mut_ptr() as *mut u8,
            rgba.len(),
        );
    }
    egui::ColorImage { size, pixels }
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

/// Returns the directory for our per-user persistent state (preferences,
/// future caches). Honors Windows %LOCALAPPDATA%, macOS Application Support,
/// and the XDG base-dir spec on Linux. Returns None when none of those env
/// vars are populated (extremely rare).
fn config_dir() -> Option<PathBuf> {
    let mut p = if cfg!(target_os = "windows") {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
    } else if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME")?;
        let mut p = PathBuf::from(home);
        p.push("Library");
        p.push("Application Support");
        p
    } else {
        match std::env::var_os("XDG_CONFIG_HOME") {
            Some(d) => PathBuf::from(d),
            None => {
                let home = std::env::var_os("HOME")?;
                let mut p = PathBuf::from(home);
                p.push(".config");
                p
            }
        }
    };
    p.push("snapview");
    Some(p)
}

fn preferences_path() -> Option<PathBuf> {
    let mut p = config_dir()?;
    p.push("preferences.txt");
    Some(p)
}

fn load_preferences() -> Option<Preferences> {
    let path = preferences_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let mut prefs = Preferences::default();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "theme" => {
                    if let Some(t) = ThemeMode::parse(v) {
                        prefs.theme = t;
                    }
                }
                _ => {}
            }
        }
    }
    Some(prefs)
}

fn save_preferences(prefs: &Preferences) -> std::io::Result<()> {
    let path = preferences_path().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no config dir")
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = format!(
        "# snapview preferences\ntheme={}\n",
        prefs.theme.as_str()
    );
    let mut tmp = path.clone();
    let name = match path.file_name().and_then(|s| s.to_str()) {
        Some(n) => format!("{}.tmp", n),
        None => return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name")),
    };
    tmp.set_file_name(name);
    if let Err(e) = std::fs::write(&tmp, content.as_bytes()) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
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

fn save_favorites(folder: &Path, favs: &HashSet<PathBuf>) -> std::io::Result<()> {
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
    // Atomic write: stage to a sibling .tmp first, then rename onto the
    // real path. fs::rename is atomic on the same volume on every OS we
    // target, so a crash mid-write can never leave .favorites.txt as a
    // truncated empty file (which would silently wipe the user's
    // favorites the next time we read it).
    let mut tmp = path.clone();
    let new_name = match path.file_name().and_then(|s| s.to_str()) {
        Some(n) => format!("{}.tmp", n),
        None => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "favorites path has no file name",
            ));
        }
    };
    tmp.set_file_name(new_name);
    if let Err(e) = std::fs::write(&tmp, content.as_bytes()) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

fn num_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().min(8).max(2))
        .unwrap_or(4)
}
