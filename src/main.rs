#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

use eframe::egui;
use rayon::prelude::*;
use std::cmp::Ordering as CmpOrdering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const SUPPORTED_EXTS: &[&str] = &["jpg", "jpeg", "png", "bmp", "gif", "webp", "tif", "tiff"];
/// Upper bound for the regular (non-HQ) decode. The actual target is the
/// long side of the monitor in physical pixels, clamped to
/// [FULL_MIN_DIM, FULL_MAX_DIM]: enough to fill the screen when fit to it,
/// and zooming in past fit promotes the image to an HQ decode anyway. For
/// JPEGs the decoder picks the closest native DCT scale that's >= the
/// target; other formats are box-halved to the same size class.
const FULL_MAX_DIM: u32 = 2400;
const FULL_MIN_DIM: u32 = 1280;
/// Target for the on-demand high-quality re-decode (triggered with Z).
const HQ_MAX_DIM: u32 = 16384;
/// Assumed GPU texture limit until the renderer reports the real one.
/// Anything a D3D10-class GPU can do; egui panics on bigger textures.
const DEFAULT_MAX_TEXTURE_SIDE: u32 = 8192;
const RAW_EXTS: &[&str] = &[
    "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "raf", "orf",
    "rw2", "pef", "ptx", "srw", "dng", "raw", "rwl", "3fr", "fff", "erf",
    "iiq", "mef", "mos", "mrw", "x3f", "x3i", "kdc", "dcr", "ari", "bay",
    "cap", "dcs", "drf", "k25", "mdc", "obm", "ori", "pxn", "r3d", "rwz",
    "xmp",
];
const FAVORITES_FILE: &str = ".favorites.txt";
const PRELOAD_RADIUS: usize = 3;
/// Full-resolution textures kept beyond the preload window, nearest first,
/// as long as they also fit in TEXTURE_BUDGET_BYTES. Going back a few
/// images stays instant without letting GPU memory grow with the folder.
const TEXTURE_KEEP_MAX: usize = 16;
const TEXTURE_BUDGET_BYTES: usize = 512 * 1024 * 1024;
/// EXIF thumbnails further than this from the current image are dropped
/// (and re-probed if the user comes back).
const THUMB_KEEP_RADIUS: usize = 60;
/// How long a file must be quiet after a write before we re-decode it, so
/// a copy in progress isn't decoded (and cached as failed) half-written.
const FS_SETTLE: Duration = Duration::from_millis(350);
const DEFAULT_WINDOW_HEIGHT: f32 = 800.0;
/// Toasts fade out over their last second.
const TOAST_FADE: Duration = Duration::from_secs(1);

fn main() -> Result<(), eframe::Error> {
    #[cfg(windows)]
    set_app_user_model_id("FilipKozina.Snapview");

    // args_os: a non-UTF-8 path must not panic. Made absolute so a bare
    // `snapview photo.jpg` from a terminal still finds its folder.
    let initial_path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .map(|p| std::path::absolute(&p).unwrap_or(p));

    // Start decoding before the window exists: eframe spends a good part of
    // startup creating the window and GL context, and the first image can
    // decode in parallel with that instead of after it.
    let loader = Loader::spawn();
    let mut initial_aspect = None;
    if let Some(p) = initial_path.as_ref().filter(|p| p.is_file()) {
        loader.full_q.replace(std::slice::from_ref(p));
        loader.probe_q.enqueue_back(std::slice::from_ref(p));
        // Header-only (~1 ms), so the window can open at the image's aspect
        // instead of resizing right after the first frame.
        initial_aspect = probe_image(p).display_aspect();
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(initial_window_size(initial_aspect))
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
        Box::new(move |cc| Ok(Box::new(SnapView::new(cc, loader, initial_path)))),
    )
}

/// Window size for the first image: DEFAULT_WINDOW_HEIGHT tall at the
/// image's aspect, with very wide or very tall images clamped so the window
/// stays a sane size before we know the monitor.
fn initial_window_size(aspect: Option<f32>) -> [f32; 2] {
    let Some(aspect) = aspect.filter(|a| a.is_finite() && *a > 0.0) else {
        return [1200.0, DEFAULT_WINDOW_HEIGHT];
    };
    let mut h = DEFAULT_WINDOW_HEIGHT;
    let mut w = h * aspect;
    if w > 1600.0 {
        w = 1600.0;
        h = (w / aspect).max(200.0);
    }
    [w.max(320.0), h]
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

/// Notify the Windows shell that the user just opened a folder via our app.
/// Populates the system "Recent items" list and, because we previously set
/// an AppUserModelID, the per-app Jump List on the taskbar icon. SHARD_PATHW
/// = 3, the wide-string-path variant of SHAddToRecentDocs.
///
/// The call writes a .lnk and updates the jump list synchronously, which
/// can take a noticeable while (longer on network paths), so it runs on its
/// own thread with COM initialized instead of delaying the first frame.
#[cfg(windows)]
fn sh_add_to_recent_docs(path: PathBuf) {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "shell32")]
    extern "system" {
        fn SHAddToRecentDocs(uFlags: u32, pv: *const std::ffi::c_void);
    }
    #[link(name = "ole32")]
    extern "system" {
        fn CoInitializeEx(reserved: *const std::ffi::c_void, coinit: u32) -> i32;
        fn CoUninitialize();
    }
    const SHARD_PATHW: u32 = 3;
    const COINIT_APARTMENTTHREADED: u32 = 0x2;
    let _ = thread::Builder::new().name("recent-docs".into()).spawn(move || {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let hr = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED);
            SHAddToRecentDocs(SHARD_PATHW, wide.as_ptr() as *const std::ffi::c_void);
            if hr >= 0 {
                CoUninitialize();
            }
        }
    });
}

#[derive(Default)]
struct PendingActions {
    next: bool,
    prev: bool,
    first: bool,
    last: bool,
    zoom_in: bool,
    zoom_out: bool,
    zoom_reset: bool,
    rot_left: bool,
    rot_right: bool,
    toggle_fav: bool,
    open_folder: bool,
    open_recent: Option<PathBuf>,
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
    show_in_explorer: bool,
    copy_to_clipboard: bool,
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

#[derive(Clone)]
struct Preferences {
    theme: ThemeMode,
    recent: Vec<PathBuf>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self { theme: ThemeMode::Auto, recent: Vec::new() }
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

    /// Full decodes on the GPU. Decoded pixels are moved straight into the
    /// texture; no CPU-side copy is kept, and textures far from the current
    /// image are evicted (see evict_textures).
    textures: HashMap<PathBuf, FullTexture>,
    /// Full decodes that failed. Not retried until the file changes on disk
    /// or the folder is reopened.
    failed: HashSet<PathBuf>,
    /// Textures whose file changed on disk; still shown until the new decode
    /// replaces them.
    stale: HashSet<PathBuf>,
    rotation: HashMap<PathBuf, i32>,
    sidecars: HashMap<PathBuf, Vec<PathBuf>>,

    /// Images whose header probe has landed (dims and orientation known,
    /// EXIF thumbnail uploaded if the file has one).
    probed: HashSet<PathBuf>,
    thumb_textures: HashMap<PathBuf, egui::TextureHandle>,
    /// Source pixel dimensions, in the same pre-rotation space as the
    /// textures (EXIF 3/6/8 rotations are applied at draw time).
    full_dims: HashMap<PathBuf, [usize; 2]>,
    exif_quarter: HashMap<PathBuf, i32>,

    full_q: Arc<PrioQueue>,
    probe_q: Arc<PrioQueue>,
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
    /// Regular decode target, derived from the monitor size each frame.
    target_dim: Arc<AtomicU32>,
    /// GPU texture size limit as reported by the renderer.
    max_texture_side: Arc<AtomicU32>,
    result_rx: mpsc::Receiver<JobResult>,
    /// Results of one-off background jobs (clipboard export, copy/cut).
    bg_tx: mpsc::Sender<BgEvent>,
    bg_rx: mpsc::Receiver<BgEvent>,
    file_job: Option<FileJobStatus>,
    ctx: egui::Context,

    fs_watcher: Option<notify::RecommendedWatcher>,
    fs_rx: Option<mpsc::Receiver<notify::Event>>,
    /// Images reported as written to, with the time of the latest event.
    /// Re-decoded once they've been quiet for FS_SETTLE.
    fs_modified: HashMap<PathBuf, Instant>,

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
    // Frames left to keep the panel background opaque after an auto-resize.
    // Without this, the window is transparent while the OS animates the
    // resize, and the desktop briefly shows through the rounded corners.
    resize_settle_frames: u8,
    // Last size we requested for the empty-state card. We re-request a resize
    // whenever the desired size changes (recent count grew, F1 help opened
    // and the card needs to expand to fit the dialog, etc.). Cleared when we
    // leave empty state so a fresh entry resizes again.
    empty_state_size: Option<(f32, f32)>,
    // Set while the window is shrunk to the start-screen card, so the next
    // image gets a normal viewing size instead of one derived from the card.
    restore_window_size: bool,
    actions: PendingActions,

    filter_mode: FilterMode,
    toast: Option<Toast>,

    pending_delete: Option<PathBuf>,
    last_image_rect: Option<egui::Rect>,
    displayed_path: Option<PathBuf>,
    /// +1 when the user was last navigating forward, -1 backward. Used by
    /// queue_preload / prioritize_probes to skew the preload window in the
    /// direction the user is most likely to keep flicking.
    nav_direction: i8,

    zoom: f32,
    target_zoom: f32,
    pan: egui::Vec2,
    target_pan: egui::Vec2,
    last_view_path: Option<PathBuf>,

    touch_swipe: Option<TouchSwipe>,
    /// Last title sent to the OS window (file name of the current image).
    window_title: String,
    /// The current image or the texture set changed since the last
    /// eviction pass.
    evict_dirty: bool,
    /// 2x2 checkerboard tiled behind images that have transparency.
    checker: Option<egui::TextureHandle>,
}

struct FullTexture {
    handle: egui::TextureHandle,
    /// Decoded at the HQ target rather than the screen-sized one (or the HQ
    /// decode was attempted and failed); never upgraded again.
    hq: bool,
    /// Every pixel is fully opaque, so no checkerboard is needed behind it.
    opaque: bool,
}

struct Toast {
    text: String,
    until: Instant,
}

enum BgEvent {
    Toast(String, f32),
    FileJobProgress { done: usize, total: usize },
    FileJobDone(FileJobReport),
}

struct FileJobStatus {
    cut: bool,
    done: usize,
    total: usize,
}

#[derive(Default)]
struct FileJobReport {
    cut: bool,
    dest: PathBuf,
    images: usize,
    sidecars: usize,
    skipped: usize,
    errors: Vec<String>,
    /// Images that left the folder (cut) and must leave the list.
    moved: Vec<PathBuf>,
}

#[derive(Clone, Copy)]
struct TouchSwipe {
    id: egui::TouchId,
    start: egui::Pos2,
    last: egui::Pos2,
    triggered: bool,
}

enum JobResult {
    Full {
        gen: u64,
        path: PathBuf,
        /// None when the file couldn't be decoded.
        decoded: Option<Decoded>,
        hq: bool,
    },
    Probe {
        gen: u64,
        path: PathBuf,
        probe: Probe,
    },
}

struct PrioQueueInner {
    queue: VecDeque<PathBuf>,
    in_queue: HashSet<PathBuf>,
    /// Popped by a worker and still being decoded. Never re-queued while
    /// here, so re-prioritizing on every keypress can't decode an image
    /// twice in parallel.
    in_flight: HashSet<PathBuf>,
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
                in_flight: HashSet::new(),
                closed: false,
            }),
            cv: Condvar::new(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PrioQueueInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn enqueue_back(&self, paths: &[PathBuf]) {
        let mut g = self.lock();
        for p in paths {
            if !g.in_flight.contains(p) && g.in_queue.insert(p.clone()) {
                g.queue.push_back(p.clone());
            }
        }
        self.cv.notify_all();
    }

    fn prioritize(&self, paths: &[PathBuf]) {
        let mut g = self.lock();
        for p in paths.iter().rev() {
            if let Some(pos) = g.queue.iter().position(|x| x == p) {
                g.queue.remove(pos);
                g.queue.push_front(p.clone());
            } else if !g.in_flight.contains(p) && g.in_queue.insert(p.clone()) {
                g.queue.push_front(p.clone());
            }
        }
        self.cv.notify_all();
    }

    /// Make `paths` (in order) the whole queue. Whatever was queued before
    /// and isn't wanted any more — images the user already flicked past —
    /// is dropped instead of being decoded after the user stops.
    fn replace(&self, paths: &[PathBuf]) {
        let mut g = self.lock();
        g.queue.clear();
        g.in_queue.clear();
        for p in paths {
            if !g.in_flight.contains(p) && g.in_queue.insert(p.clone()) {
                g.queue.push_back(p.clone());
            }
        }
        self.cv.notify_all();
    }

    fn clear(&self) {
        let mut g = self.lock();
        g.queue.clear();
        g.in_queue.clear();
    }

    /// Mark the queue as closed and wake every waiter so workers can exit
    /// their pop loop. Used from Drop to unwind decoder threads cleanly
    /// instead of leaving them parked on the Condvar at process exit.
    fn close(&self) {
        let mut g = self.lock();
        g.closed = true;
        self.cv.notify_all();
    }

    fn pop(&self) -> Option<PathBuf> {
        let mut g = self.lock();
        loop {
            if g.closed { return None; }
            if let Some(p) = g.queue.pop_front() {
                g.in_queue.remove(&p);
                g.in_flight.insert(p.clone());
                return Some(p);
            }
            g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Called by the worker once the result for `p` has been sent.
    fn done(&self, p: &Path) {
        self.lock().in_flight.remove(p);
    }
}

/// The decode machinery: a pool of full-decode workers and a single probe
/// worker (header reads, ~1 ms each, so one thread keeps up). Created in
/// main() before the window so the first image starts decoding while
/// eframe is still setting up.
struct Loader {
    full_q: Arc<PrioQueue>,
    probe_q: Arc<PrioQueue>,
    result_rx: mpsc::Receiver<JobResult>,
    generation: Arc<AtomicU64>,
    hq_mode: Arc<AtomicBool>,
    zoom_hq_paths: Arc<Mutex<VecDeque<PathBuf>>>,
    target_dim: Arc<AtomicU32>,
    max_texture_side: Arc<AtomicU32>,
    /// Set once the window exists; workers poke it after every job.
    repaint: Arc<OnceLock<egui::Context>>,
}

impl Loader {
    fn spawn() -> Self {
        let (tx, result_rx) = mpsc::channel::<JobResult>();
        let loader = Self {
            full_q: PrioQueue::new(),
            probe_q: PrioQueue::new(),
            result_rx,
            generation: Arc::new(AtomicU64::new(0)),
            hq_mode: Arc::new(AtomicBool::new(false)),
            zoom_hq_paths: Arc::new(Mutex::new(VecDeque::new())),
            target_dim: Arc::new(AtomicU32::new(FULL_MAX_DIM)),
            max_texture_side: Arc::new(AtomicU32::new(DEFAULT_MAX_TEXTURE_SIDE)),
            repaint: Arc::new(OnceLock::new()),
        };

        let n_full = num_workers().saturating_sub(1).max(2);
        for i in 0..n_full {
            let q = Arc::clone(&loader.full_q);
            let tx = tx.clone();
            let gen = Arc::clone(&loader.generation);
            let hq_mode = Arc::clone(&loader.hq_mode);
            let zoom_hq = Arc::clone(&loader.zoom_hq_paths);
            let target_dim = Arc::clone(&loader.target_dim);
            let max_side = Arc::clone(&loader.max_texture_side);
            let repaint = Arc::clone(&loader.repaint);
            let _ = thread::Builder::new().name(format!("decode-{i}")).spawn(move || {
                while let Some(path) = q.pop() {
                    let job_gen = gen.load(Ordering::Relaxed);
                    let hq = hq_mode.load(Ordering::Relaxed)
                        || zoom_hq.lock().unwrap_or_else(|e| e.into_inner()).contains(&path);
                    let max_side = max_side.load(Ordering::Relaxed);
                    let target = if hq { HQ_MAX_DIM } else { target_dim.load(Ordering::Relaxed) };
                    // A decoder panicking on a malformed file shows up as a
                    // failed image instead of taking the whole app down.
                    let decoded = std::panic::catch_unwind(|| {
                        decode_full(&path, target.min(max_side), max_side)
                    })
                    .ok()
                    .flatten();
                    let _ = tx.send(JobResult::Full { gen: job_gen, path: path.clone(), decoded, hq });
                    q.done(&path);
                    if let Some(ctx) = repaint.get() {
                        ctx.request_repaint();
                    }
                }
            });
        }

        let q = Arc::clone(&loader.probe_q);
        let gen = Arc::clone(&loader.generation);
        let repaint = Arc::clone(&loader.repaint);
        let _ = thread::Builder::new().name("probe".into()).spawn(move || {
            while let Some(path) = q.pop() {
                let job_gen = gen.load(Ordering::Relaxed);
                let probe = std::panic::catch_unwind(|| probe_image(&path)).unwrap_or_default();
                let _ = tx.send(JobResult::Probe { gen: job_gen, path: path.clone(), probe });
                q.done(&path);
                if let Some(ctx) = repaint.get() {
                    ctx.request_repaint();
                }
            }
        });

        loader
    }
}

impl SnapView {
    fn new(cc: &eframe::CreationContext<'_>, loader: Loader, initial_path: Option<PathBuf>) -> Self {
        let _ = loader.repaint.set(cc.egui_ctx.clone());
        // Ctrl +/-/0 zoom the image. Left on, egui would also rescale the
        // whole interface on those keys.
        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let Loader {
            full_q,
            probe_q,
            result_rx,
            generation,
            hq_mode,
            zoom_hq_paths,
            target_dim,
            max_texture_side,
            repaint: _,
        } = loader;
        let (bg_tx, bg_rx) = mpsc::channel::<BgEvent>();

        let mut app = Self {
            images: Vec::new(),
            current: 0,
            favorites: HashSet::new(),
            folder: None,
            textures: HashMap::new(),
            failed: HashSet::new(),
            stale: HashSet::new(),
            rotation: HashMap::new(),
            sidecars: HashMap::new(),
            probed: HashSet::new(),
            thumb_textures: HashMap::new(),
            full_dims: HashMap::new(),
            exif_quarter: HashMap::new(),
            full_q,
            probe_q,
            hq_mode,
            current_generation: generation,
            zoom_hq_paths,
            target_dim,
            max_texture_side,
            result_rx,
            bg_tx,
            bg_rx,
            file_job: None,
            ctx: cc.egui_ctx.clone(),
            fs_watcher: None,
            fs_rx: None,
            fs_modified: HashMap::new(),
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
            resize_settle_frames: 0,
            empty_state_size: None,
            restore_window_size: false,
            actions: PendingActions::default(),
            filter_mode: FilterMode::All,
            toast: None,
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
            window_title: String::new(),
            evict_dirty: false,
            checker: None,
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

    fn toast(&mut self, text: impl Into<String>, secs: f32) {
        self.toast = Some(Toast {
            text: text.into(),
            until: Instant::now() + Duration::from_secs_f32(secs),
        });
        self.ctx.request_repaint();
    }

    fn open_folder_with_file(&mut self, file: &Path) {
        if let Some(parent) = file.parent() {
            self.open_folder(parent, Some(file.to_path_buf()));
        }
    }

    fn open_folder(&mut self, folder: &Path, select: Option<PathBuf>) {
        let (images, sidecars) = scan_folder(folder);
        let reopening = self.folder.is_some();

        self.folder = Some(folder.to_path_buf());
        self.record_recent_folder(folder);
        self.favorites = load_favorites(folder, &images);
        self.images = images;
        self.sidecars = sidecars;
        self.textures.clear();
        self.failed.clear();
        self.stale.clear();
        self.probed.clear();
        self.thumb_textures.clear();
        self.full_dims.clear();
        self.exif_quarter.clear();
        self.metadata_cache.clear();
        self.rotation.clear();
        self.fs_modified.clear();
        self.zoom_hq_paths.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.displayed_path = None;
        self.nav_direction = 1;
        // A filter carried over from the previous folder would hide images
        // the user has never seen ("0 / 0 (favs only)").
        self.filter_mode = FilterMode::All;
        self.pending_delete = None;
        if self.show_filter {
            self.filter_selected = self.favorites.clone();
        }

        self.probe_q.clear();
        self.full_q.clear();
        self.start_fs_watch(folder);
        // Bump the generation so any in-flight decodes from the previous
        // folder are dropped on arrival in drain_results. Not on the first
        // open: main() may already be decoding this very image.
        if reopening {
            self.current_generation.fetch_add(1, Ordering::Relaxed);
        }

        self.current = select
            .as_deref()
            .and_then(|sel| self.find_selected(sel))
            .unwrap_or(0);

        // Lazy probe fill: only seed the immediate vicinity. The rest is added
        // on demand by prioritize_probes() as the user navigates.
        const PROBE_INITIAL_RADIUS: usize = 30;
        let n = self.images.len();
        let cur = self.current;
        let mut order: Vec<PathBuf> = Vec::with_capacity(PROBE_INITIAL_RADIUS * 2 + 1);
        if n > 0 { order.push(self.images[cur].clone()); }
        for d in 1..=PROBE_INITIAL_RADIUS.min(n.saturating_sub(1)) {
            if cur + d < n { order.push(self.images[cur + d].clone()); }
            if cur >= d { order.push(self.images[cur - d].clone()); }
        }
        self.probe_q.enqueue_back(&order);

        self.queue_preload();
        self.prioritize_probes();
        self.evict_dirty = true;
        if self.images.is_empty() {
            self.toast(format!("No images in {}", recent_label(folder)), 2.5);
        }
    }

    /// Index of `sel` in the list, falling back to a case-insensitive file
    /// name match: the shell may hand us a path whose casing differs from
    /// what read_dir reports.
    fn find_selected(&self, sel: &Path) -> Option<usize> {
        self.index_of(sel).or_else(|| {
            let name = sel.file_name()?.to_string_lossy().to_lowercase();
            self.images.iter().position(|p| {
                p.file_name().is_some_and(|n| n.to_string_lossy().to_lowercase() == name)
            })
        })
    }

    /// Position of `p` in `self.images`, which is kept sorted by compare_paths.
    fn index_of(&self, p: &Path) -> Option<usize> {
        self.images.binary_search_by(|x| compare_paths(x, p)).ok()
    }

    fn distance_to_current(&self, p: &Path) -> Option<usize> {
        self.index_of(p).map(|i| i.abs_diff(self.current))
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

    fn preload_radius(&self) -> usize {
        // HQ decodes are full resolution: keep fewer of them in flight and
        // resident at once.
        if self.hq_mode.load(Ordering::Relaxed) { 1 } else { PRELOAD_RADIUS }
    }

    fn prioritize_probes(&self) {
        if self.images.is_empty() { return; }
        const RADIUS: usize = 12;
        let pending: Vec<PathBuf> = self
            .directional_neighbors(RADIUS)
            .into_iter()
            .filter(|p| !self.probed.contains(p))
            .collect();
        if !pending.is_empty() {
            self.probe_q.prioritize(&pending);
        }
    }

    /// Point the full-decode queue at the images around the current one that
    /// still need a decode. This replaces the queue, so images the user has
    /// already flicked past are never decoded after the fact.
    fn queue_preload(&self) {
        if self.images.is_empty() {
            self.full_q.clear();
            return;
        }
        let wanted: Vec<PathBuf> = self
            .directional_neighbors(self.preload_radius())
            .into_iter()
            .filter(|p| self.needs_full_decode(p))
            .collect();
        self.full_q.replace(&wanted);
    }

    fn needs_full_decode(&self, p: &Path) -> bool {
        if self.failed.contains(p) { return false; }
        if self.stale.contains(p) { return true; }
        match self.textures.get(p) {
            None => true,
            Some(t) => !t.hq && self.wants_hq(p) && !self.texture_is_complete(p, t),
        }
    }

    fn wants_hq(&self, p: &Path) -> bool {
        self.hq_mode.load(Ordering::Relaxed)
            || self
                .zoom_hq_paths
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|x| x == p)
    }

    /// The texture already holds every source pixel, so an HQ decode would
    /// produce exactly the same thing.
    fn texture_is_complete(&self, p: &Path, t: &FullTexture) -> bool {
        let [tw, th] = t.handle.size();
        self.full_dims.get(p).is_none_or(|d| tw >= d[0] && th >= d[1])
    }

    /// (Re)start the filesystem watcher for the freshly-opened folder. Drops
    /// any prior watcher (which stops watching the old path) and stands up a
    /// fresh one that sends events into self.fs_rx. Failures (network
    /// drives, permission denied) silently skip — the app keeps working,
    /// the user just doesn't get auto-refresh.
    fn start_fs_watch(&mut self, folder: &Path) {
        use notify::event::{AccessKind, AccessMode, ModifyKind};
        use notify::{EventKind, Watcher};
        self.fs_watcher = None;
        self.fs_rx = None;
        let (tx, rx) = mpsc::channel::<notify::Event>();
        let ctx = self.ctx.clone();
        let mut watcher = match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            // Only what drain_fs_events acts on: on Linux every open() by our
            // own decoders is an event too, and each would wake the UI.
            let relevant = matches!(
                ev.kind,
                EventKind::Create(_)
                    | EventKind::Remove(_)
                    | EventKind::Modify(ModifyKind::Name(_) | ModifyKind::Data(_) | ModifyKind::Any)
                    | EventKind::Access(AccessKind::Close(AccessMode::Write))
            );
            if relevant && tx.send(ev).is_ok() {
                // Without this the change would sit in the channel until the
                // next mouse move.
                ctx.request_repaint();
            }
        }) {
            Ok(w) => w,
            Err(_) => return,
        };
        if watcher
            .watch(folder, notify::RecursiveMode::NonRecursive)
            .is_err()
        {
            return;
        }
        self.fs_watcher = Some(watcher);
        self.fs_rx = Some(rx);
    }

    /// Pull all pending FS events and reconcile self.images with the changes.
    /// Only fires for the current folder (NonRecursive); cross-folder moves
    /// arrive as Create/Remove pairs.
    fn drain_fs_events(&mut self) {
        use notify::event::{AccessKind, AccessMode, ModifyKind};
        use notify::EventKind;
        let Some(folder) = self.folder.clone() else { return };
        let Some(rx) = self.fs_rx.as_ref() else { return };
        let mut added: Vec<PathBuf> = Vec::new();
        let mut removed: Vec<PathBuf> = Vec::new();
        let mut written: Vec<PathBuf> = Vec::new();
        while let Ok(event) = rx.try_recv() {
            for p in event.paths {
                // Ignore anything that isn't inside the active folder.
                if p.parent() != Some(folder.as_path()) {
                    continue;
                }
                match event.kind {
                    EventKind::Create(_) => added.push(p),
                    EventKind::Remove(_) => removed.push(p),
                    EventKind::Modify(ModifyKind::Name(_)) => {
                        // Rename: depending on platform we get one Modify
                        // with two paths, or separate Create/Remove. Treat
                        // any name event as a potential add/remove — we
                        // check exists() below to disambiguate.
                        if p.exists() { added.push(p); } else { removed.push(p); }
                    }
                    EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Any)
                    | EventKind::Access(AccessKind::Close(AccessMode::Write)) => written.push(p),
                    _ => {}
                }
            }
        }
        // RAW files aren't listed, but a JPEG's sidecars can appear or go
        // away with them.
        let sidecars_changed = added.iter().chain(&removed).any(|p| is_raw_sidecar(p) || is_image(p));
        let now = Instant::now();
        for p in removed {
            if p.exists() {
                // Removed and put back already: a save that replaces the
                // file. Treat it as a change rather than dropping the image
                // and re-adding it, which would move the view off it.
                if self.index_of(&p).is_some() {
                    self.fs_modified.insert(p, now);
                }
            } else {
                self.handle_fs_remove(&p);
            }
        }
        for p in added {
            self.handle_fs_add(&p);
        }
        for p in written {
            if self.index_of(&p).is_some() {
                self.fs_modified.insert(p, now);
            }
        }
        if !self.fs_modified.is_empty() {
            let settled: Vec<PathBuf> = self
                .fs_modified
                .iter()
                .filter(|(_, t)| now.duration_since(**t) >= FS_SETTLE)
                .map(|(p, _)| p.clone())
                .collect();
            for p in settled {
                self.fs_modified.remove(&p);
                self.invalidate_image(&p);
            }
            if !self.fs_modified.is_empty() {
                self.ctx.request_repaint_after(FS_SETTLE);
            }
        }
        if sidecars_changed {
            self.sidecars = scan_folder(&folder).1;
        }
    }

    fn handle_fs_add(&mut self, path: &Path) {
        if !is_listable_image(path) { return; }
        match self.images.binary_search_by(|p| compare_paths(p, path)) {
            // Already listed: replaced in place. Editors often save by writing
            // a temp file and renaming it over the original.
            Ok(_) => {
                self.fs_modified.insert(path.to_path_buf(), Instant::now());
                self.ctx.request_repaint_after(FS_SETTLE);
            }
            Err(pos) => {
                let was_empty = self.images.is_empty();
                self.images.insert(pos, path.to_path_buf());
                // Insertion at or before current shifts current up by one to
                // keep pointing at the same image the user was looking at.
                if !was_empty && pos <= self.current {
                    self.current += 1;
                }
                // Reload favorites from disk in case the user dropped a new
                // file already listed there.
                if let Some(folder) = &self.folder {
                    self.favorites = load_favorites(folder, &self.images);
                }
                self.after_navigation();
                self.toast(format!("Added: {}", file_label(path)), 1.5);
            }
        }
    }

    fn handle_fs_remove(&mut self, path: &Path) {
        let was_current = self.current_path().as_deref() == Some(path);
        if !self.remove_from_list(path) { return; }
        self.favorites.remove(path);
        if was_current {
            self.ensure_current_visible();
        }
        self.after_navigation();
        self.toast(format!("Removed: {}", file_label(path)), 1.5);
    }

    /// A listed file changed on disk. Forget what we know about it and
    /// decode it again; the current image keeps its old pixels on screen
    /// until the new ones land.
    fn invalidate_image(&mut self, path: &Path) {
        let Some(dist) = self.distance_to_current(path) else { return };
        self.failed.remove(path);
        self.probed.remove(path);
        self.metadata_cache.remove(path);
        self.thumb_textures.remove(path);
        if dist <= self.preload_radius() && self.textures.contains_key(path) {
            self.stale.insert(path.to_path_buf());
        } else {
            self.textures.remove(path);
        }
        self.queue_preload();
        self.prioritize_probes();
    }

    /// Drop every piece of per-image state for `path`. Leaves the list alone.
    fn forget_image(&mut self, path: &Path) {
        self.textures.remove(path);
        self.thumb_textures.remove(path);
        self.failed.remove(path);
        self.stale.remove(path);
        self.probed.remove(path);
        self.rotation.remove(path);
        self.exif_quarter.remove(path);
        self.full_dims.remove(path);
        self.sidecars.remove(path);
        self.metadata_cache.remove(path);
        self.filter_selected.remove(path);
        self.fs_modified.remove(path);
        if self.pending_delete.as_deref() == Some(path) {
            self.pending_delete = None;
        }
    }

    /// Remove `path` from the list. When another image goes, `current` keeps
    /// pointing at the same image; when the current one goes, the image that
    /// slides into its slot (the next one) becomes current — most viewers'
    /// "delete, then show the following image".
    fn remove_from_list(&mut self, path: &Path) -> bool {
        let Some(idx) = self.index_of(path) else { return false };
        self.forget_image(path);
        self.images.remove(idx);
        if idx < self.current {
            self.current -= 1;
        }
        if self.current >= self.images.len() {
            self.current = self.images.len().saturating_sub(1);
        }
        self.evict_dirty = true;
        true
    }

    /// If the filter hides the current image, move forward to the nearest
    /// image it shows.
    fn ensure_current_visible(&mut self) {
        let n = self.images.len();
        if n == 0 || self.matches_filter(&self.images[self.current]) || self.visible_count() == 0 {
            return;
        }
        for d in 1..n {
            let f = (self.current + d) % n;
            if self.matches_filter(&self.images[f]) {
                self.current = f;
                break;
            }
        }
    }

    /// Push the just-opened folder to the front of the recents list, dedupe,
    /// cap at MAX_RECENT, and persist preferences.
    fn record_recent_folder(&mut self, folder: &Path) {
        const MAX_RECENT: usize = 10;
        let f = folder.to_path_buf();
        self.prefs.recent.retain(|p| p != &f);
        self.prefs.recent.insert(0, f.clone());
        self.prefs.recent.truncate(MAX_RECENT);
        let _ = save_preferences(&self.prefs);
        // Tell Windows about it too — populates the OS "Recent items" list
        // and the AppUserModelID-keyed Jump List under our taskbar icon.
        #[cfg(windows)]
        sh_add_to_recent_docs(f);
    }

    /// Open the platform file manager focused on the currently displayed
    /// image. On Windows that's `explorer.exe /select,path` which highlights
    /// the file inside its folder; macOS `open -R path` does the same;
    /// Linux falls back to `xdg-open <folder>` since there's no portable way
    /// to highlight a specific file.
    fn show_current_in_file_manager(&mut self) {
        let Some(path) = self.current_path() else { return };
        #[cfg(windows)]
        let res = {
            use std::os::windows::process::CommandExt;
            // explorer.exe parses its own command line and wants
            // `/select,"C:\some dir\file.jpg"`. Rust's default quoting would
            // wrap the whole argument, which breaks paths with spaces.
            std::process::Command::new("explorer.exe")
                .raw_arg(format!("/select,\"{}\"", path.display()))
                .spawn()
                .map(|_| ())
        };
        #[cfg(target_os = "macos")]
        let res = std::process::Command::new("open")
            .arg("-R")
            .arg(&path)
            .spawn()
            .map(|_| ());
        #[cfg(not(any(windows, target_os = "macos")))]
        let res = std::process::Command::new("xdg-open")
            .arg(path.parent().unwrap_or(Path::new(".")))
            .spawn()
            .map(|_| ());
        match res {
            Ok(_) => self.toast("Opened in file manager", 1.2),
            Err(e) => self.toast(format!("Couldn't open file manager: {}", e), 2.5),
        }
    }

    /// Copy the current image to the clipboard as a bitmap, at full
    /// resolution and turned the way it's shown (EXIF orientation plus any
    /// Q/W rotation). Decoding a big photo takes a moment, so it runs on a
    /// background thread and reports back through a toast.
    fn copy_current_to_clipboard(&mut self) {
        let Some(path) = self.current_path() else { return };
        let user_quarter = self.rotation.get(&path).copied().unwrap_or(0);
        let tx = self.bg_tx.clone();
        let ctx = self.ctx.clone();
        self.toast("Copying to clipboard…", 10.0);
        let _ = thread::Builder::new().name("clipboard".into()).spawn(move || {
            let exported = std::panic::catch_unwind(|| export_image(&path, user_quarter)).ok().flatten();
            let msg = match exported {
                None => "Couldn't read the image".to_string(),
                Some(img) => {
                    let (w, h) = img.dimensions();
                    let data = arboard::ImageData {
                        width: w as usize,
                        height: h as usize,
                        bytes: std::borrow::Cow::Owned(img.into_raw()),
                    };
                    match arboard::Clipboard::new().and_then(|mut cb| cb.set_image(data)) {
                        Ok(()) => format!("Copied to clipboard ({} × {})", w, h),
                        Err(e) => format!("Clipboard error: {}", e),
                    }
                }
            };
            let _ = tx.send(BgEvent::Toast(msg, 1.8));
            ctx.request_repaint();
        });
    }

    fn drain_results(&mut self, ctx: &egui::Context) {
        let cur_gen = self.current_generation.load(Ordering::Relaxed);
        let mut got_full = false;
        while let Ok(res) = self.result_rx.try_recv() {
            match res {
                JobResult::Full { gen, path, decoded, hq } => {
                    // Drop anything that was decoded for a previous folder
                    // open, or for a file that has left the list since.
                    if gen != cur_gen { continue; }
                    let Some(dist) = self.distance_to_current(&path) else { continue };
                    got_full = true;
                    let was_stale = self.stale.remove(&path);
                    let Some(d) = decoded else {
                        match self.textures.get_mut(&path) {
                            // Only the HQ upgrade failed (e.g. too large to
                            // decode at full size): keep the regular texture
                            // and don't try again.
                            Some(t) if hq && !was_stale => t.hq = true,
                            _ => {
                                self.failed.insert(path.clone());
                                self.textures.remove(&path);
                            }
                        }
                        continue;
                    };
                    self.full_dims.insert(path.clone(), d.src_dims);
                    self.exif_quarter.insert(path.clone(), d.quarter);
                    // Landed after the user flicked well past it: not worth a
                    // GPU upload. It's decoded again if they come back.
                    if dist > self.preload_radius() + 2 { continue; }
                    let max_side = self.max_texture_side.load(Ordering::Relaxed) as usize;
                    let image = clamp_to_texture_limit(d.image, max_side);
                    let handle = ctx.load_texture(
                        format!("full:{}", path.display()),
                        image,
                        full_texture_options(),
                    );
                    self.textures.insert(path, FullTexture { handle, hq, opaque: d.opaque });
                }
                JobResult::Probe { gen, path, probe } => {
                    if gen != cur_gen || self.index_of(&path).is_none() { continue; }
                    self.probed.insert(path.clone());
                    self.exif_quarter.insert(path.clone(), probe.quarter);
                    if let Some(d) = probe.dims {
                        self.full_dims.insert(path.clone(), d);
                    }
                    if let Some(thumb) = probe.thumb {
                        let tex = ctx.load_texture(
                            format!("thumb:{}", path.display()),
                            thumb,
                            egui::TextureOptions::LINEAR,
                        );
                        self.thumb_textures.insert(path, tex);
                    }
                }
            }
        }
        if got_full {
            // Lets a zoomed-in image whose regular decode just landed move on
            // to its HQ decode.
            self.queue_preload();
            self.evict_dirty = true;
        }
    }

    fn drain_bg_events(&mut self) {
        while let Ok(ev) = self.bg_rx.try_recv() {
            match ev {
                BgEvent::Toast(text, secs) => self.toast(text, secs),
                BgEvent::FileJobProgress { done, total } => {
                    if let Some(job) = self.file_job.as_mut() {
                        job.done = done;
                        job.total = total;
                    }
                }
                BgEvent::FileJobDone(report) => self.finish_file_job(report),
            }
        }
    }

    /// Bound GPU memory: keep the preload window and what's on screen, plus
    /// the nearest other full textures up to TEXTURE_KEEP_MAX and
    /// TEXTURE_BUDGET_BYTES; drop EXIF thumbnails far from the current image.
    fn evict_textures(&mut self) {
        if !std::mem::take(&mut self.evict_dirty) { return; }
        let radius = self.preload_radius();
        let mut by_distance: Vec<(usize, PathBuf, usize)> = self
            .textures
            .iter()
            .map(|(p, t)| {
                let [w, h] = t.handle.size();
                // RGBA plus a third for the mip chain.
                let bytes = w * h * 4 * 4 / 3;
                (self.distance_to_current(p).unwrap_or(usize::MAX), p.clone(), bytes)
            })
            .collect();
        by_distance.sort_by_key(|e| e.0);
        let (mut count, mut bytes) = (0usize, 0usize);
        for (dist, p, size) in by_distance {
            let on_screen = self.displayed_path.as_ref() == Some(&p);
            let keep = dist != usize::MAX
                && (dist <= radius
                    || on_screen
                    || (count < TEXTURE_KEEP_MAX && bytes + size <= TEXTURE_BUDGET_BYTES));
            if keep {
                count += 1;
                bytes += size;
            } else {
                self.textures.remove(&p);
                self.stale.remove(&p);
            }
        }
        let far: Vec<PathBuf> = self
            .thumb_textures
            .keys()
            .filter(|p| self.distance_to_current(p).is_none_or(|d| d > THUMB_KEEP_RADIUS))
            .cloned()
            .collect();
        for p in far {
            self.thumb_textures.remove(&p);
            self.probed.remove(&p);
        }
    }

    /// Hand the decoders what only the window knows: the GPU texture size
    /// limit and the monitor size in physical pixels.
    fn update_decode_limits(&self, ctx: &egui::Context) {
        let (max_side, monitor, native_ppp) = ctx.input(|i| {
            (i.max_texture_side, i.viewport().monitor_size, i.viewport().native_pixels_per_point)
        });
        if max_side > 0 {
            self.max_texture_side
                .store(max_side.min(u32::MAX as usize) as u32, Ordering::Relaxed);
        }
        if let Some(m) = monitor {
            let ppp = native_ppp.unwrap_or_else(|| ctx.pixels_per_point());
            let long = (m.x.max(m.y) * ppp).round().max(0.0) as u32;
            self.target_dim
                .store(long.clamp(FULL_MIN_DIM, FULL_MAX_DIM), Ordering::Relaxed);
        }
    }

    fn update_window_title(&mut self, ctx: &egui::Context) {
        let title = match self.current_path() {
            Some(p) => format!("{} — snapview", file_label(&p)),
            None => "snapview".to_string(),
        };
        if title != self.window_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.window_title = title;
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
                self.after_navigation();
                return;
            }
        }
    }

    fn next(&mut self) { self.step(true); }
    fn prev(&mut self) { self.step(false); }

    /// Jump to the first (Home) or last (End) image the filter shows.
    fn go_to_edge(&mut self, last: bool) {
        let n = self.images.len();
        let found = if last {
            (0..n).rev().find(|&i| self.matches_filter(&self.images[i]))
        } else {
            (0..n).find(|&i| self.matches_filter(&self.images[i]))
        };
        if let Some(i) = found.filter(|&i| i != self.current) {
            // From the end the only way is back; bias preloading that way.
            self.nav_direction = if last { -1 } else { 1 };
            self.current = i;
            self.after_navigation();
        }
    }

    fn after_navigation(&mut self) {
        self.queue_preload();
        self.prioritize_probes();
        self.evict_dirty = true;
    }

    fn cycle_filter(&mut self) {
        self.filter_mode = self.filter_mode.next();
        let count = self.visible_count();
        self.toast(
            format!("Filter: {}  ({} images)", self.filter_mode.label(), count),
            1.5,
        );
        if count == 0 { return; }
        self.ensure_current_visible();
        self.after_navigation();
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
        if self.index_of(&path).is_none() { return; }
        let mut to_trash: Vec<PathBuf> = vec![path.clone()];
        if let Some(side) = self.sidecars.get(&path) {
            to_trash.extend(side.iter().cloned());
        }
        if let Err(e) = trash::delete_all(&to_trash) {
            self.toast(format!("Delete failed: {}", e), 3.0);
            return;
        }
        if self.favorites.remove(&path) {
            self.persist_favorites();
        }
        self.remove_from_list(&path);
        self.ensure_current_visible();
        let extra = to_trash.len() - 1;
        let msg = if extra > 0 {
            format!("Moved to trash (+ {} sidecar{})", extra, plural(extra))
        } else {
            "Moved to trash".to_string()
        };
        self.toast(msg, 1.2);
        self.after_navigation();
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
            self.toast(format!("Couldn't save favorites: {}", e), 3.0);
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
        // re-decode in the background, unless its texture already has every
        // source pixel. Promotions are kept as a bounded FIFO (most recent at
        // the back) so that long browsing sessions don't accumulate dozens of
        // native-resolution textures. The screen-sized texture stays on
        // screen until the HQ one replaces it.
        if self.target_zoom > 1.05 {
            if let Some(p) = self.current_path() {
                let have_all_pixels = self
                    .textures
                    .get(&p)
                    .is_some_and(|t| t.hq || self.texture_is_complete(&p, t));
                if !have_all_pixels {
                    const ZOOM_HQ_MAX: usize = 20;
                    let mut set = self.zoom_hq_paths.lock().unwrap_or_else(|e| e.into_inner());
                    if !set.iter().any(|x| x == &p) {
                        if set.len() >= ZOOM_HQ_MAX {
                            set.pop_front();
                        }
                        set.push_back(p);
                        drop(set);
                        self.queue_preload();
                    }
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

    /// Frameless windows don't get OS-provided edge-drag-to-resize, so we run
    /// our own hit-test on every frame: when the pointer is within a thin
    /// band of any edge/corner of the window, swap the cursor to the matching
    /// resize glyph and, on primary press, hand off to the OS via
    /// `ViewportCommand::BeginResize`. Skipped in fullscreen.
    fn handle_window_resize(&self, ctx: &egui::Context) {
        if self.is_fullscreen { return; }
        let rect = ctx.screen_rect();
        let pos = match ctx.input(|i| i.pointer.hover_pos()) {
            Some(p) => p,
            None => return,
        };
        if !rect.contains(pos) { return; }

        let edge = 8.0_f32;
        let corner = 14.0_f32;
        let on_left = pos.x - rect.left() < edge;
        let on_right = rect.right() - pos.x < edge;
        let on_top = pos.y - rect.top() < edge;
        let on_bottom = rect.bottom() - pos.y < edge;
        let near_left = pos.x - rect.left() < corner;
        let near_right = rect.right() - pos.x < corner;
        let near_top = pos.y - rect.top() < corner;
        let near_bottom = rect.bottom() - pos.y < corner;

        // Corners win over edges so a 14x14 box at each corner always picks
        // the diagonal resize direction even if only one axis is within the
        // edge band.
        let direction = if (on_top && near_left) || (near_top && on_left) {
            Some(egui::ResizeDirection::NorthWest)
        } else if (on_top && near_right) || (near_top && on_right) {
            Some(egui::ResizeDirection::NorthEast)
        } else if (on_bottom && near_left) || (near_bottom && on_left) {
            Some(egui::ResizeDirection::SouthWest)
        } else if (on_bottom && near_right) || (near_bottom && on_right) {
            Some(egui::ResizeDirection::SouthEast)
        } else if on_top {
            Some(egui::ResizeDirection::North)
        } else if on_bottom {
            Some(egui::ResizeDirection::South)
        } else if on_left {
            Some(egui::ResizeDirection::West)
        } else if on_right {
            Some(egui::ResizeDirection::East)
        } else {
            None
        };

        let dir = match direction {
            Some(d) => d,
            None => return,
        };

        let cursor = match dir {
            egui::ResizeDirection::NorthWest | egui::ResizeDirection::SouthEast => {
                egui::CursorIcon::ResizeNwSe
            }
            egui::ResizeDirection::NorthEast | egui::ResizeDirection::SouthWest => {
                egui::CursorIcon::ResizeNeSw
            }
            egui::ResizeDirection::North | egui::ResizeDirection::South => {
                egui::CursorIcon::ResizeVertical
            }
            egui::ResizeDirection::East | egui::ResizeDirection::West => {
                egui::CursorIcon::ResizeHorizontal
            }
        };
        ctx.set_cursor_icon(cursor);

        if ctx.input(|i| i.pointer.primary_pressed()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(dir));
        }
    }

    /// When windowed, only on a portrait ↔ landscape transition: rescale the
    /// window's width to the new image's aspect, leave height untouched, and
    /// reposition so the window stays centered around its previous middle.
    /// Goal is the visual effect "the picture shrunk into portrait" rather
    /// than "the window jumped sideways". Coming from the start-screen card,
    /// the window instead grows back to a normal viewing size around the
    /// card's center.
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
        let class = aspect_class(iw, ih);
        let restore = self.restore_window_size;

        // Skip when aspect class hasn't changed: keep current window unchanged.
        if !restore && Some(class) == self.last_aspect_class {
            self.last_resized_path = Some(path);
            return;
        }

        // Need actual window geometry on screen to recenter. egui makes this
        // available via ViewportInfo; if it's not populated yet, defer to
        // next frame.
        let (outer, monitor) = ctx.input(|i| (i.viewport().outer_rect, i.viewport().monitor_size));
        let outer = match outer {
            Some(r) => r,
            None => return,
        };
        let height = if restore { DEFAULT_WINDOW_HEIGHT } else { outer.height() };
        let (new_w, new_h) = fit_window_to_aspect(aspect, height, monitor);
        let center = outer.center();
        let mut pos = if restore {
            center - egui::vec2(new_w, new_h) / 2.0
        } else {
            egui::pos2(center.x - new_w / 2.0, outer.top())
        };
        // Keep the window on screen (a panorama next to the screen edge, or
        // the card growing near the top). Only when the window is on a monitor
        // anchored at the origin — the one coordinate system we know.
        if let Some(m) = monitor {
            if (0.0..=m.x).contains(&center.x) && (0.0..=m.y).contains(&center.y) {
                pos.x = pos.x.clamp(0.0, (m.x - new_w).max(0.0));
                pos.y = pos.y.clamp(0.0, (m.y - new_h).max(0.0));
            }
        }
        self.restore_window_size = false;
        self.last_aspect_class = Some(class);
        self.last_resized_path = Some(path);

        if (outer.width() - new_w).abs() < 2.0 && (outer.height() - new_h).abs() < 2.0 {
            return;
        }

        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(new_w, new_h)));
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
        // Keep the window opaque for a few frames so the user doesn't see the
        // desktop through the rounded corners while the OS animates the resize.
        self.resize_settle_frames = 12;
        ctx.request_repaint();
    }

    /// Returns the on-screen (post-rotation) dimensions of the image, using
    /// whatever is known: source dims from the probe or decode, else the
    /// size of whatever texture we have. EXIF and user rotation both
    /// factored in.
    fn display_dims(&self, path: &Path) -> Option<(usize, usize)> {
        let [w, h] = self
            .full_dims
            .get(path)
            .copied()
            .or_else(|| self.textures.get(path).map(|t| t.handle.size()))
            .or_else(|| self.thumb_textures.get(path).map(|t| t.size()))?;
        let rot = self.rotation.get(path).copied().unwrap_or(0)
            + self.exif_quarter.get(path).copied().unwrap_or(0);
        if rot.rem_euclid(2) == 0 { Some((w, h)) } else { Some((h, w)) }
    }

    /// Copy (or move) the selected favorites, with their RAW sidecars, into
    /// `dest` on a background thread. Progress and the result come back as
    /// BgEvents; the UI stays responsive for big batches on slow drives.
    fn start_file_job(&mut self, dest: PathBuf, cut: bool) {
        if self.file_job.is_some() || self.filter_selected.is_empty() { return; }
        let mut items: Vec<(PathBuf, Vec<PathBuf>)> = self
            .filter_selected
            .iter()
            .map(|p| (p.clone(), self.sidecars.get(p).cloned().unwrap_or_default()))
            .collect();
        items.sort_by(|a, b| compare_paths(&a.0, &b.0));
        self.file_job = Some(FileJobStatus { cut, done: 0, total: items.len() });
        let tx = self.bg_tx.clone();
        let ctx = self.ctx.clone();
        let spawned = thread::Builder::new().name("file-job".into()).spawn(move || {
            let report = run_file_job(&items, &dest, cut, |done, total| {
                let _ = tx.send(BgEvent::FileJobProgress { done, total });
                ctx.request_repaint();
            });
            let _ = tx.send(BgEvent::FileJobDone(report));
            ctx.request_repaint();
        });
        if spawned.is_err() {
            self.file_job = None;
            self.toast("Couldn't start the copy", 2.5);
        }
    }

    fn finish_file_job(&mut self, r: FileJobReport) {
        self.file_job = None;
        if r.cut && !r.moved.is_empty() {
            for p in &r.moved {
                self.favorites.remove(p);
                self.remove_from_list(p);
            }
            self.persist_favorites();
            self.ensure_current_visible();
            self.after_navigation();
        }
        let mut msg = format!(
            "{} {} image{}",
            if r.cut { "Moved" } else { "Copied" },
            r.images,
            plural(r.images)
        );
        if r.sidecars > 0 {
            msg.push_str(&format!(" + {} RAW/sidecar", r.sidecars));
        }
        msg.push_str(&format!(" to {}", recent_label(&r.dest)));
        if r.skipped > 0 {
            msg.push_str(&format!("  ·  {} skipped (already there)", r.skipped));
        }
        if r.errors.is_empty() {
            self.toast(msg, 3.5);
            return;
        }
        let mut desc = format!(
            "{}.\n\n{} file{} failed:\n",
            msg,
            r.errors.len(),
            plural(r.errors.len())
        );
        for e in r.errors.iter().take(12) {
            desc.push_str(&format!("• {}\n", e));
        }
        if r.errors.len() > 12 {
            desc.push_str(&format!("… and {} more", r.errors.len() - 12));
        }
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title(if r.cut { "Cut" } else { "Copy" })
            .set_description(desc)
            .show();
    }
}

impl Drop for SnapView {
    fn drop(&mut self) {
        // Wake the decoder threads so they can exit their blocking pop()
        // loop. The threads detach naturally with the process, but closing
        // explicitly avoids the zombie-on-restart case and quiets shutdown
        // diagnostics from sanitizers.
        self.full_q.close();
        self.probe_q.close();
    }
}

impl eframe::App for SnapView {
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.apply_theme(ctx);
        self.update_decode_limits(ctx);
        self.drain_results(ctx);
        self.drain_fs_events();
        self.drain_bg_events();

        // Reset view when current image changes
        let cur_path = self.current_path();
        if cur_path != self.last_view_path {
            self.reset_view();
            self.last_view_path = cur_path;
            self.evict_dirty = true;
        }
        self.evict_textures();
        self.update_window_title(ctx);

        // stable_dt, not unstable_dt: after the app sat idle the latter is the
        // whole idle time, which would skip the first frame of any animation.
        let dt = ctx.input(|i| i.stable_dt).min(0.1);
        if self.animate_view(dt) { ctx.request_repaint(); }

        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));

        let modal_open = self.pending_delete.is_some();
        let dialog_open = self.show_help || self.show_prefs;

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
            // Esc closes whatever is on top first; only then does it leave
            // fullscreen, and only then quit.
            if i.key_pressed(egui::Key::Escape) {
                if self.show_help { self.show_help = false; }
                else if self.show_prefs { self.show_prefs = false; }
                else if self.show_filter { self.show_filter = false; }
                else if self.is_fullscreen { self.actions.toggle_max = true; }
                else { self.actions.quit = true; }
                return;
            }
            // Help and Preferences are modal: keys don't reach the image.
            if dialog_open {
                if self.show_help && i.key_pressed(egui::Key::F1) { self.actions.toggle_help = true; }
                return;
            }
            if self.show_filter { return; }
            let cmd = i.modifiers.command;
            if i.key_pressed(egui::Key::ArrowRight)
                || i.pointer.button_pressed(egui::PointerButton::Extra2)
            {
                self.actions.next = true;
            }
            if i.key_pressed(egui::Key::ArrowLeft)
                || i.pointer.button_pressed(egui::PointerButton::Extra1)
            {
                self.actions.prev = true;
            }
            if i.key_pressed(egui::Key::Home) { self.actions.first = true; }
            if i.key_pressed(egui::Key::End) { self.actions.last = true; }
            if i.key_pressed(egui::Key::Q) { self.actions.rot_left = true; }
            if i.key_pressed(egui::Key::W) { self.actions.rot_right = true; }
            if i.key_pressed(egui::Key::Space) { self.actions.toggle_fav = true; }
            if i.key_pressed(egui::Key::Z) { self.actions.request_hq = true; }
            if i.key_pressed(egui::Key::F) {
                if i.modifiers.shift { self.actions.open_filter = true; }
                else { self.actions.cycle_filter = true; }
            }
            if i.key_pressed(egui::Key::Delete) { self.actions.delete = true; }
            if i.key_pressed(egui::Key::O) && cmd { self.actions.open_folder = true; }
            if i.key_pressed(egui::Key::F11) { self.actions.toggle_max = true; }
            if i.key_pressed(egui::Key::Enter) && i.modifiers.alt { self.actions.toggle_max = true; }
            if i.key_pressed(egui::Key::F1) { self.actions.toggle_help = true; }
            if i.key_pressed(egui::Key::I) { self.actions.toggle_metadata = true; }
            if i.key_pressed(egui::Key::E) && cmd { self.actions.show_in_explorer = true; }
            // egui-winit turns Ctrl+C into Event::Copy and drops the key
            // event, so checking Key::C alone never fires.
            if i.events.iter().any(|e| matches!(e, egui::Event::Copy))
                || (i.key_pressed(egui::Key::C) && cmd)
            {
                self.actions.copy_to_clipboard = true;
            }
            if i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals) {
                self.actions.zoom_in = true;
            }
            if i.key_pressed(egui::Key::Minus) { self.actions.zoom_out = true; }
            if i.key_pressed(egui::Key::Num0) { self.actions.zoom_reset = true; }
        });

        let empty_state = self.images.is_empty();

        // Scroll / pinch -> zoom (cursor-anchored)
        if !self.show_filter && !modal_open && !dialog_open && !empty_state {
            let (scroll_y, pinch, hover) = ctx.input(|i| {
                // Ctrl+scroll shows up in raw_scroll_delta *and* in egui's
                // zoom_delta(), so only the raw delta is read here. Pinch is
                // multi-touch on touchscreens, Event::Zoom on trackpads.
                let pinch = match i.multi_touch() {
                    Some(t) => t.zoom_delta,
                    None => i
                        .events
                        .iter()
                        .filter_map(|e| match e {
                            egui::Event::Zoom(f) => Some(*f),
                            _ => None,
                        })
                        .product(),
                };
                (i.raw_scroll_delta.y, pinch, i.pointer.hover_pos())
            });
            let mut factor = 1.0_f32;
            if scroll_y.abs() > 0.5 { factor *= (scroll_y * 0.0018).exp(); }
            if (pinch - 1.0).abs() > 0.0005 { factor *= pinch; }
            if (factor - 1.0).abs() > 0.0001 {
                self.apply_zoom(factor, hover, ctx);
            }
        }

        // Windowed view auto-fits the window to the image's aspect, so when
        // the image fills the window at zoom 1 we can drop the panel fill and
        // let the rounded image itself be the visible "rounded window" — same
        // effect in both portrait and landscape. Only while something is
        // actually painted: while the first decode is still running (or it
        // failed) the panel is the only thing painting pixels, and a
        // transparent fill would leave the window invisible and click-through.
        let drawable = self
            .current_path()
            .is_some_and(|p| self.pick_draw_path(&p).is_some());
        // In empty state the rendered card IS the window — same trick as the
        // rounded image in image mode. Drop the panel fill so the transparent
        // window outside the rounded card lets the desktop show, and size the
        // window down to the card so there's no big dark frame around it.
        let suppress_dim = (drawable && !self.is_fullscreen && self.target_zoom <= 1.001)
            || (empty_state && !self.is_fullscreen);
        if empty_state && !self.is_fullscreen {
            let want = empty_state_card_size(self.prefs.recent.len(), self.show_help);
            let needs_resize = self
                .empty_state_size
                .map(|prev| (prev.0 - want.0).abs() > 1.0 || (prev.1 - want.1).abs() > 1.0)
                .unwrap_or(true);
            if needs_resize {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(want.0, want.1)));
                self.empty_state_size = Some(want);
                // The next image needs a real window again, not one derived
                // from the card's height.
                self.restore_window_size = true;
                self.last_resized_path = None;
                // Cover the OS-side resize with an opaque panel for a few
                // frames so the desktop doesn't flash through the rounded
                // corners.
                self.resize_settle_frames = 12;
                ctx.request_repaint();
            }
        }
        if !empty_state {
            self.empty_state_size = None;
        }
        if !self.is_fullscreen {
            self.maybe_resize_window_to_image(ctx);
        }
        // During an OS resize the window is transparent and the desktop shows
        // through the rounded corners; hold the panel opaque for a few frames
        // after an auto-resize to cover that window.
        let settling = self.resize_settle_frames > 0;
        if settling {
            self.resize_settle_frames -= 1;
            ctx.request_repaint();
        }
        // Custom window border resize: must run before the empty-state /
        // background interacts so it can claim the press when the cursor is
        // on an edge or corner.
        self.handle_window_resize(ctx);

        // Fullscreen is fully opaque. Windowed, the panel fill is transparent
        // in steady state for both image and empty modes (rounded image / card
        // is the visible surface); it turns opaque during a resize settle
        // window, when zoomed in (focused), and while loading / on error.
        let placeholder = !empty_state && !drawable;
        let bg_alpha: u8 = if self.is_fullscreen {
            255
        } else if settling || placeholder || (focused && !suppress_dim) {
            235
        } else {
            0
        };
        let panel_frame = egui::Frame::none()
            .fill(egui::Color32::from_rgba_unmultiplied(13, 13, 13, bg_alpha));
        egui::CentralPanel::default().frame(panel_frame).show(ctx, |ui| {
            self.render_image(ui, ctx);
            if !empty_state {
                // In empty state the central area is filled with the
                // start-screen card; a full-area background interact would
                // sit on top of the buttons and swallow their clicks, and
                // the window-drag-on-press is handled by the card's own
                // empty zones.
                self.render_overlay(ui);
                self.render_metadata_overlay(ui);
                self.handle_background_interaction(ui, ctx);
                self.render_close_button(ui);
                self.render_nav_chevrons(ui);
                self.handle_touch_swipe(ui);
            } else {
                self.render_close_button(ui);
            }
            self.render_toast(ui, empty_state);
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
        if actions.first { self.go_to_edge(false); }
        if actions.last { self.go_to_edge(true); }
        if actions.zoom_in { self.apply_zoom(1.25, None, ctx); }
        if actions.zoom_out { self.apply_zoom(0.8, None, ctx); }
        if actions.zoom_reset {
            self.target_zoom = 1.0;
            self.target_pan = egui::Vec2::ZERO;
            ctx.request_repaint();
        }
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
            let mut dialog = rfd::FileDialog::new();
            if let Some(f) = &self.folder {
                dialog = dialog.set_directory(f);
            }
            if let Some(d) = dialog.pick_folder() {
                self.open_folder(&d, None);
            }
        }
        if let Some(d) = actions.open_recent {
            if d.is_dir() {
                self.open_folder(&d, None);
            } else {
                // Folder is gone — drop it from recents and surface why.
                self.prefs.recent.retain(|p| p != &d);
                let _ = save_preferences(&self.prefs);
                self.toast(format!("Folder no longer exists: {}", d.display()), 2.5);
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
        if actions.show_in_explorer { self.show_current_in_file_manager(); }
        if actions.copy_to_clipboard { self.copy_current_to_clipboard(); }
        if actions.request_hq {
            // Global HQ mode toggle. Existing textures stay on the GPU so the
            // screen stays full while HQ decodes land in the background;
            // queue_preload picks the ones that need upgrading.
            let new_mode = !self.hq_mode.load(Ordering::Relaxed);
            self.hq_mode.store(new_mode, Ordering::Relaxed);
            self.queue_preload();
            self.evict_dirty = true;
            self.toast(if new_mode { "HQ mode" } else { "Snappy mode" }, 1.2);
        }

        // Toasts expire on wall-clock time. Counting down by frame dt lost
        // them: the first frame after an idle pause carries the whole pause
        // as its dt, so a toast set by a key press vanished before it was
        // ever painted.
        if let Some(t) = &self.toast {
            let now = Instant::now();
            if now >= t.until {
                self.toast = None;
            } else {
                let left = t.until - now;
                if left <= TOAST_FADE {
                    ctx.request_repaint();
                } else {
                    ctx.request_repaint_after(left - TOAST_FADE);
                }
            }
        }
    }
}

impl SnapView {
    /// Empty-state view: title, a "Choose folder…" primary button, hint about
    /// Ctrl+O / drag-drop, and a recent-folders list when we have any saved.
    fn render_empty_state(&mut self, ui: &mut egui::Ui) {
        let avail = ui.available_rect_before_wrap();
        let ctx = ui.ctx().clone();

        // The card IS the window. We paint a rounded rect covering the entire
        // panel, against the (transparent) panel fill — so the window appears
        // as just the rounded card on the desktop, with no dark frame around.
        let surface = avail;
        let painter = ui.painter();
        painter.rect_filled(
            surface,
            14.0,
            egui::Color32::from_rgba_unmultiplied(18, 18, 20, 245),
        );
        painter.rect_stroke(
            surface,
            14.0,
            egui::Stroke::new(
                1.0_f32,
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 22),
            ),
        );

        // Window drag handle. Sits on the whole surface, behind the widgets:
        // egui resolves overlapping interacts to the last-registered widget,
        // so buttons added later still win their clicks.
        let drag_resp = ui.interact(
            surface,
            egui::Id::new("empty_state_drag"),
            egui::Sense::click_and_drag(),
        );
        if drag_resp.drag_started_by(egui::PointerButton::Primary) {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        if drag_resp.double_clicked() {
            self.actions.toggle_max = true;
        }

        let inner = surface.shrink2(egui::vec2(26.0, 22.0));
        let mut col = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(inner)
                .layout(egui::Layout::top_down(egui::Align::Center)),
        );

        // ── Header ─────────────────────────────────────────────────────
        col.add_space(4.0);
        col.label(
            egui::RichText::new("snapview")
                .size(26.0)
                .strong()
                .color(egui::Color32::from_gray(240)),
        );
        col.add_space(1.0);
        col.label(
            egui::RichText::new("Fast, minimal photo viewer")
                .size(12.0)
                .color(egui::Color32::from_gray(135)),
        );
        col.add_space(16.0);

        // ── Drop zone with primary action ──────────────────────────────
        let drop_h = 96.0_f32;
        let drop_rect = egui::Rect::from_min_size(
            egui::pos2(inner.left(), col.cursor().top()),
            egui::vec2(inner.width(), drop_h),
        );
        let drop_resp = col.interact(
            drop_rect,
            egui::Id::new("empty_drop_zone"),
            egui::Sense::click(),
        );
        let hovered = drop_resp.hovered();
        {
            let dp = col.painter();
            dp.rect_filled(
                drop_rect,
                10.0,
                if hovered {
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 14)
                } else {
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 6)
                },
            );
            let border_col = if hovered {
                egui::Color32::from_rgba_unmultiplied(120, 160, 230, 200)
            } else {
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 50)
            };
            dp.rect_stroke(drop_rect, 10.0, egui::Stroke::new(1.0_f32, border_col));
            dp.text(
                drop_rect.center() - egui::vec2(0.0, 22.0),
                egui::Align2::CENTER_CENTER,
                "Drop a folder or image",
                egui::FontId::proportional(13.0),
                egui::Color32::from_gray(215),
            );
        }

        let btn_w = 190.0_f32;
        let btn_h = 30.0_f32;
        let btn_rect = egui::Rect::from_center_size(
            drop_rect.center() + egui::vec2(0.0, 14.0),
            egui::vec2(btn_w, btn_h),
        );
        let mut btn_ui = col.new_child(
            egui::UiBuilder::new()
                .max_rect(btn_rect)
                .layout(egui::Layout::centered_and_justified(
                    egui::Direction::TopDown,
                )),
        );
        let btn_resp = btn_ui.add(
            egui::Button::new(
                egui::RichText::new("Choose folder")
                    .size(13.0)
                    .color(egui::Color32::WHITE),
            )
            .fill(egui::Color32::from_rgb(48, 110, 215))
            .rounding(7.0)
            .min_size(egui::vec2(btn_w, btn_h)),
        );
        if btn_resp.clicked() || drop_resp.clicked() {
            self.actions.open_folder = true;
        }
        // "Ctrl+O" hint anchored to the bottom-right corner of the drop zone.
        col.painter().text(
            egui::pos2(drop_rect.right() - 10.0, drop_rect.bottom() - 10.0),
            egui::Align2::RIGHT_BOTTOM,
            "Ctrl+O",
            egui::FontId::monospace(10.5),
            egui::Color32::from_gray(120),
        );

        col.advance_cursor_after_rect(drop_rect);
        col.add_space(18.0);

        // ── Recent folders ─────────────────────────────────────────────
        let recents = self.prefs.recent.clone();
        let recent_rows = recents.len().min(5);
        if recent_rows > 0 {
            col.with_layout(egui::Layout::left_to_right(egui::Align::TOP), |row| {
                row.add_space(2.0);
                row.label(
                    egui::RichText::new("RECENT")
                        .size(10.5)
                        .color(egui::Color32::from_gray(135))
                        .strong(),
                );
            });
            col.add_space(4.0);
            for p in recents.iter().take(recent_rows) {
                let label = recent_label(p);
                let full = p.display().to_string();
                let row_h = 24.0_f32;
                let row_rect = egui::Rect::from_min_size(
                    egui::pos2(inner.left(), col.cursor().top()),
                    egui::vec2(inner.width(), row_h),
                );
                // Peek hover state before laying out so we can paint a
                // background underneath the button.
                let pre_hovered = col.rect_contains_pointer(row_rect);
                if pre_hovered {
                    col.painter().rect_filled(
                        row_rect.expand2(egui::vec2(4.0, 1.0)),
                        6.0,
                        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 20),
                    );
                }
                let text_color = if pre_hovered {
                    egui::Color32::from_gray(245)
                } else {
                    egui::Color32::from_gray(220)
                };
                let row_resp = col.add(
                    egui::Button::new(
                        egui::RichText::new(label).size(13.0).color(text_color),
                    )
                    .frame(false)
                    .min_size(egui::vec2(inner.width(), row_h)),
                );
                if row_resp.clicked() {
                    self.actions.open_recent = Some(p.clone());
                }
                row_resp
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(full);
            }
        }

        // ── Bottom hint strip pinned to the card edge ──────────────────
        let hp = ui.painter();
        let dim = egui::Color32::from_gray(115);
        let key = egui::Color32::from_gray(185);
        let font = egui::FontId::proportional(11.0);
        let hints: [(&str, &str); 3] = [("F1", "help"), ("Ctrl+O", "open"), ("Esc", "quit")];
        let gap = 4.0_f32;
        let sep = 14.0_f32;
        let mut widths: Vec<(f32, f32)> = Vec::with_capacity(hints.len());
        let mut total = 0.0_f32;
        for (k, t) in &hints {
            let wk = hp.layout_no_wrap(k.to_string(), font.clone(), key).rect.width();
            let wt = hp.layout_no_wrap(t.to_string(), font.clone(), dim).rect.width();
            widths.push((wk, wt));
            total += wk + gap + wt;
        }
        total += sep * (hints.len() - 1) as f32;
        let y = surface.bottom() - 14.0;
        let mut x = surface.center().x - total / 2.0;
        for ((k, t), (wk, wt)) in hints.iter().zip(widths.iter()) {
            hp.text(egui::pos2(x, y), egui::Align2::LEFT_CENTER, *k, font.clone(), key);
            x += wk + gap;
            hp.text(egui::pos2(x, y), egui::Align2::LEFT_CENTER, *t, font.clone(), dim);
            x += wt + sep;
        }
    }

    /// Which image's texture to paint while `path` is current. Preference:
    ///   1. `path` has a full texture.
    ///   2. the previously displayed image has a full texture — keep it, so
    ///      we never downgrade from a sharp image to a blurry thumb while the
    ///      user waits for the next full / HQ decode.
    ///   3. `path` has an EXIF thumbnail.
    ///   4. the previously displayed image has anything.
    ///
    /// A failed decode never falls back to the previous image: that would
    /// show the wrong photo under the new file name.
    fn pick_draw_path(&self, path: &Path) -> Option<PathBuf> {
        if self.textures.contains_key(path) {
            return Some(path.to_path_buf());
        }
        let own_thumb = self.thumb_textures.contains_key(path);
        if self.failed.contains(path) {
            return own_thumb.then(|| path.to_path_buf());
        }
        let prev = self.displayed_path.as_ref().filter(|p| p.as_path() != path);
        if let Some(prev) = prev.filter(|p| self.textures.contains_key(*p)) {
            return Some(prev.clone());
        }
        if own_thumb {
            return Some(path.to_path_buf());
        }
        prev.filter(|p| self.thumb_textures.contains_key(*p)).cloned()
    }

    fn render_image(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let avail_rect = ui.max_rect();
        let avail = avail_rect.size();
        let path = match self.current_path() {
            Some(p) => p,
            None => {
                self.last_image_rect = None;
                self.render_empty_state(ui);
                self.handle_drop(ctx);
                return;
            }
        };

        let Some(draw_path) = self.pick_draw_path(&path) else {
            // Nothing to paint yet, but keep last_image_rect in sync with the
            // image's known aspect so overlays/chevrons sit in the right
            // place during the brief decode window.
            self.last_image_rect = self.display_dims(&path).map(|(w, h)| {
                let (w, h) = (w as f32, h as f32);
                let scale = (avail.x / w).min(avail.y / h).min(1.0).max(0.0001) * self.zoom;
                egui::Rect::from_center_size(avail_rect.center() + self.pan, egui::vec2(w, h) * scale)
            });
            let painter = ui.painter();
            if self.failed.contains(&path) {
                painter.text(
                    avail_rect.center() - egui::vec2(0.0, 12.0),
                    egui::Align2::CENTER_CENTER,
                    "Can't open this image",
                    egui::FontId::proportional(16.0),
                    egui::Color32::from_gray(200),
                );
                painter.text(
                    avail_rect.center() + egui::vec2(0.0, 14.0),
                    egui::Align2::CENTER_CENTER,
                    file_label(&path),
                    egui::FontId::proportional(12.5),
                    egui::Color32::from_gray(140),
                );
            } else {
                painter.text(
                    avail_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "…",
                    egui::FontId::proportional(40.0),
                    egui::Color32::from_gray(80),
                );
            }
            self.handle_drop(ctx);
            return;
        };
        if draw_path == path {
            self.displayed_path = Some(path.clone());
        }

        let user_quarter = *self.rotation.get(&draw_path).unwrap_or(&0);
        let exif_q = *self.exif_quarter.get(&draw_path).unwrap_or(&0);
        let rotation_quarter = (user_quarter + exif_q).rem_euclid(4);

        let (tex_id, tex_size, opaque) = match self.textures.get(&draw_path) {
            Some(t) => (t.handle.id(), t.handle.size_vec2(), t.opaque),
            None => match self.thumb_textures.get(&draw_path) {
                Some(t) => (t.id(), t.size_vec2(), true),
                None => return,
            },
        };
        // Size the quad from the source dimensions, not the texture's: the
        // texture may be an EXIF thumbnail or a downscaled decode, and the
        // picture shouldn't change size when the sharper version lands.
        let src = self
            .full_dims
            .get(&draw_path)
            .map(|d| egui::vec2(d[0] as f32, d[1] as f32))
            .unwrap_or(tex_size);
        let (fit_w, fit_h) = if rotation_quarter % 2 == 0 {
            (src.x, src.y)
        } else {
            (src.y, src.x)
        };
        let base_scale = (avail.x / fit_w).min(avail.y / fit_h).min(1.0).max(0.0001);

        // Keep the picture on screen: zoomed in, it can't be dragged past its
        // own edges; smaller than the window, it stays centered.
        let pan_limit = |zoom: f32| {
            ((egui::vec2(fit_w, fit_h) * base_scale * zoom - avail) * 0.5).max(egui::Vec2::ZERO)
        };
        let lim = pan_limit(self.target_zoom);
        self.target_pan = self.target_pan.clamp(-lim, lim);
        let lim = pan_limit(self.zoom);
        self.pan = self.pan.clamp(-lim, lim);

        let scale = base_scale * self.zoom;
        let draw_w = src.x * scale;
        let draw_h = src.y * scale;

        let center = avail_rect.center() + self.pan;
        let angle = rotation_quarter as f32 * std::f32::consts::FRAC_PI_2;
        // macOS-style continuous-ish corner rounding when windowed. In
        // fullscreen the image bleeds to the display edges (no panel
        // around it to round against).
        let corner_radius = if self.is_fullscreen { 0.0 } else { 12.0 };
        let shown = egui::Rect::from_center_size(center, egui::vec2(fit_w * scale, fit_h * scale));
        if !opaque {
            self.paint_checkerboard(ui, shown, corner_radius);
        }

        let mut mesh = egui::Mesh::with_texture(tex_id);
        let local_rect =
            egui::Rect::from_center_size(egui::Pos2::ZERO, egui::vec2(draw_w, draw_h));
        let uv_rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
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

        self.last_image_rect = Some(shown);
        self.handle_drop(ctx);
    }

    /// Tile a checkerboard behind an image with transparency, so its clear
    /// parts read as transparent instead of showing the desktop through the
    /// (transparent) window.
    fn paint_checkerboard(&mut self, ui: &egui::Ui, rect: egui::Rect, radius: f32) {
        const CELL: f32 = 8.0;
        let tex = self.checker.get_or_insert_with(|| {
            let (a, b) = (egui::Color32::from_gray(62), egui::Color32::from_gray(44));
            ui.ctx().load_texture(
                "checker",
                egui::ColorImage { size: [2, 2], pixels: vec![a, b, b, a] },
                egui::TextureOptions {
                    magnification: egui::TextureFilter::Nearest,
                    minification: egui::TextureFilter::Nearest,
                    wrap_mode: egui::TextureWrapMode::Repeat,
                    mipmap_mode: None,
                },
            )
        });
        // One texel per cell; the 2x2 texture repeats every two cells.
        let uv = egui::Rect::from_min_max(
            egui::Pos2::ZERO,
            egui::pos2(rect.width() / (2.0 * CELL), rect.height() / (2.0 * CELL)),
        );
        let mut mesh = egui::Mesh::with_texture(tex.id());
        add_rounded_rect_with_uv(&mut mesh, rect, uv, radius, egui::Color32::WHITE);
        ui.painter().add(egui::Shape::mesh(mesh));
    }

    /// Where overlays anchor: the whole screen in fullscreen, otherwise the
    /// part of the image that's actually inside the window (when zoomed in,
    /// the image rect extends past it).
    fn overlay_rect(&self, ui: &egui::Ui) -> egui::Rect {
        let screen = ui.max_rect();
        if self.is_fullscreen {
            return screen;
        }
        match self.last_image_rect {
            Some(r) => r.intersect(screen),
            None => screen,
        }
    }

    fn render_overlay(&self, ui: &mut egui::Ui) {
        if self.images.is_empty() { return; }
        let path = match self.current_path() {
            Some(p) => p,
            None => return,
        };
        let is_fav = self.favorites.contains(&path);
        let name = file_label(&path);
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

        let painter = ui.painter();
        let rect = self.overlay_rect(ui);
        let font = egui::FontId::proportional(13.0);
        let text_color = egui::Color32::from_gray(232);
        let star = if is_fav { "★" } else { "☆" };
        let width = |s: &str| painter.layout_no_wrap(s.to_owned(), font.clone(), text_color).size().x;

        // Both labels on one line without overlapping, even in a narrow
        // portrait window: drop the favorites count first, then shorten the
        // file name from the middle.
        let room = rect.width() - 14.0 * 2.0 - 18.0;
        let mut fav_text = format!("{}  ({} favs)", star, self.favorites.len());
        let mut left = format!("{}   ·   {}", name, counter);
        if width(&left) + width(&fav_text) > room {
            fav_text = star.to_string();
        }
        if width(&left) + width(&fav_text) > room {
            let tail = format!("   ·   {}", counter);
            let budget = room - width(&fav_text) - width(&tail);
            left = format!("{}{}", elide_middle(&name, budget, &width), tail);
        }

        shadowed_text(
            painter,
            egui::pos2(rect.left() + 14.0, rect.bottom() - 14.0),
            egui::Align2::LEFT_BOTTOM,
            &left,
            font.clone(),
            text_color,
        );
        shadowed_text(
            painter,
            egui::pos2(rect.right() - 14.0, rect.bottom() - 14.0),
            egui::Align2::RIGHT_BOTTOM,
            &fav_text,
            font.clone(),
            if is_fav { egui::Color32::from_rgb(255, 200, 60) } else { text_color },
        );

        if self.failed.contains(&path) {
            // Only the embedded thumbnail could be shown.
            shadowed_text(
                painter,
                egui::pos2(rect.center().x, rect.bottom() - 40.0),
                egui::Align2::CENTER_BOTTOM,
                "Preview only — the full image can't be decoded",
                font,
                egui::Color32::from_rgb(255, 190, 120),
            );
        }
    }

    /// Transient message (or copy/cut progress) in a dark pill: top-center of
    /// the image, or near the bottom of the start-screen card.
    fn render_toast(&self, ui: &mut egui::Ui, empty_state: bool) {
        let (text, alpha) = if let Some(job) = &self.file_job {
            let verb = if job.cut { "Moving" } else { "Copying" };
            (format!("{}…  {} / {}", verb, job.done, job.total), 1.0)
        } else if let Some(t) = &self.toast {
            let left = t.until.saturating_duration_since(Instant::now());
            (t.text.clone(), (left.as_secs_f32() / TOAST_FADE.as_secs_f32()).min(1.0))
        } else {
            return;
        };
        if alpha <= 0.0 { return; }
        let rect = if empty_state { ui.max_rect() } else { self.overlay_rect(ui) };
        let painter = ui.painter();
        let galley = painter.layout(
            text,
            egui::FontId::proportional(14.5),
            egui::Color32::WHITE.gamma_multiply(alpha),
            (rect.width() - 48.0).max(120.0),
        );
        let pad = egui::vec2(14.0, 8.0);
        let size = galley.size() + pad * 2.0;
        let top = if empty_state {
            rect.bottom() - 34.0 - size.y
        } else {
            rect.top() + 18.0
        };
        let pill = egui::Rect::from_min_size(egui::pos2(rect.center().x - size.x / 2.0, top), size);
        painter.rect_filled(pill, 8.0, egui::Color32::from_black_alpha((200.0 * alpha) as u8));
        painter.galley(pill.min + pad, galley, egui::Color32::WHITE);
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
        // The decoder-reported size, the way the image is displayed. EXIF's
        // own size tags are only a fallback: they're missing from most
        // non-JPEG files, unrotated, and often stale after an edit.
        let dimensions = self
            .full_dims
            .get(&path)
            .map(|&[w, h]| {
                let q = self.exif_quarter.get(&path).copied().unwrap_or(0);
                let (w, h) = if q.rem_euclid(2) == 1 { (h, w) } else { (w, h) };
                format!("{} × {}", w, h)
            })
            .or_else(|| md.dimensions.clone());
        let mut lines: Vec<String> = Vec::new();
        if let Some(s) = &md.date_taken { lines.push(s.clone()); }
        if let Some(s) = &md.camera { lines.push(s.clone()); }
        if let Some(s) = &md.lens { lines.push(s.clone()); }
        let exposure: Vec<&str> = [&md.shutter, &md.aperture, &md.iso, &md.focal]
            .iter()
            .filter_map(|o| o.as_deref())
            .collect();
        if !exposure.is_empty() { lines.push(exposure.join("   ·   ")); }
        let dims_size: Vec<&str> = [&dimensions, &md.file_size]
            .iter()
            .filter_map(|o| o.as_deref())
            .collect();
        if !dims_size.is_empty() { lines.push(dims_size.join("   ·   ")); }
        if lines.is_empty() { return; }

        let rect = self.overlay_rect(ui);
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
        let rect = self.overlay_rect(ui);
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
        let stroke = egui::Stroke::new(2.5_f32, color);
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
        let rect = self.overlay_rect(ui);
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
            ui.max_rect(),
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

        let recents = self.prefs.recent.clone();
        resp.context_menu(|ui| {
            if ui.button("Open folder…  (Ctrl+O)").clicked() {
                self.actions.open_folder = true;
                ui.close_menu();
            }
            if !recents.is_empty() {
                ui.menu_button("Recent folders", |ui| {
                    for p in &recents {
                        if ui
                            .button(recent_label(p))
                            .on_hover_text(p.display().to_string())
                            .clicked()
                        {
                            self.actions.open_recent = Some(p.clone());
                            ui.close_menu();
                        }
                    }
                });
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
            if ui.button("Copy image to clipboard  (Ctrl+C)").clicked() { self.actions.copy_to_clipboard = true; ui.close_menu(); }
            if ui.button("Show in file manager  (Ctrl+E)").clicked() { self.actions.show_in_explorer = true; ui.close_menu(); }
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
                } else if is_listable_image(&path) {
                    self.open_folder_with_file(&path);
                    return;
                }
            }
        }
        self.toast("Not an image or folder snapview can open", 2.0);
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
                    format!("Move this image and {} RAW/sidecar file{} to trash?", side_count, plural(side_count))
                } else {
                    "Move this image to trash?".to_string()
                };
                ui.label(egui::RichText::new(header).size(15.0));
                ui.add_space(6.0);
                let weak = ui.visuals().weak_text_color();
                ui.label(egui::RichText::new(&name).color(ui.visuals().text_color()).italics());
                for sn in &side_names {
                    ui.label(egui::RichText::new(format!("+ {}", sn)).color(weak).italics().size(12.0));
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(
                            egui::RichText::new("Delete  (Enter)").color(egui::Color32::WHITE),
                        )
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
            ("Next / previous image", "Right  /  Left"),
            ("First / last image", "Home  /  End"),
            ("Zoom in / out", "scroll  /  +  -"),
            ("Reset zoom", "0"),
            ("HQ mode (native resolution)", "Z"),
            ("Show / hide image info", "I"),
            ("Open folder…", "Ctrl + O"),
            ("Copy image to clipboard", "Ctrl + C"),
            ("Show in file manager", "Ctrl + E"),
        ];
        let pairs_right: &[(&str, &str)] = &[
            ("Mark / unmark favorite", "Space"),
            ("Cycle filter (all / favs / non-favs)", "F"),
            ("Filter favorites window…", "Shift + F"),
            ("Move to trash", "Delete"),
            ("Rotate left / right", "Q  /  W"),
            ("Toggle fullscreen", "F11  /  Alt + Enter"),
            ("Show / hide this help", "F1"),
            ("Close dialog / leave fullscreen / quit", "Esc"),
        ];

        egui::Window::new("Keyboard shortcuts")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .order(egui::Order::Tooltip)
            .show(ctx, |ui| {
                ui.set_min_width(560.0);
                ui.add_space(4.0);
                let desc_color = ui.visuals().text_color();
                let key_color = if ui.visuals().dark_mode {
                    egui::Color32::from_rgb(180, 200, 255)
                } else {
                    egui::Color32::from_rgb(30, 80, 180)
                };
                egui::Grid::new("help_grid")
                    .num_columns(4)
                    .spacing([24.0, 6.0])
                    .show(ui, |ui| {
                        let rows = pairs_left.len().max(pairs_right.len());
                        for i in 0..rows {
                            for pairs in [pairs_left, pairs_right] {
                                if let Some((desc, key)) = pairs.get(i) {
                                    ui.label(egui::RichText::new(*desc).color(desc_color));
                                    ui.label(egui::RichText::new(*key).strong().color(key_color));
                                } else {
                                    ui.label("");
                                    ui.label("");
                                }
                            }
                            ui.end_row();
                        }
                    });
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new("Mouse:").strong());
                    ui.label("drag = move window (pan when zoomed) · double-click = fullscreen · right-click = menu · back / forward buttons = previous / next image");
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new("Touch:").strong());
                    ui.label("swipe horizontally to flick between images, pinch to zoom");
                });
                ui.add_space(6.0);
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new("Press F1 or Esc to dismiss")
                        .color(ui.visuals().weak_text_color()).italics());
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
                    ui.radio_value(&mut t, ThemeMode::Auto, "Follow system");
                    ui.radio_value(&mut t, ThemeMode::Light, "Light");
                    ui.radio_value(&mut t, ThemeMode::Dark, "Dark");
                });
                if t != self.prefs.theme {
                    theme_changed = Some(t);
                }

                ui.add_space(14.0);
                ui.separator();
                ui.add_space(10.0);

                let weak = ui.visuals().weak_text_color();
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new("snapview").size(20.0).strong());
                    ui.label(
                        egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                            .color(weak),
                    );
                    ui.add_space(6.0);
                    ui.label("Fast, minimal image viewer");
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new("by Filip Kozina").color(weak));
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
        // Esc is handled with the other keys in update().
        if close {
            self.show_prefs = false;
        }
    }

    fn render_filter_window(&mut self, ctx: &egui::Context) {
        let mut open = true;
        let mut do_copy = false;
        let mut do_cut = false;
        let mut do_select_all = false;
        let mut do_select_none = false;
        let busy = self.file_job.is_some();

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
                    favs.sort_by(|a, b| compare_paths(a, b));
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
                    let enabled = !busy && !self.filter_selected.is_empty();
                    if ui.add_enabled(
                        enabled,
                        egui::Button::new(format!(
                            "Copy {} selected to folder…",
                            self.filter_selected.len()
                        )),
                    ).clicked() {
                        do_copy = true;
                    }
                    if ui.add_enabled(
                        enabled,
                        egui::Button::new(format!(
                            "Cut {} selected to folder…",
                            self.filter_selected.len()
                        )),
                    ).clicked() {
                        do_cut = true;
                    }
                });
                if let Some(job) = &self.file_job {
                    ui.label(format!(
                        "{}…  {} / {}",
                        if job.cut { "Moving" } else { "Copying" },
                        job.done,
                        job.total
                    ));
                }
                ui.label(
                    egui::RichText::new("RAW sidecars go along. Files already in the destination are skipped, never overwritten.")
                        .size(11.5)
                        .color(ui.visuals().weak_text_color()),
                );
            });

        if do_select_all { self.filter_selected = self.favorites.clone(); }
        if do_select_none { self.filter_selected.clear(); }
        if do_copy || do_cut {
            let mut dialog = rfd::FileDialog::new().set_title(if do_cut {
                "Move selected images to…"
            } else {
                "Copy selected images to…"
            });
            if let Some(start) = self.folder.as_deref().and_then(Path::parent) {
                dialog = dialog.set_directory(start);
            }
            if let Some(dest) = dialog.pick_folder() {
                self.start_file_job(dest, do_cut);
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
    let stroke = egui::Stroke::new(3.0_f32, color);
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

/// Text with a soft drop shadow, so labels stay readable over bright photos.
fn shadowed_text(
    painter: &egui::Painter,
    pos: egui::Pos2,
    align: egui::Align2,
    text: &str,
    font: egui::FontId,
    color: egui::Color32,
) {
    painter.text(pos + egui::vec2(1.0, 1.0), align, text, font.clone(), egui::Color32::from_black_alpha(160));
    painter.text(pos, align, text, font, color);
}

/// Shorten `s` from the middle ("IMG_2024…0412.jpg") until `width` says it
/// fits in `max_w`.
fn elide_middle(s: &str, max_w: f32, width: &dyn Fn(&str) -> f32) -> String {
    if width(s) <= max_w {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let shortened = |keep: usize| -> String {
        let head = keep.div_ceil(2);
        let mut out: String = chars[..head].iter().collect();
        out.push('…');
        out.extend(&chars[chars.len() - keep / 2..]);
        out
    };
    // Largest number of kept characters that still fits.
    let (mut lo, mut hi) = (0usize, chars.len().saturating_sub(1));
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if width(&shortened(mid)) <= max_w { lo = mid; } else { hi = mid - 1; }
    }
    shortened(lo)
}

fn file_label(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// 1 for portrait (narrower than ~square), 0 for landscape / square.
fn aspect_class(w: usize, h: usize) -> i8 {
    if (w as f32) < (h as f32) * 0.95 { 1 } else { 0 }
}

/// Window size that shows an image of `aspect` at `height`, scaled down to
/// fit 95% of the monitor when we know its size (panoramas, tall scans).
fn fit_window_to_aspect(aspect: f32, height: f32, monitor: Option<egui::Vec2>) -> (f32, f32) {
    let mut h = height.max(150.0);
    let mut w = h * aspect;
    if let Some(m) = monitor.filter(|m| m.x > 0.0 && m.y > 0.0) {
        let (max_w, max_h) = (m.x * 0.95, m.y * 0.95);
        if h > max_h {
            h = max_h;
            w = h * aspect;
        }
        if w > max_w {
            w = max_w;
            h = w / aspect;
        }
    }
    (w.max(200.0), h.max(120.0))
}

/// Window dimensions for the empty-state card. Sized just large enough to
/// fit the header, drop zone, up-to-5 recent rows and the bottom hint strip
/// — anything more and the user sees a tall empty rectangle.
fn empty_state_card_size(recent_count: usize, show_help: bool) -> (f32, f32) {
    let n = recent_count.min(5);
    let header = 4.0 + 26.0 + 1.0 + 16.0 + 16.0;
    let drop_zone = 96.0 + 18.0;
    let recents = if n == 0 { 0.0 } else { 14.0 + 4.0 + n as f32 * 24.0 + 8.0 };
    let hint_strip = 24.0;
    let padding = 22.0 * 2.0;
    let h = header + drop_zone + recents + hint_strip + padding;
    if show_help {
        // The F1 help is a centered egui Window with min_width 560 plus its
        // own padding, two columns of shortcuts and a few footer lines. The
        // start-screen card is much smaller than that — expand the OS window
        // so the help dialog isn't clipped.
        (640.0_f32.max(440.0), 460.0_f32.max(h))
    } else {
        (440.0, h)
    }
}

/// Compact one-line label for a recent-folder entry: the last two path
/// segments ("Photos / 2024-08") so similarly-named folders are
/// distinguishable without showing the whole absolute path.
fn recent_label(p: &Path) -> String {
    let segs: Vec<String> = p
        .components()
        .rev()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().to_string()),
            _ => None,
        })
        .take(2)
        .collect();
    if segs.is_empty() {
        p.display().to_string()
    } else if segs.len() == 1 {
        segs.into_iter().next().unwrap()
    } else {
        let mut it = segs.into_iter();
        let last = it.next().unwrap();
        let parent = it.next().unwrap();
        format!("{} / {}", parent, last)
    }
}

/// List the images in `folder` in Explorer order, plus the RAW sidecars
/// that share a stem with each of them.
fn scan_folder(folder: &Path) -> (Vec<PathBuf>, HashMap<PathBuf, Vec<PathBuf>>) {
    let mut images: Vec<PathBuf> = Vec::new();
    let mut raws: HashMap<String, Vec<PathBuf>> = HashMap::new();
    if let Ok(rd) = std::fs::read_dir(folder) {
        for entry in rd.flatten() {
            let path = entry.path();
            // The entry's type comes with the directory listing; only
            // symlinks need an extra stat to see what they point at.
            let is_file = match entry.file_type() {
                Ok(t) if t.is_symlink() => path.is_file(),
                Ok(t) => t.is_file(),
                Err(_) => false,
            };
            if !is_file {
                continue;
            }
            if is_raw_sidecar(&path) {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    raws.entry(stem.to_lowercase()).or_default().push(path);
                }
            } else if is_listable_name(&path) && is_image(&path) {
                images.push(path);
            }
        }
    }
    images.sort_by(|a, b| compare_paths(a, b));
    let mut sidecars: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for img in &images {
        if let Some(stem) = img.file_stem().and_then(|s| s.to_str()) {
            if let Some(list) = raws.get(&stem.to_lowercase()) {
                sidecars.insert(img.clone(), list.clone());
            }
        }
    }
    (images, sidecars)
}

/// Explorer-like order for file names: case-insensitive, with runs of
/// digits compared by value ("IMG_2" before "IMG_10"). Ties fall back to a
/// plain path comparison so the order is total, which the binary searches
/// over `SnapView::images` rely on.
fn compare_paths(a: &Path, b: &Path) -> CmpOrdering {
    let an = a.file_name().map(|s| s.to_string_lossy()).unwrap_or_default();
    let bn = b.file_name().map(|s| s.to_string_lossy()).unwrap_or_default();
    natural_cmp(&an, &bn).then_with(|| a.cmp(b))
}

fn natural_cmp(a: &str, b: &str) -> CmpOrdering {
    fn digits(it: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
        let mut s = String::new();
        while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
            s.push(c);
            it.next();
        }
        s
    }
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return CmpOrdering::Equal,
            (None, Some(_)) => return CmpOrdering::Less,
            (Some(_), None) => return CmpOrdering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let (da, db) = (digits(&mut ai), digits(&mut bi));
                let (ta, tb) = (da.trim_start_matches('0'), db.trim_start_matches('0'));
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != CmpOrdering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                let ord = x.to_lowercase().cmp(y.to_lowercase());
                if ord != CmpOrdering::Equal {
                    return ord;
                }
                ai.next();
                bi.next();
            }
        }
    }
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

/// macOS leaves "._name.jpg" AppleDouble files next to every file it
/// touches on FAT/exFAT cards and sticks. They carry the image's extension
/// but no image.
fn is_listable_name(p: &Path) -> bool {
    !p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("._"))
}

/// A file we'd list: exists, isn't an AppleDouble companion, and is an
/// image by extension (or by magic bytes when it has no extension).
fn is_listable_image(p: &Path) -> bool {
    p.is_file() && is_listable_name(p) && is_image(p)
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

/// A full decode, ready for upload.
struct Decoded {
    image: egui::ColorImage,
    /// Display rotation in quarter turns: EXIF 3/6/8 are drawn rotated
    /// instead of being rotated in pixels.
    quarter: i32,
    /// Source dimensions, in the same pre-rotation space as `image`.
    src_dims: [usize; 2],
    /// No pixel has alpha below 255.
    opaque: bool,
}

/// What a header probe learns without decoding the image itself.
#[derive(Default)]
struct Probe {
    /// The embedded EXIF thumbnail, when the file has one.
    thumb: Option<egui::ColorImage>,
    dims: Option<[usize; 2]>,
    quarter: i32,
}

impl Probe {
    /// Width / height as displayed.
    fn display_aspect(&self) -> Option<f32> {
        let [w, h] = self.dims?;
        if w == 0 || h == 0 {
            return None;
        }
        let (w, h) = if self.quarter.rem_euclid(2) == 1 { (h, w) } else { (w, h) };
        Some(w as f32 / h as f32)
    }
}

/// Decoder output before orientation, downscaling and color conversion.
struct RawDecode {
    img: image::DynamicImage,
    /// EXIF orientation, 1-8.
    orientation: u32,
    icc: Option<Vec<u8>>,
    src_dims: [usize; 2],
}

/// Decode `path` for display: downscaled to the `target` size class (and
/// within the GPU's `max_side`), color-managed to sRGB, premultiplied.
fn decode_full(path: &Path, target: u32, max_side: u32) -> Option<Decoded> {
    // One read of the whole file: the decoders consume all of it anyway,
    // and a single large read beats thousands of small ones, especially on
    // network drives.
    let data = std::fs::read(path).ok()?;
    let RawDecode { img, orientation, icc, src_dims } = decode_any(&data, target)?;
    let (img, quarter) = apply_exif_orientation_lazy(img, orientation);
    // Mirrored orientations are baked into the pixels; 5 and 7 also swap axes.
    let src_dims = if matches!(orientation, 5 | 7) { [src_dims[1], src_dims[0]] } else { src_dims };
    let (image, opaque) = to_color_image(img, icc.as_deref(), target, max_side);
    Some(Decoded { image, quarter, src_dims, opaque })
}

fn decode_any(data: &[u8], target: u32) -> Option<RawDecode> {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        // jpeg-decoder scales in the DCT domain (1/2, 1/4, 1/8), far cheaper
        // than decoding at full size and resizing. The image crate handles
        // what it can't (CMYK, 12-bit, arithmetic coding).
        decode_jpeg_data(data, target).or_else(|| decode_generic(data))
    } else {
        decode_generic(data)
    }
}

fn decode_jpeg_data(data: &[u8], target: u32) -> Option<RawDecode> {
    let mut decoder = jpeg_decoder::Decoder::new(data);
    decoder.read_info().ok()?;
    let full = decoder.info()?;
    // Must be called before decode so the IDCT runs at the smaller scale
    // natively: the smallest of 1/8, 1/4, 1/2, 1 that keeps the long side
    // at or above the target.
    let t = target.clamp(1, u16::MAX as u32) as u16;
    decoder.scale(t, t).ok()?;
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let (w, h) = (info.width as u32, info.height as u32);
    let img = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => {
            image::DynamicImage::ImageRgb8(image::RgbImage::from_raw(w, h, pixels)?)
        }
        jpeg_decoder::PixelFormat::L8 => {
            image::DynamicImage::ImageLuma8(image::GrayImage::from_raw(w, h, pixels)?)
        }
        _ => return None,
    };
    // The decoder already parsed the EXIF segment; no second pass over the file.
    let orientation = decoder.exif_data().and_then(exif_orientation_from_tiff).unwrap_or(1);
    Some(RawDecode {
        img,
        orientation,
        icc: decoder.icc_profile(),
        src_dims: [full.width as usize, full.height as usize],
    })
}

fn decode_generic(data: &[u8]) -> Option<RawDecode> {
    use image::ImageDecoder;
    let reader = image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let mut decoder = reader.into_decoder().ok()?;
    let icc = decoder.icc_profile().ok().flatten();
    let orientation = decoder.orientation().map(orientation_to_exif).unwrap_or(1);
    let (w, h) = decoder.dimensions();
    let img = image::DynamicImage::from_decoder(decoder).ok()?;
    Some(RawDecode { img, orientation, icc, src_dims: [w as usize, h as usize] })
}

/// Downscale into the target's size class and convert to egui's
/// premultiplied sRGBA, applying the embedded color profile on the way.
/// Returns the image and whether it is fully opaque.
fn to_color_image(
    img: image::DynamicImage,
    icc: Option<&[u8]>,
    target: u32,
    max_side: u32,
) -> (egui::ColorImage, bool) {
    let (w, h) = (img.width() as usize, img.height() as usize);
    if img.color().has_alpha() {
        let mut rgba = img.into_rgba8().into_raw();
        if let Some(icc) = icc {
            apply_icc_to_srgb(&mut rgba, icc, 4);
        }
        // Premultiply before filtering, so the arbitrary colors stored in
        // fully transparent pixels can't bleed into the edges.
        let opaque = premultiply_alpha(&mut rgba);
        let (rgba, w, h) = downscale_pow2(rgba, w, h, 4, target, max_side);
        (color_image_from_premultiplied(w, h, &rgba), opaque)
    } else if matches!(img.color(), image::ColorType::L8 | image::ColorType::L16) {
        let (gray, w, h) = downscale_pow2(img.into_luma8().into_raw(), w, h, 1, target, max_side);
        let pixels = gray.par_iter().map(|&v| egui::Color32::from_gray(v)).collect();
        (egui::ColorImage { size: [w, h], pixels }, true)
    } else {
        let (mut rgb, w, h) = downscale_pow2(img.into_rgb8().into_raw(), w, h, 3, target, max_side);
        // After the downscale: the transform is per pixel, so fewer pixels
        // means less work.
        if let Some(icc) = icc {
            apply_icc_to_srgb(&mut rgb, icc, 3);
        }
        let pixels = rgb
            .par_chunks_exact(3)
            .map(|p| egui::Color32::from_rgb(p[0], p[1], p[2]))
            .collect();
        (egui::ColorImage { size: [w, h], pixels }, true)
    }
}

fn color_image_from_premultiplied(w: usize, h: usize, rgba: &[u8]) -> egui::ColorImage {
    let pixels = rgba
        .par_chunks_exact(4)
        .map(|p| egui::Color32::from_rgba_premultiplied(p[0], p[1], p[2], p[3]))
        .collect();
    egui::ColorImage { size: [w, h], pixels }
}

/// Straight alpha → egui's premultiplied alpha, in place. Returns whether
/// every pixel was fully opaque.
fn premultiply_alpha(rgba: &mut [u8]) -> bool {
    rgba.par_chunks_mut(4 * 4096)
        .map(|chunk| {
            let mut opaque = true;
            for px in chunk.chunks_exact_mut(4) {
                if px[3] != 255 {
                    opaque = false;
                    let c = egui::Color32::from_rgba_unmultiplied(px[0], px[1], px[2], px[3]);
                    px.copy_from_slice(&c.to_array());
                }
            }
            opaque
        })
        .reduce(|| true, |a, b| a && b)
}

/// Halve with a 2x2 box filter while the long side stays at or above
/// `target` afterwards, and while it exceeds `max_side` (the GPU limit).
/// Lands in the same [target, 2*target) size class that JPEG DCT scaling
/// produces, so every format decodes to comparable sizes — and far faster
/// than a general-purpose resize.
fn downscale_pow2(
    mut buf: Vec<u8>,
    mut w: usize,
    mut h: usize,
    channels: usize,
    target: u32,
    max_side: u32,
) -> (Vec<u8>, usize, usize) {
    let target = target.max(1) as usize;
    let max_side = max_side.max(1) as usize;
    while w > 1 || h > 1 {
        let long = w.max(h);
        if long / 2 < target && long <= max_side {
            break;
        }
        (buf, w, h) = match channels {
            1 => halve::<1>(&buf, w, h),
            3 => halve::<3>(&buf, w, h),
            _ => halve::<4>(&buf, w, h),
        };
    }
    (buf, w, h)
}

/// One 2x2 box-filter halving; an odd last row/column is averaged with itself.
fn halve<const C: usize>(src: &[u8], w: usize, h: usize) -> (Vec<u8>, usize, usize) {
    let (ow, oh) = (w.div_ceil(2), h.div_ceil(2));
    let stride = w * C;
    let mut out = vec![0u8; ow * oh * C];
    out.par_chunks_mut(ow * C).enumerate().for_each(|(oy, row)| {
        let y0 = oy * 2;
        let y1 = (y0 + 1).min(h - 1);
        let r0 = &src[y0 * stride..(y0 + 1) * stride];
        let r1 = &src[y1 * stride..(y1 + 1) * stride];
        for (ox, px) in row.chunks_exact_mut(C).enumerate() {
            let x0 = ox * 2 * C;
            let x1 = (ox * 2 + 1).min(w - 1) * C;
            for c in 0..C {
                let sum = r0[x0 + c] as u16 + r0[x1 + c] as u16 + r1[x0 + c] as u16 + r1[x1 + c] as u16;
                px[c] = ((sum + 2) >> 2) as u8;
            }
        }
    });
    (out, ow, oh)
}

/// Last-resort guard before upload: egui panics on textures larger than
/// the GPU supports. Decodes are already sized for the limit; this only
/// matters when the limit wasn't known yet as the decode started.
fn clamp_to_texture_limit(image: egui::ColorImage, max_side: usize) -> egui::ColorImage {
    let [w, h] = image.size;
    if max_side == 0 || (w <= max_side && h <= max_side) {
        return image;
    }
    let bytes: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
    let limit = max_side as u32;
    let (bytes, w, h) = downscale_pow2(bytes, w, h, 4, limit, limit);
    color_image_from_premultiplied(w, h, &bytes)
}

/// Convert packed 8-bit pixels (`channels`: 3 = RGB, 4 = RGBA) from their
/// embedded ICC profile to sRGB in place. Returns true if a conversion was
/// actually performed. The common case (sRGB or untagged) short-circuits to
/// no-op, as do profiles that don't describe RGB data (gray, CMYK).
fn apply_icc_to_srgb(pixels: &mut [u8], icc_bytes: &[u8], channels: usize) -> bool {
    use lcms2::*;
    let src = match Profile::new_icc(icc_bytes) {
        Ok(p) => p,
        Err(_) => return false,
    };
    if src.color_space() != ColorSpaceSignature::RgbData {
        return false;
    }
    if let Some(desc) = src.info(InfoType::Description, Locale::none()) {
        let d = desc.to_lowercase();
        // sRGB and the various "untagged"/"display" aliases all map to our
        // assumed output space — skip the per-pixel transform.
        if d.contains("srgb") || d.contains("iec61966") {
            return false;
        }
    }
    let format = if channels == 4 { PixelFormat::RGBA_8 } else { PixelFormat::RGB_8 };
    let dst = Profile::new_srgb();
    let transform = match Transform::new(&src, format, &dst, format, Intent::Perceptual) {
        Ok(t) => t,
        Err(_) => return false,
    };
    transform.transform_in_place(pixels);
    true
}

/// The current image for the clipboard: full resolution, straight-alpha
/// RGBA in sRGB, turned the way it's displayed (EXIF orientation, then the
/// user's Q/W quarter turns).
fn export_image(path: &Path, user_quarter: i32) -> Option<image::RgbaImage> {
    let data = std::fs::read(path).ok()?;
    let RawDecode { img, orientation, icc, .. } = decode_any(&data, u32::MAX)?;
    let (img, quarter) = apply_exif_orientation_lazy(img, orientation);
    let img = match (quarter + user_quarter).rem_euclid(4) {
        1 => img.rotate90(),
        2 => img.rotate180(),
        3 => img.rotate270(),
        _ => img,
    };
    let mut rgba = img.into_rgba8();
    if let Some(icc) = icc {
        apply_icc_to_srgb(&mut rgba, &icc, 4);
    }
    // Keep the bitmap within what other apps comfortably accept.
    const MAX: u32 = 16384;
    let (w, h) = rgba.dimensions();
    if w.max(h) <= MAX {
        return Some(rgba);
    }
    let (buf, w, h) = downscale_pow2(rgba.into_raw(), w as usize, h as usize, 4, MAX, MAX);
    image::RgbaImage::from_raw(w as u32, h as u32, buf)
}

/// Header-only look at an image: dimensions, orientation and, for JPEGs,
/// the embedded EXIF thumbnail (~1 ms, against 100+ ms for a decode).
/// Formats without an embedded thumbnail get no placeholder: producing one
/// would cost about as much as the real decode it's meant to stand in for.
fn probe_image(path: &Path) -> Probe {
    if let Some(h) = read_jpeg_header(path) {
        let (w, ht) = (h.width as usize, h.height as usize);
        let dims = (w > 0 && ht > 0)
            .then_some(if matches!(h.orientation, 5 | 7) { [ht, w] } else { [w, ht] });
        if dims.is_some() || h.thumb.is_some() {
            let thumb = h.thumb.as_deref().and_then(|t| decode_exif_thumb(t, h.orientation));
            return Probe { thumb, dims, quarter: quarter_for(h.orientation) };
        }
    }
    probe_generic(path)
}

fn probe_generic(path: &Path) -> Probe {
    use image::ImageDecoder;
    let decoder = image::ImageReader::open(path)
        .ok()
        .and_then(|r| r.with_guessed_format().ok())
        .and_then(|r| r.into_decoder().ok());
    let Some(mut decoder) = decoder else { return Probe::default() };
    let orientation = decoder.orientation().map(orientation_to_exif).unwrap_or(1);
    let (w, h) = decoder.dimensions();
    let dims = if matches!(orientation, 5 | 7) { [h, w] } else { [w, h] };
    Probe {
        thumb: None,
        dims: Some(dims.map(|v| v as usize)),
        quarter: quarter_for(orientation),
    }
}

/// Decode the embedded EXIF thumbnail (typically 160x120). Cameras and
/// phones embed it for instant preview; it's stored like the main image, so
/// the main image's orientation applies.
fn decode_exif_thumb(bytes: &[u8], orientation: u32) -> Option<egui::ColorImage> {
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg).ok()?;
    let (img, _) = apply_exif_orientation_lazy(img, orientation);
    Some(to_color_image(img, None, u32::MAX, u32::MAX).0)
}

struct JpegHeader {
    width: u32,
    height: u32,
    orientation: u32,
    thumb: Option<Vec<u8>>,
}

/// Dimensions (from the SOFn frame header, always present, unlike EXIF's
/// optional size tags), EXIF orientation and EXIF thumbnail of a JPEG,
/// without decoding image data. None if the file isn't a JPEG.
fn read_jpeg_header(path: &Path) -> Option<JpegHeader> {
    use std::io::Read;
    // EXIF (≤ 64 KB, thumbnail included), ICC/XMP and the frame header all
    // come before the image data; 256 KB covers them in one read.
    let mut head = Vec::with_capacity(64 * 1024);
    std::fs::File::open(path)
        .ok()?
        .take(256 * 1024)
        .read_to_end(&mut head)
        .ok()?;
    parse_jpeg_header(&head)
}

fn parse_jpeg_header(data: &[u8]) -> Option<JpegHeader> {
    if !data.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut exif: Option<(u32, Option<Vec<u8>>)> = None;
    let mut sof: Option<(u32, u32)> = None;
    let mut pos = 2;
    while pos + 1 < data.len() {
        // A marker is 0xFF followed by a non-0xFF byte; encoders may emit any
        // number of 0xFF fill bytes between segments. Walk past them and any
        // stray non-marker bytes (corruption resilience).
        if data[pos] != 0xFF || data[pos + 1] == 0xFF {
            pos += 1;
            continue;
        }
        let m = data[pos + 1];
        pos += 2;
        // TEM / RSTn / SOI (and a stuffed 0x00) carry no length or payload.
        if m == 0x00 || m == 0x01 || (0xD0..=0xD8).contains(&m) {
            continue;
        }
        // EOI ends the image; after SOS comes entropy-coded data, and
        // everything we care about appears before it.
        if m == 0xD9 || m == 0xDA {
            break;
        }
        let Some(len) = data.get(pos..pos + 2).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize) else {
            break;
        };
        // A valid length is >= 2 (it includes the two length bytes themselves).
        if len < 2 {
            break;
        }
        let body = &data[(pos + 2).min(data.len())..(pos + len).min(data.len())];
        // SOFn family (frame header). Excludes DHT (C4), JPG (C8), DAC (CC).
        if (0xC0..=0xCF).contains(&m) && m != 0xC4 && m != 0xC8 && m != 0xCC {
            if body.len() >= 5 {
                let h = u16::from_be_bytes([body[1], body[2]]) as u32;
                let w = u16::from_be_bytes([body[3], body[4]]) as u32;
                sof = Some((w, h));
            }
        } else if m == 0xE1 && exif.is_none() && body.starts_with(b"Exif\0\0") {
            exif = parse_exif_tiff(&body[6..]);
        }
        if sof.is_some() && exif.is_some() {
            break;
        }
        pos += len;
    }
    let (orientation, thumb) = exif.unwrap_or((1, None));
    let (width, height) = sof.unwrap_or((0, 0));
    Some(JpegHeader { width, height, orientation, thumb })
}

/// Orientation (IFD0) and the embedded JPEG thumbnail (IFD1) from the TIFF
/// structure inside an EXIF segment. Offsets are relative to the start of
/// `tiff`; every read is bounds-checked, so a corrupt or truncated segment
/// yields whatever could be read.
fn parse_exif_tiff(tiff: &[u8]) -> Option<(u32, Option<Vec<u8>>)> {
    let le = match tiff.get(0..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |o: usize| -> Option<u16> {
        let b = tiff.get(o..o.checked_add(2)?)?;
        Some(if le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) })
    };
    let u32_at = |o: usize| -> Option<u32> {
        let b = tiff.get(o..o.checked_add(4)?)?;
        let b = [b[0], b[1], b[2], b[3]];
        Some(if le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    };
    let ifd0 = u32_at(4)? as usize;
    let n0 = u16_at(ifd0)? as usize;
    let mut orientation = 1;
    for i in 0..n0 {
        let e = ifd0 + 2 + i * 12;
        let Some(tag) = u16_at(e) else { break };
        if tag == 0x0112 {
            // SHORT, left-justified in the value field in both byte orders.
            orientation = u16_at(e + 8).map_or(1, u32::from);
        }
    }
    if !(1..=8).contains(&orientation) {
        orientation = 1;
    }
    let thumb = (|| {
        let ifd1 = u32_at(ifd0 + 2 + n0 * 12)? as usize;
        if ifd1 == 0 {
            return None;
        }
        let (mut off, mut len) = (None, None);
        for i in 0..u16_at(ifd1)? as usize {
            let e = ifd1 + 2 + i * 12;
            match u16_at(e)? {
                0x0201 => off = u32_at(e + 8),
                0x0202 => len = u32_at(e + 8),
                _ => {}
            }
        }
        let start = off? as usize;
        let bytes = tiff.get(start..start.checked_add(len? as usize)?)?;
        bytes.starts_with(&[0xFF, 0xD8]).then(|| bytes.to_vec())
    })();
    Some((orientation, thumb))
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

fn exif_orientation_from_tiff(tiff: &[u8]) -> Option<u32> {
    image::metadata::Orientation::from_exif_chunk(tiff).map(orientation_to_exif)
}

fn orientation_to_exif(o: image::metadata::Orientation) -> u32 {
    o.to_exif() as u32
}

/// Display rotation, in clockwise quarter turns, for an EXIF orientation
/// that is a pure rotation (3/6/8); 0 for everything else.
fn quarter_for(orientation: u32) -> i32 {
    match orientation {
        3 => 2,
        6 => 1,
        8 => 3,
        _ => 0,
    }
}

/// EXIF orientation handling: rotation-only orientations (1/3/6/8) are
/// deferred to display-time mesh rotation (free). Mirror orientations
/// (2/4/5/7) are rare and still applied in pixels here.
/// Returns (transformed_image, display_quarter_to_add).
fn apply_exif_orientation_lazy(img: image::DynamicImage, orientation: u32) -> (image::DynamicImage, i32) {
    match orientation {
        2 => (img.fliph(), 0),
        4 => (img.flipv(), 0),
        5 => (img.rotate90().fliph(), 0),
        7 => (img.rotate270().fliph(), 0),
        o => (img, quarter_for(o)),
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
                "recent" => {
                    let p = PathBuf::from(v);
                    if !p.as_os_str().is_empty() && prefs.recent.iter().all(|x| x != &p) {
                        prefs.recent.push(p);
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
    let mut content = format!("# snapview preferences\ntheme={}\n", prefs.theme.as_str());
    for p in &prefs.recent {
        content.push_str(&format!("recent={}\n", p.display()));
    }
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

enum Transfer {
    Done,
    /// A file with that name already exists in the destination.
    Skipped,
}

/// Copy (or move, when `cut`) each image and then its sidecars into `dest`,
/// calling `progress(done, total)` as it goes. Never overwrites anything.
/// An image that can't be moved keeps its sidecars with it.
fn run_file_job(
    items: &[(PathBuf, Vec<PathBuf>)],
    dest: &Path,
    cut: bool,
    mut progress: impl FnMut(usize, usize),
) -> FileJobReport {
    let mut r = FileJobReport { cut, dest: dest.to_path_buf(), ..Default::default() };
    if let Err(e) = std::fs::create_dir_all(dest) {
        r.errors.push(format!("{}: {}", dest.display(), e));
        return r;
    }
    let total = items.len();
    for (i, (image, sidecars)) in items.iter().enumerate() {
        progress(i, total);
        match transfer_file(image, dest, cut) {
            Ok(Transfer::Done) => {
                r.images += 1;
                if cut {
                    r.moved.push(image.clone());
                }
            }
            Ok(Transfer::Skipped) => {
                r.skipped += 1;
                if cut {
                    continue;
                }
            }
            Err(e) => {
                r.errors.push(format!("{}: {}", file_label(image), e));
                continue;
            }
        }
        for s in sidecars {
            match transfer_file(s, dest, cut) {
                Ok(Transfer::Done) => r.sidecars += 1,
                Ok(Transfer::Skipped) => r.skipped += 1,
                Err(e) => r.errors.push(format!("{}: {}", file_label(s), e)),
            }
        }
    }
    progress(total, total);
    r
}

/// Copy or move one file into `dest_dir` under its own name. A name that
/// already exists there is skipped rather than overwritten — which also
/// covers picking the source folder itself as the destination, where a
/// plain copy would truncate the file onto itself.
fn transfer_file(src: &Path, dest_dir: &Path, cut: bool) -> std::io::Result<Transfer> {
    let name = src
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name"))?;
    let target = dest_dir.join(name);
    if target.exists() {
        return Ok(Transfer::Skipped);
    }
    if cut && std::fs::rename(src, &target).is_ok() {
        return Ok(Transfer::Done);
    }
    // Copy under a temporary name and rename into place, so an interrupted
    // copy never leaves a truncated file under the real name (which a re-run
    // would then skip as "already there").
    let mut part = std::ffi::OsString::from(".");
    part.push(name);
    part.push(".snapview-part");
    let part = dest_dir.join(part);
    if let Err(e) = std::fs::copy(src, &part).and_then(|_| std::fs::rename(&part, &target)) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    if cut {
        if let Err(e) = std::fs::remove_file(src) {
            // Leave exactly one copy behind, not two.
            let _ = std::fs::remove_file(&target);
            return Err(e);
        }
    }
    Ok(Transfer::Done)
}

fn num_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().min(8).max(2))
        .unwrap_or(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scratch directory unique to one test, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("snapview-test-{}-{}", name, std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Minimal big-endian EXIF TIFF block: IFD0 with Orientation, IFD1
    /// pointing at `thumb`.
    fn exif_tiff(orientation: u16, thumb: &[u8]) -> Vec<u8> {
        let mut t = b"MM\x00\x2a".to_vec();
        t.extend(8u32.to_be_bytes());
        let ifd1 = 8 + 2 + 12 + 4;
        t.extend(1u16.to_be_bytes());
        t.extend([0x01, 0x12, 0x00, 0x03, 0, 0, 0, 1]);
        t.extend(orientation.to_be_bytes());
        t.extend([0, 0]);
        t.extend((ifd1 as u32).to_be_bytes());
        let thumb_off = ifd1 + 2 + 2 * 12 + 4;
        t.extend(2u16.to_be_bytes());
        t.extend([0x02, 0x01, 0x00, 0x04, 0, 0, 0, 1]);
        t.extend((thumb_off as u32).to_be_bytes());
        t.extend([0x02, 0x02, 0x00, 0x04, 0, 0, 0, 1]);
        t.extend((thumb.len() as u32).to_be_bytes());
        t.extend(0u32.to_be_bytes());
        t.extend(thumb);
        t
    }

    /// Encode an image as JPEG and splice an EXIF APP1 segment in after SOI.
    fn jpeg_with_exif(img: &image::RgbImage, orientation: u16, thumb: &[u8]) -> Vec<u8> {
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
            .encode_image(img)
            .unwrap();
        let mut payload = b"Exif\0\0".to_vec();
        payload.extend(exif_tiff(orientation, thumb));
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
        out.extend(((payload.len() + 2) as u16).to_be_bytes());
        out.extend(payload);
        out.extend(&jpeg[2..]);
        out
    }

    fn gradient(w: u32, h: u32) -> image::RgbImage {
        image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]))
    }

    #[test]
    fn natural_order_matches_explorer() {
        let mut names = vec!["IMG_10.jpg", "img_2.JPG", "IMG_1.jpg", "b.png", "A.png", "IMG_01.jpg"];
        names.sort_by(|a, b| compare_paths(Path::new(a), Path::new(b)));
        assert_eq!(names, ["A.png", "b.png", "IMG_01.jpg", "IMG_1.jpg", "img_2.JPG", "IMG_10.jpg"]);
        assert_eq!(natural_cmp("photo 9", "photo 10"), CmpOrdering::Less);
        assert_eq!(natural_cmp("x99999999999999999999", "x100000000000000000000"), CmpOrdering::Less);
    }

    #[test]
    fn jpeg_header_reads_sof_orientation_and_thumbnail() {
        let mut thumb = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut thumb).encode_image(&gradient(16, 12)).unwrap();
        let data = jpeg_with_exif(&gradient(64, 48), 6, &thumb);
        let h = parse_jpeg_header(&data).unwrap();
        assert_eq!((h.width, h.height, h.orientation), (64, 48, 6));
        assert_eq!(h.thumb.as_deref(), Some(thumb.as_slice()));

        // Truncated in the middle of the EXIF block: orientation survives,
        // the thumbnail (and frame header) are simply missing.
        let cut = &data[..60];
        let h = parse_jpeg_header(cut).unwrap();
        assert_eq!(h.orientation, 6);
        assert!(h.thumb.is_none());
        assert!(parse_jpeg_header(b"\x89PNG\r\n\x1a\n").is_none());
    }

    #[test]
    fn probe_and_decode_agree_on_dims_for_rotated_jpeg() {
        let dir = TempDir::new("probe");
        let path = dir.0.join("portrait.jpg");
        let mut thumb = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut thumb).encode_image(&gradient(16, 12)).unwrap();
        std::fs::write(&path, jpeg_with_exif(&gradient(400, 300), 6, &thumb)).unwrap();

        // Stored landscape, displayed portrait: dims stay in stored
        // (pre-rotation) space with a quarter turn on top.
        let probe = probe_image(&path);
        assert_eq!(probe.dims, Some([400, 300]));
        assert_eq!(probe.quarter, 1);
        assert!((probe.display_aspect().unwrap() - 0.75).abs() < 1e-6);
        assert_eq!(probe.thumb.as_ref().map(|t| t.size), Some([16, 12]));

        let d = decode_full(&path, 2400, 8192).unwrap();
        assert_eq!(d.src_dims, [400, 300]);
        assert_eq!(d.image.size, [400, 300]);
        assert_eq!(d.quarter, 1);
        assert!(d.opaque);
    }

    #[test]
    fn decode_scales_into_target_size_class() {
        let dir = TempDir::new("scale");
        let jpg = dir.0.join("big.jpg");
        let mut buf = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut buf).encode_image(&gradient(1600, 1200)).unwrap();
        std::fs::write(&jpg, buf).unwrap();
        // DCT scaling: smallest of 1/8..1 keeping the long side >= target.
        assert_eq!(decode_full(&jpg, 700, 8192).unwrap().image.size, [800, 600]);
        let png = dir.0.join("big.png");
        gradient(1600, 1200).save(&png).unwrap();
        // Box halving lands in the same class.
        assert_eq!(decode_full(&png, 700, 8192).unwrap().image.size, [800, 600]);
        // The GPU limit wins over the target.
        assert_eq!(decode_full(&png, 2400, 1000).unwrap().image.size, [800, 600]);
        // Source dims are always the original's.
        assert_eq!(decode_full(&png, 700, 8192).unwrap().src_dims, [1600, 1200]);
    }

    #[test]
    fn halving_averages_and_handles_odd_edges() {
        // 3x1 gray: [0, 100, 200] -> 2x1: avg(0,100)=50, edge 200 alone.
        let (out, w, h) = downscale_pow2(vec![0, 100, 200], 3, 1, 1, 2, 2);
        assert_eq!((w, h), (2, 1));
        assert_eq!(out, vec![50, 200]);
        let (_, w, h) = downscale_pow2(vec![0; 6000 * 10 * 3], 6000, 10, 3, 2400, 16384);
        assert_eq!((w, h), (3000, 5));
        // Already in class: untouched.
        let (_, w, h) = downscale_pow2(vec![0; 3000 * 3], 3000, 1, 3, 2400, 16384);
        assert_eq!((w, h), (3000, 1));
    }

    #[test]
    fn transparent_pixels_are_premultiplied() {
        let mut px = vec![255, 255, 255, 0, 10, 20, 30, 255, 200, 100, 50, 128];
        assert!(!premultiply_alpha(&mut px));
        assert_eq!(&px[0..4], &[0, 0, 0, 0]);
        assert_eq!(&px[4..8], &[10, 20, 30, 255]);
        assert!(px[8] < 200 && px[11] == 128);
        let mut opaque = vec![1, 2, 3, 255];
        assert!(premultiply_alpha(&mut opaque));
    }

    #[test]
    fn mirrored_orientations_swap_axes_in_pixels() {
        let img = image::DynamicImage::ImageRgb8(gradient(4, 2));
        for o in [5, 7] {
            let (out, q) = apply_exif_orientation_lazy(img.clone(), o);
            assert_eq!((out.width(), out.height(), q), (2, 4, 0));
        }
        let (out, q) = apply_exif_orientation_lazy(img, 6);
        assert_eq!((out.width(), out.height(), q), (4, 2, 1));
    }

    #[test]
    fn transfer_never_overwrites_and_keeps_one_copy() {
        let dir = TempDir::new("transfer");
        let (src, dst) = (dir.0.join("src"), dir.0.join("dst"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        let a = src.join("a.jpg");
        std::fs::write(&a, b"new").unwrap();
        std::fs::write(dst.join("a.jpg"), b"old").unwrap();
        assert!(matches!(transfer_file(&a, &dst, false).unwrap(), Transfer::Skipped));
        assert_eq!(std::fs::read(dst.join("a.jpg")).unwrap(), b"old");

        // Copying a folder onto itself must not truncate anything.
        assert!(matches!(transfer_file(&a, &src, false).unwrap(), Transfer::Skipped));
        assert_eq!(std::fs::read(&a).unwrap(), b"new");

        let b = src.join("b.jpg");
        std::fs::write(&b, b"bee").unwrap();
        assert!(matches!(transfer_file(&b, &dst, true).unwrap(), Transfer::Done));
        assert!(!b.exists());
        assert_eq!(std::fs::read(dst.join("b.jpg")).unwrap(), b"bee");
        let leftovers: Vec<_> = std::fs::read_dir(&dst)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".snapview-part"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn file_job_moves_sidecars_with_their_image() {
        let dir = TempDir::new("job");
        let (src, dst) = (dir.0.join("src"), dir.0.join("dst"));
        std::fs::create_dir_all(&src).unwrap();
        let (jpg, raw) = (src.join("IMG_1.JPG"), src.join("IMG_1.CR3"));
        std::fs::write(&jpg, b"j").unwrap();
        std::fs::write(&raw, b"r").unwrap();
        let report = run_file_job(&[(jpg.clone(), vec![raw.clone()])], &dst, true, |_, _| {});
        assert_eq!((report.images, report.sidecars, report.skipped), (1, 1, 0));
        assert!(report.errors.is_empty());
        assert_eq!(report.moved, vec![jpg.clone()]);
        assert!(!jpg.exists() && !raw.exists());
        assert!(dst.join("IMG_1.JPG").exists() && dst.join("IMG_1.CR3").exists());
    }

    #[test]
    fn scan_lists_images_naturally_and_pairs_sidecars() {
        let dir = TempDir::new("scan");
        for name in ["IMG_10.jpg", "IMG_2.jpg", "IMG_2.CR2", "._IMG_2.jpg", "notes.txt"] {
            std::fs::write(dir.0.join(name), b"x").unwrap();
        }
        std::fs::create_dir(dir.0.join("folder.jpg")).unwrap();
        let (images, sidecars) = scan_folder(&dir.0);
        let names: Vec<String> = images.iter().map(|p| file_label(p)).collect();
        assert_eq!(names, ["IMG_2.jpg", "IMG_10.jpg"]);
        assert_eq!(sidecars.get(&dir.0.join("IMG_2.jpg")), Some(&vec![dir.0.join("IMG_2.CR2")]));
    }

    #[test]
    fn queue_replace_drops_stale_work_and_skips_in_flight() {
        let q = PrioQueue::new();
        let p = |s: &str| PathBuf::from(s);
        q.replace(&[p("a"), p("b"), p("c")]);
        assert_eq!(q.pop(), Some(p("a")));
        // "a" is being decoded: re-requesting it must not queue a duplicate.
        q.replace(&[p("a"), p("d")]);
        assert_eq!(q.pop(), Some(p("d")));
        q.done(Path::new("a"));
        q.prioritize(&[p("a")]);
        assert_eq!(q.pop(), Some(p("a")));
        q.close();
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn window_sizes_fit_monitor() {
        assert_eq!(initial_window_size(None), [1200.0, 800.0]);
        assert_eq!(initial_window_size(Some(0.75)), [600.0, 800.0]);
        let [w, h] = initial_window_size(Some(8.0));
        assert!(w <= 1600.0 && h >= 200.0);
        let (w, h) = fit_window_to_aspect(8.0, 800.0, Some(egui::vec2(1920.0, 1080.0)));
        assert!(w <= 1920.0 * 0.95 + 0.01);
        assert!((w / h - 8.0).abs() < 0.01);
        assert_eq!(aspect_class(3024, 4032), 1);
        assert_eq!(aspect_class(4032, 3024), 0);
    }

    #[test]
    fn elide_keeps_both_ends() {
        let width = |s: &str| s.chars().count() as f32;
        assert_eq!(elide_middle("short.jpg", 20.0, &width), "short.jpg");
        let e = elide_middle("IMG_20240412_123456.jpg", 11.0, &width);
        assert_eq!(e.chars().count(), 11);
        assert!(e.starts_with("IMG_") && e.ends_with(".jpg") && e.contains('…'));
    }
}
