//! egui front end: toolbar, folder table, treemap, status bar.

use crate::tree::{fmt_count, fmt_filetime, fmt_size, Method, Progress, SortKey, Tree, NO_PARENT};
use crate::treemap::{self, Item, FREE, R};
use crate::icons::{self, Icons};
use crate::win;
use eframe::egui::{
    self, Align, Align2, Color32, FontId, Layout, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2,
};
use egui_extras::{Column, TableBuilder};
use std::sync::atomic::Ordering::Relaxed;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

type ScanResult = Result<(Tree, Option<String>), String>;

struct Scan {
    progress: Arc<Progress>,
    rx: Receiver<ScanResult>,
    start: Instant,
    method: Method,
}

#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
enum Pref {
    Auto,
    Walk,
}

const AUTHOR: &str = "destory1984";
const REPO_URL: &str = "https://github.com/destory1984/oddity-diskmap";

/// What is remembered between runs (eframe storage, %APPDATA%\oddity-diskmap\datapp.ron).
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Saved {
    pref: Pref,
    use_alloc: bool,
    video_preview: bool,
    cache_max_mb: u64,
    history_on: bool,
    history_depth: u32,
    history_min_mb: u64,
    history_keep: usize,
}

impl Default for Saved {
    fn default() -> Self {
        Saved {
            pref: Pref::Auto,
            use_alloc: false,
            video_preview: true,
            cache_max_mb: crate::cache::DEFAULT_MAX_MB,
            history_on: true,
            history_depth: 4,
            history_min_mb: 10,
            history_keep: 10,
        }
    }
}

enum Action {
    Select(u32),
    Reveal(u32),
    Toggle(u32),
    MapRoot(u32),
    Explorer(u32),
    Copy(u32),
    Sort(SortKey),
    HoverVideo(u32),
    RevealPath(String),
    Open(u32),
}

pub struct App {
    drives: Vec<win::Drive>,
    path: String,
    admin: bool,
    pref: Pref,
    scan: Option<Scan>,
    tree: Option<Tree>,
    message: Option<String>,
    expanded: Vec<bool>,
    rows: Vec<(u32, u16)>,
    selected: Option<u32>,
    scroll_to: Option<usize>,
    map_root: u32,
    use_alloc: bool,
    sort: (SortKey, bool),
    free_space: Option<(u64, u64)>,
    generation: u64,
    layout_cache: Option<(u64, u32, Rect, bool, Vec<Item>)>,
    icons: Icons,
    show_settings: bool,
    show_about: bool,
    video_preview: bool,
    clips: HashMap<u32, Clip>,
    clip_order: VecDeque<u32>,
    clip_tx: Sender<ClipMsg>,
    clip_rx: Receiver<ClipMsg>,
    clips_running: Arc<AtomicUsize>,
    /// node the pointer rests on (u64::MAX: none); a worker for another node gives up
    clip_want: Arc<AtomicU64>,
    /// video file under the pointer this frame, and since when it has been there
    hover_now: Option<u32>,
    hover: Option<(u32, Instant)>,
    cache_max_mb: u64,
    /// bytes on disk, u64::MAX while being measured
    cache_usage: Arc<AtomicU64>,
    history_on: bool,
    history_depth: u32,
    history_min_mb: u64,
    history_keep: usize,
    /// bytes of all saved history, u64::MAX while being measured
    history_usage: Arc<AtomicU64>,
    /// snapshot written for the tree on screen (left out of the comparison list)
    last_snapshot: Option<std::path::PathBuf>,
    /// (generation, depth, min) -> (folders, bytes) the current settings would write
    history_estimate: Option<((u64, u32, u64), (usize, u64))>,
    show_changes: bool,
    changes: Option<ChangesView>,
}

/// State of the "지난번 대비" window.
struct ChangesView {
    snaps: Vec<crate::history::Meta>,
    pick: usize,
    causes_only: bool,
    /// (generation, pick, causes_only) the rows were computed for
    made_for: Option<(u64, usize, bool)>,
    rows: Vec<crate::history::Change>,
    old_total: u64,
    error: Option<String>,
}

/// Preview frames for one video file.
enum Clip {
    Pending,
    Ready(Vec<egui::TextureHandle>),
    Failed,
}

/// (tree generation, node, frames or None on failure)
/// (tree generation, node, frames; Err(true) = cancelled, Err(false) = failed)
type ClipMsg = (u64, u32, Result<Vec<egui::TextureHandle>, bool>);

const CLIP_FRAMES: usize = 10;
const CLIP_W: u32 = 320;
const CLIP_STEP: f32 = 0.7;
const CLIP_HOVER_DELAY: f32 = 0.3;
const CLIP_CACHE: usize = 24;
const CLIP_THREADS: usize = 2;

const LEVEL_COLORS: [Color32; 8] = [
    Color32::from_rgb(38, 70, 140),
    Color32::from_rgb(44, 92, 170),
    Color32::from_rgb(52, 114, 188),
    Color32::from_rgb(62, 134, 200),
    Color32::from_rgb(78, 152, 208),
    Color32::from_rgb(98, 168, 214),
    Color32::from_rgb(120, 182, 220),
    Color32::from_rgb(142, 194, 226),
];
const FILE_COLOR: Color32 = Color32::from_rgb(70, 150, 130);
const FREE_COLOR: Color32 = Color32::from_rgb(180, 0, 180);
const BAR_COLOR: Color32 = Color32::from_rgb(70, 90, 200);

fn level_color(depth: u16) -> Color32 {
    LEVEL_COLORS[(depth as usize).min(LEVEL_COLORS.len() - 1)]
}

fn to_rect(r: R) -> Rect {
    Rect::from_min_size(Pos2::new(r.x, r.y), Vec2::new(r.w, r.h))
}

fn install_fonts(ctx: &egui::Context) {
    // egui's built-in fonts have no Hangul; add Malgun Gothic from Windows.
    for (name, file) in [("malgun", "malgun.ttf"), ("gulim", "gulim.ttc")] {
        let path = format!(r"C:\Windows\Fonts\{file}");
        if let Ok(bytes) = std::fs::read(&path) {
            ctx.add_font(egui::epaint::text::FontInsert::new(
                name,
                egui::FontData::from_owned(bytes),
                vec![
                    egui::epaint::text::InsertFontFamily {
                        family: egui::FontFamily::Proportional,
                        priority: egui::epaint::text::FontPriority::Highest,
                    },
                    egui::epaint::text::InsertFontFamily {
                        family: egui::FontFamily::Monospace,
                        priority: egui::epaint::text::FontPriority::Lowest,
                    },
                ],
            ));
            break;
        }
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, start: Option<String>) -> Self {
        install_fonts(&cc.egui_ctx);
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        let saved: Saved = cc
            .storage
            .and_then(|st| eframe::get_value(st, eframe::APP_KEY))
            .unwrap_or_default();
        let drives = win::drives();
        let (clip_tx, clip_rx) = channel();
        let limit = saved.cache_max_mb.clamp(crate::cache::MIN_MB, crate::cache::MAX_MB) * 1024 * 1024;
        std::thread::spawn(move || crate::cache::trim(limit));
        let path = start
            .clone()
            .or_else(|| drives.iter().find(|d| d.root == "C:\\").map(|d| d.root.clone()))
            .unwrap_or_else(|| "C:\\".into());
        let mut app = App {
            drives,
            path,
            admin: win::is_admin(),
            pref: saved.pref,
            scan: None,
            tree: None,
            message: None,
            expanded: Vec::new(),
            rows: Vec::new(),
            selected: None,
            scroll_to: None,
            map_root: 0,
            use_alloc: saved.use_alloc,
            sort: (SortKey::Size, true),
            free_space: None,
            generation: 0,
            layout_cache: None,
            icons: Icons::new(&cc.egui_ctx),
            // DISKMAP_SHOW=settings / about opens that window at start (for screenshots)
            show_settings: matches!(std::env::var("DISKMAP_SHOW").as_deref(), Ok("settings" | "about")),
            show_about: std::env::var("DISKMAP_SHOW").as_deref() == Ok("about"),
            video_preview: saved.video_preview,
            clips: HashMap::new(),
            clip_order: VecDeque::new(),
            clip_tx,
            clip_rx,
            clips_running: Arc::new(AtomicUsize::new(0)),
            clip_want: Arc::new(AtomicU64::new(u64::MAX)),
            hover_now: None,
            hover: None,
            cache_max_mb: saved.cache_max_mb.clamp(crate::cache::MIN_MB, crate::cache::MAX_MB),
            cache_usage: Arc::new(AtomicU64::new(u64::MAX)),
            history_on: saved.history_on,
            history_depth: saved.history_depth.clamp(1, 12),
            history_min_mb: saved.history_min_mb.clamp(1, 10_240),
            history_keep: saved.history_keep.clamp(1, 200),
            history_usage: Arc::new(AtomicU64::new(u64::MAX)),
            last_snapshot: None,
            history_estimate: None,
            show_changes: std::env::var("DISKMAP_SHOW").as_deref() == Ok("changes"),
            changes: None,
        };
        if start.is_some() {
            app.start_scan();
        }
        app
    }

    fn start_scan(&mut self) {
        if self.scan.is_some() {
            return;
        }
        self.path = crate::normalize_path(&self.path);
        let path = self.path.clone();
        let root = path.get(..3).unwrap_or(&path).to_string();
        let mft_ok = self.admin && win::fs_name(&root) == "NTFS";
        let method = if self.pref == Pref::Auto && mft_ok { Method::Mft } else { Method::Walk };
        let progress = Arc::new(Progress::default());
        let (tx, rx) = channel();
        let p = progress.clone();
        std::thread::spawn(move || {
            let _ = tx.send(crate::run_scan(&path, method, &p));
        });
        self.message = None;
        self.scan = Some(Scan { progress, rx, start: Instant::now(), method });
    }

    fn poll_scan(&mut self) {
        let Some(scan) = &self.scan else { return };
        let Ok(res) = scan.rx.try_recv() else { return };
        self.scan = None;
        match res {
            Ok((mut tree, note)) => {
                self.clips.clear();
                self.clip_order.clear();
                self.hover = None;
                tree.sort(self.sort.0, self.sort.1);
                self.message = note;
                self.expanded = vec![false; tree.nodes.len()];
                self.expanded[tree.root as usize] = true;
                self.map_root = tree.root;
                self.selected = Some(tree.root);
                self.free_space = if tree.root_path.len() <= 3 {
                    Some(win::disk_space(&tree.root_path))
                } else {
                    None
                };
                self.tree = Some(tree);
                self.generation += 1;
                self.rebuild_rows();
                self.drives = win::drives();
                self.last_snapshot = None;
                self.changes = None;
                if self.history_on {
                    self.record_history();
                }
            }
            Err(e) => self.message = Some(e),
        }
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        let Some(t) = &self.tree else { return };
        let mut stack = vec![(t.root, 0u16)];
        while let Some((i, d)) = stack.pop() {
            self.rows.push((i, d));
            if self.expanded[i as usize] {
                for &c in t.nodes[i as usize].children.iter().rev() {
                    stack.push((c, d + 1));
                }
            }
        }
    }

    /// Select a node, open every folder above it and scroll the table to it.
    fn reveal(&mut self, idx: u32) {
        let Some(t) = &self.tree else { return };
        let chain = t.chain(idx);
        for &a in &chain[..chain.len() - 1] {
            self.expanded[a as usize] = true;
        }
        self.rebuild_rows();
        self.selected = Some(idx);
        self.scroll_to = self.rows.iter().position(|&(i, _)| i == idx);
    }

    fn apply(&mut self, actions: Vec<Action>, ctx: &egui::Context) {
        for a in actions {
            match a {
                Action::Select(i) => self.selected = Some(i),
                Action::Reveal(i) => self.reveal(i),
                Action::Toggle(i) => {
                    self.expanded[i as usize] = !self.expanded[i as usize];
                    self.rebuild_rows();
                }
                Action::MapRoot(i) => {
                    self.map_root = i;
                }
                Action::Open(i) => {
                    if let Some(t) = &self.tree {
                        let path = t.path_of(i);
                        if !win::open_file(&path) {
                            self.message = Some(format!("열 수 없습니다: {path}"));
                        }
                    }
                }
                Action::Explorer(i) => {
                    if let Some(t) = &self.tree {
                        win::show_in_explorer(&t.path_of(i));
                    }
                }
                Action::Copy(i) => {
                    if let Some(t) = &self.tree {
                        ctx.copy_text(t.path_of(i));
                    }
                }
                Action::HoverVideo(i) => self.hover_now = Some(i),
                Action::RevealPath(rel) => {
                    if let Some(i) = self.tree.as_ref().and_then(|t| t.find_path(&rel)) {
                        self.reveal(i);
                    } else {
                        self.message = Some(format!("지금 목록에 없는 폴더입니다: {rel}"));
                    }
                }
                Action::Sort(k) => {
                    self.sort = if self.sort.0 == k {
                        (k, !self.sort.1)
                    } else {
                        (k, k != SortKey::Name)
                    };
                    if let Some(t) = &mut self.tree {
                        t.sort(self.sort.0, self.sort.1);
                    }
                    self.rebuild_rows();
                    if let Some(s) = self.selected {
                        self.scroll_to = self.rows.iter().position(|&(i, _)| i == s);
                    }
                }
            }
        }
    }

    // ---------------------------------------------------------------- history

    fn history_detail(&self) -> crate::history::Detail {
        crate::history::Detail { depth: self.history_depth, min_bytes: self.history_min_mb * 1024 * 1024 }
    }

    /// Save the folder sizes of the tree on screen.
    fn record_history(&mut self) {
        let Some(t) = &self.tree else { return };
        let entries = crate::history::collect(t, self.history_detail());
        let when = chrono::Utc::now().timestamp();
        match crate::history::save(t, when, &entries, self.history_keep) {
            Ok(p) => self.last_snapshot = Some(p),
            Err(e) => self.message = Some(format!("기록 저장 실패: {e}")),
        }
    }

    fn measure_history(&self, ctx: &egui::Context) {
        let (usage, ctx) = (self.history_usage.clone(), ctx.clone());
        usage.store(u64::MAX, Relaxed);
        std::thread::spawn(move || {
            usage.store(crate::history::usage(), Relaxed);
            ctx.request_repaint();
        });
    }

    /// "지난번 대비": folders that grew (or shrank) the most since a saved scan.
    fn changes_window(&mut self, ctx: &egui::Context) {
        if !self.show_changes {
            return;
        }
        // no tree yet (still scanning): keep the window wanted, draw it once there is one
        let Some(t) = &self.tree else { return };
        if self.changes.is_none() {
            let snaps: Vec<_> = crate::history::list(&t.root_path)
                .into_iter()
                .filter(|m| Some(&m.path) != self.last_snapshot.as_ref())
                .collect();
            self.changes = Some(ChangesView {
                snaps,
                pick: 0,
                causes_only: true,
                made_for: None,
                rows: Vec::new(),
                old_total: 0,
                error: None,
            });
        }
        let detail = self.history_detail();
        let generation = self.generation;
        let cv = self.changes.as_mut().unwrap();
        let want = (generation, cv.pick, cv.causes_only);
        if cv.made_for != Some(want) && !cv.snaps.is_empty() {
            cv.made_for = Some(want);
            let meta = &cv.snaps[cv.pick.min(cv.snaps.len() - 1)];
            match crate::history::load(&meta.path) {
                Some(old) => {
                    // current side: same depth, no size floor, so new folders show up
                    let cur = crate::history::collect(t, crate::history::Detail { depth: detail.depth, min_bytes: 0 });
                    // list folders that changed by 1 MB or more
                    cv.rows = crate::history::compare(&old, &cur, 1024 * 1024, cv.causes_only);
                    cv.old_total = meta.total_alloc;
                    cv.error = None;
                }
                None => {
                    cv.rows.clear();
                    cv.error = Some("기록 파일을 읽지 못했습니다".into());
                }
            }
        }

        let muted = Color32::from_gray(150);
        let up = Color32::from_rgb(235, 110, 90);
        let down = Color32::from_rgb(110, 200, 130);
        let fmt_delta = |d: i64| {
            let s = fmt_size(d.unsigned_abs());
            if d >= 0 { format!("+{s}") } else { format!("-{s}") }
        };
        let fmt_when = |w: i64| {
            chrono::DateTime::from_timestamp(w, 0)
                .map(|t| t.with_timezone(&chrono::Local).format("%y-%m-%d %H:%M").to_string())
                .unwrap_or_default()
        };
        let mut open = true;
        let mut actions = Vec::new();
        let cur_method = t.method;
        let cur_total = t.nodes[t.root as usize].alloc;
        let root_path = t.root_path.clone();
        egui::Window::new(egui::RichText::new("지난번 대비 변화").size(17.0))
            .id(egui::Id::unique("changes_window"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([680.0, 520.0])
            .show(ctx, |ui| {
                ui.label(egui::RichText::new(&root_path).color(muted));
                if cv.snaps.is_empty() {
                    ui.add_space(8.0);
                    ui.label("비교할 지난 기록이 없습니다.");
                    ui.label(
                        egui::RichText::new(
                            "스캔할 때마다 폴더 크기를 기록합니다. 같은 경로를 한 번 더 스캔하면 여기서 비교할 수 있습니다. \
                             기록 범위와 보관 개수는 설정에서 바꿉니다.",
                        )
                        .color(muted),
                    );
                    return;
                }
                ui.horizontal(|ui| {
                    ui.label("비교할 기록");
                    let label = |m: &crate::history::Meta| {
                        format!(
                            "{}  ({}, {}, 기록 {})",
                            fmt_when(m.when),
                            fmt_size(m.total_alloc),
                            if m.method == "Mft" { "MFT" } else { "일반" },
                            fmt_size(m.bytes)
                        )
                    };
                    egui::ComboBox::from_id_salt("changes_pick")
                        .selected_text(label(&cv.snaps[cv.pick]))
                        .width(260.0)
                        .show_ui(ui, |ui| {
                            for (k, m) in cv.snaps.iter().enumerate() {
                                ui.selectable_value(&mut cv.pick, k, label(m));
                            }
                        });
                    ui.checkbox(&mut cv.causes_only, "원인 폴더만")
                        .on_hover_text("하위 폴더 하나가 변화의 80% 이상을 차지하면 위 폴더는 숨깁니다");
                });
                let d = cur_total as i64 - cv.old_total as i64;
                ui.horizontal(|ui| {
                    ui.label(format!("전체 할당 {} → {}", fmt_size(cv.old_total), fmt_size(cur_total)));
                    ui.colored_label(if d >= 0 { up } else { down }, fmt_delta(d));
                });
                let old_method = &cv.snaps[cv.pick].method;
                let now_method = if cur_method == Method::Mft { "Mft" } else { "Walk" };
                if old_method != now_method {
                    ui.colored_label(
                        Color32::from_rgb(230, 180, 80),
                        "스캔 방식이 달라 숫자가 어긋날 수 있습니다 (일반 방식은 권한 없는 폴더를 빼고, 하드 링크를 여러 번 셉니다).",
                    );
                }
                if let Some(e) = &cv.error {
                    ui.colored_label(up, e);
                }
                ui.separator();
                if cv.rows.is_empty() && cv.error.is_none() {
                    ui.label("1 MB 넘게 바뀐 폴더가 없습니다.");
                    return;
                }
                TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    .sense(Sense::click())
                    .cell_layout(Layout::left_to_right(Align::Center))
                    .column(Column::initial(100.0).at_least(70.0))
                    .column(Column::initial(90.0).at_least(60.0))
                    .column(Column::initial(90.0).at_least(60.0))
                    .column(Column::remainder().at_least(120.0).clip(true))
                    .header(22.0, |mut h| {
                        for title in ["변화", "이전", "지금", "폴더"] {
                            h.col(|ui| {
                                ui.strong(title);
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(22.0, cv.rows.len(), |mut row| {
                            let c = &cv.rows[row.index()];
                            row.col(|ui| {
                                if c.old.is_none() {
                                    // not in the old record: new, or under its size floor then
                                    ui.colored_label(up, format!("{} 이하", fmt_delta(c.delta)))
                                        .on_hover_text("지난 기록에 없던 폴더입니다. 새로 생겼거나, 그때 기록 최소 크기보다 작았습니다");
                                } else {
                                    ui.colored_label(if c.delta >= 0 { up } else { down }, fmt_delta(c.delta));
                                }
                            });
                            row.col(|ui| {
                                ui.label(c.old.map(fmt_size).unwrap_or_else(|| "기록 없음".into()))
                                    .on_hover_text("지난번에는 없었거나, 기록 최소 크기보다 작았습니다");
                            });
                            row.col(|ui| {
                                ui.label(c.new.map(fmt_size).unwrap_or_else(|| "없음".into()));
                            });
                            row.col(|ui| {
                                let name = if c.rel.is_empty() { "(전체)" } else { c.rel.as_str() };
                                ui.add(egui::Label::new(name).truncate());
                            });
                            let resp = row.response();
                            if resp.clicked() && c.new.is_some() {
                                actions.push(Action::RevealPath(c.rel.clone()));
                            }
                            resp.on_hover_text("누르면 표에서 이 폴더를 찾습니다");
                        });
                    });
            });
        if !open {
            self.show_changes = false;
        }
        self.apply(actions, ctx);
    }

    // ---------------------------------------------------------------- video preview

    /// Start making preview frames for a video file (at most CLIP_THREADS at once).
    fn request_clip(&mut self, idx: u32, ctx: &egui::Context) {
        if self.clips.contains_key(&idx) {
            if let Some(pos) = self.clip_order.iter().position(|&i| i == idx) {
                self.clip_order.remove(pos);
                self.clip_order.push_back(idx);
            }
            return;
        }
        if self.clips_running.load(Relaxed) >= CLIP_THREADS {
            return; // try again next frame while the pointer stays
        }
        let Some(t) = &self.tree else { return };
        let path = std::path::PathBuf::from(t.path_of(idx));
        let (size, mtime) = (t.nodes[idx as usize].size, t.nodes[idx as usize].mtime);
        self.clips.insert(idx, Clip::Pending);
        self.clip_order.push_back(idx);
        while self.clip_order.len() > CLIP_CACHE {
            if let Some(old) = self.clip_order.pop_front() {
                self.clips.remove(&old);
            }
        }
        let (tx, ctx, running, generation, want) = (
            self.clip_tx.clone(),
            ctx.clone(),
            self.clips_running.clone(),
            self.generation,
            self.clip_want.clone(),
        );
        running.fetch_add(1, Relaxed);
        std::thread::spawn(move || {
            let key = format!("{}#clip{CLIP_FRAMES}", path.display());
            let frames = match crate::cache::load(&key, size, mtime) {
                Some(strip) if strip.width() >= CLIP_W => {
                    let n = (strip.width() / CLIP_W) as usize;
                    Some(
                        (0..n)
                            .map(|k| image::imageops::crop_imm(&strip, k as u32 * CLIP_W, 0, CLIP_W, strip.height()).to_image())
                            .collect::<Vec<_>>(),
                    )
                }
                _ => crate::video::video_frames(&path, CLIP_FRAMES, CLIP_W, &|| want.load(Relaxed) == idx as u64).ok().inspect(|v| {
                    let h = v.iter().map(|f| f.height()).max().unwrap_or(1);
                    let mut strip = image::RgbaImage::new(CLIP_W * v.len() as u32, h);
                    for (k, f) in v.iter().enumerate() {
                        image::imageops::replace(&mut strip, f, k as i64 * CLIP_W as i64, 0);
                    }
                    crate::cache::store(&key, size, mtime, &strip);
                }),
            };
            let frames = frames.map(|v| {
                v.into_iter()
                    .enumerate()
                    .map(|(k, img)| {
                        let ci = egui::ColorImage::from_rgba_unmultiplied(
                            [img.width() as usize, img.height() as usize],
                            img.as_raw(),
                        );
                        ctx.load_texture(format!("clip/{generation}/{idx}/{k}"), ci, egui::TextureOptions::LINEAR)
                    })
                    .collect()
            });
            running.fetch_sub(1, Relaxed);
            let cancelled = want.load(Relaxed) != idx as u64;
            let _ = tx.send((generation, idx, frames.ok_or(cancelled)));
            ctx.request_repaint();
        });
    }

    /// Floating preview next to the pointer while it rests on a video file.
    fn video_preview_popup(&mut self, ctx: &egui::Context) {
        while let Ok((g, idx, frames)) = self.clip_rx.try_recv() {
            if g == self.generation && self.clips.contains_key(&idx) {
                match frames {
                    Ok(f) => {
                        self.clips.insert(idx, Clip::Ready(f));
                    }
                    // cancelled: forget it, so resting on it again starts over
                    Err(true) => {
                        self.clips.remove(&idx);
                        self.clip_order.retain(|&i| i != idx);
                    }
                    Err(false) => {
                        self.clips.insert(idx, Clip::Failed);
                    }
                }
            }
        }
        if !self.video_preview {
            self.hover = None;
            self.clip_want.store(u64::MAX, Relaxed);
            return;
        }
        self.hover = match (self.hover_now, self.hover) {
            (Some(i), Some((j, since))) if i == j => Some((j, since)),
            (Some(i), _) => Some((i, Instant::now())),
            (None, _) => None,
        };
        self.clip_want.store(self.hover.map(|h| h.0 as u64).unwrap_or(u64::MAX), Relaxed);
        let Some((idx, since)) = self.hover else { return };
        let waited = since.elapsed().as_secs_f32();
        if waited < CLIP_HOVER_DELAY {
            ctx.request_repaint_after(Duration::from_secs_f32(CLIP_HOVER_DELAY - waited + 0.01));
            return;
        }
        self.request_clip(idx, ctx);
        let Some(t) = &self.tree else { return };
        let Some(pointer) = ctx.pointer_hover_pos() else { return };
        let n = &t.nodes[idx as usize];
        let screen = ctx.content_rect();
        let size = Vec2::new(CLIP_W as f32 + 16.0, CLIP_W as f32 * 9.0 / 16.0 + 70.0);
        let mut at = pointer + Vec2::new(18.0, 18.0);
        if at.x + size.x > screen.right() {
            at.x = pointer.x - size.x - 12.0;
        }
        if at.y + size.y > screen.bottom() {
            at.y = (pointer.y - size.y - 12.0).max(screen.top());
        }
        let clip = self.clips.get(&idx);
        egui::Area::new(egui::Id::unique("video_preview"))
            .order(egui::Order::Tooltip)
            .fixed_pos(at)
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(CLIP_W as f32);
                    match clip {
                        Some(Clip::Ready(frames)) => {
                            let k = ((waited - CLIP_HOVER_DELAY) / CLIP_STEP) as usize % frames.len();
                            let tex = &frames[k];
                            let sz = tex.size_vec2();
                            // fit in CLIP_W x 240 (portrait phone video stays short)
                            ui.image((tex.id(), sz * (CLIP_W as f32 / sz.x).min(240.0 / sz.y)));
                            ui.horizontal(|ui| {
                                for j in 0..frames.len() {
                                    let (r, _) = ui.allocate_exact_size(Vec2::new(10.0, 4.0), Sense::hover());
                                    let c = if j == k { Color32::from_rgb(90, 160, 240) } else { Color32::from_gray(70) };
                                    ui.painter().rect_filled(r, 1.0, c);
                                }
                            });
                            ctx.request_repaint_after(Duration::from_secs_f32(CLIP_STEP / 2.0));
                        }
                        Some(Clip::Failed) => {
                            ui.label(
                                egui::RichText::new("미리보기를 만들 수 없습니다 (코덱이 없거나 깨진 파일)")
                                    .color(Color32::from_gray(160)),
                            );
                        }
                        _ => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("미리보기 만드는 중…");
                            });
                            ctx.request_repaint_after(Duration::from_millis(100));
                        }
                    }
                    ui.add(egui::Label::new(egui::RichText::new(&*n.name).strong()).truncate());
                    ui.label(
                        egui::RichText::new(format!("{}  ·  {}", fmt_size(n.size), fmt_filetime(n.mtime)))
                            .color(Color32::from_gray(160)),
                    );
                });
            });
    }

    // ---------------------------------------------------------------- toolbar

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        use icons::{tool_button, Glyph};
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let scanning = self.scan.is_some();
            for d in self.drives.clone() {
                let r = tool_button(ui, Glyph::Drive, &d.root[..2], !scanning, false).on_hover_text(format!(
                    "{} 스캔 ({})\n사용 가능 {} / 전체 {}",
                    &d.root[..2],
                    d.fs,
                    fmt_size(d.free),
                    fmt_size(d.total)
                ));
                if r.clicked() {
                    self.path = d.root.clone();
                    self.start_scan();
                }
            }
            ui.separator();
            let edit = ui.add_sized(
                [300.0, 26.0],
                egui::TextEdit::singleline(&mut self.path)
                    .hint_text("스캔할 경로")
                    .align(Align2::LEFT_CENTER),
            );
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !scanning {
                self.start_scan();
            }
            if tool_button(ui, Glyph::Tex(&self.icons.folder), "폴더", !scanning, false)
                .on_hover_text("스캔할 폴더 고르기")
                .clicked()
            {
                if let Some(p) = rfd::FileDialog::new().set_directory(&self.path).pick_folder() {
                    self.path = p.to_string_lossy().into_owned();
                    self.start_scan();
                }
            }
            if scanning {
                if tool_button(ui, Glyph::Stop, "중지", true, false).on_hover_text("스캔 멈추기").clicked() {
                    if let Some(s) = &self.scan {
                        s.progress.cancel.store(true, Relaxed);
                    }
                }
            } else if tool_button(ui, Glyph::Tex(&self.icons.scan), "스캔", true, false)
                .on_hover_text("경로 칸의 폴더를 스캔 (Enter)")
                .clicked()
            {
                self.start_scan();
            }
            ui.separator();
            if tool_button(ui, Glyph::Size, "크기", true, !self.use_alloc)
                .on_hover_text("파일 크기로 보기")
                .clicked()
            {
                self.use_alloc = false;
            }
            if tool_button(ui, Glyph::Alloc, "할당됨", true, self.use_alloc)
                .on_hover_text("디스크에 실제로 잡힌 공간으로 보기")
                .clicked()
            {
                self.use_alloc = true;
            }
            ui.separator();
            if tool_button(ui, Glyph::Changes, "변화", self.tree.is_some(), self.show_changes)
                .on_hover_text("지난번 스캔 대비 많이 커진 폴더")
                .clicked()
            {
                self.show_changes = !self.show_changes;
            }
            ui.separator();
            if self.admin {
                tool_button(ui, Glyph::Shield, "관리자", false, false)
                    .on_hover_text("관리자 권한으로 실행 중. MFT 고속 읽기를 쓸 수 있습니다");
            } else if tool_button(ui, Glyph::Shield, "관리자로", true, false)
                .on_hover_text("관리자로 다시 시작. MFT 고속 읽기는 관리자 권한이 필요합니다")
                .clicked()
            {
                let arg = format!("\"{}\"", crate::normalize_path(&self.path));
                if win::relaunch_as_admin(&arg) {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if tool_button(ui, Glyph::Tex(&self.icons.settings), "설정", true, self.show_settings)
                    .on_hover_text("설정, 프로그램 정보")
                    .clicked()
                {
                    self.show_settings = !self.show_settings;
                    if self.show_settings {
                        self.measure_cache(ui.ctx());
                        self.measure_history(ui.ctx());
                    }
                }
            });
        });
    }

    // ---------------------------------------------------------------- settings

    fn measure_cache(&self, ctx: &egui::Context) {
        let (usage, ctx) = (self.cache_usage.clone(), ctx.clone());
        usage.store(u64::MAX, Relaxed);
        std::thread::spawn(move || {
            usage.store(crate::cache::usage(), Relaxed);
            ctx.request_repaint();
        });
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        use egui::text::{LayoutJob, TextFormat};
        if !self.show_settings {
            return;
        }
        let muted = Color32::from_gray(150);
        let mut title = LayoutJob::default();
        title.append(
            "설정",
            0.0,
            TextFormat { font_id: FontId::proportional(17.0), color: Color32::from_gray(230), valign: Align::BOTTOM, ..Default::default() },
        );
        title.append(
            concat!("v", env!("CARGO_PKG_VERSION")),
            6.0,
            TextFormat { font_id: FontId::proportional(12.0), color: muted, valign: Align::BOTTOM, ..Default::default() },
        );
        let mut open = true;
        let shown = egui::Window::new(title)
            .id(egui::Id::unique("settings_window"))
            .vscroll(true)
            .min_height((ctx.content_rect().height() - 140.0).clamp(240.0, 760.0))
            .max_height((ctx.content_rect().height() - 120.0).max(240.0))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_width(380.0);
                ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
                egui::Frame::group(ui.style()).inner_margin(12).show(ui, |ui| {
                    ui.set_width(354.0);
                    ui.label(egui::RichText::new("스캔 방식").size(15.0).strong());
                    ui.separator();
                    ui.radio_value(&mut self.pref, Pref::Auto, "MFT 방식 (빠름)");
                    ui.add(egui::Label::new(egui::RichText::new(
                        "NTFS의 파일 목록 표(MFT)를 직접 읽습니다. 파일 250만 개 C드라이브에서 2.6 → 3.7초였습니다. \
                         관리자 권한과 NTFS가 필요하고, 안 되면 일반 방식으로 돌아갑니다.",
                    ).size(12.0).color(muted)).wrap());
                    ui.radio_value(&mut self.pref, Pref::Walk, "일반 방식");
                    ui.add(egui::Label::new(egui::RichText::new(
                        "폴더를 하나씩 엽니다. 같은 드라이브에서 24 → 27초였습니다. 권한 없는 폴더는 건너뜁니다.",
                    ).size(12.0).color(muted)).wrap());
                    ui.add_space(2.0);
                    let state = if self.admin { "지금 관리자 권한으로 실행 중입니다." } else { "지금은 관리자 권한이 없어 일반 방식으로 스캔합니다." };
                    ui.label(egui::RichText::new(state).size(12.0));
                });
                egui::Frame::group(ui.style()).inner_margin(12).show(ui, |ui| {
                    ui.set_width(354.0);
                    ui.label(egui::RichText::new("동영상 미리보기").size(15.0).strong());
                    ui.separator();
                    ui.checkbox(&mut self.video_preview, "동영상에서 10장을 뽑아 돌려 보여 주기");
                    ui.add(egui::Label::new(egui::RichText::new(
                        "표나 트리맵에서 동영상 파일 위에 마우스를 0.3초 올리면 옆에 뜨고, 0.7초마다 다음 장으로 넘어갑니다. \
                         Windows에 들어 있는 코덱으로 읽습니다.",
                    ).size(12.0).color(muted)).wrap());
                });
                egui::Frame::group(ui.style()).inner_margin(12).show(ui, |ui| {
                    use crate::cache::{MAX_MB, MIN_MB};
                    ui.set_width(354.0);
                    ui.label(egui::RichText::new("썸네일 캐시").size(15.0).strong());
                    ui.add(egui::Label::new(egui::RichText::new(
                        "동영상 미리보기 10장은 영상을 풀어 뽑아야 해서 한 편에 몇 초씩 걸립니다. \
                         그래서 한 번 만든 것을 디스크에 저장해 두고 다음에 다시 씁니다. \
                         시험한 영상 한 편이 38 KB였고 장면이 복잡하면 더 큽니다. 비워도 영상은 그대로이고 다음에 볼 때 다시 만듭니다.",
                    ).size(12.0).color(muted)).wrap());
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label("최대 크기");
                        ui.spacing_mut().slider_width = 170.0;
                        ui.add(egui::Slider::new(&mut self.cache_max_mb, MIN_MB..=MAX_MB).show_value(false));
                        ui.add(egui::DragValue::new(&mut self.cache_max_mb).range(MIN_MB..=MAX_MB).speed(5).suffix(" MB"))
                            .on_hover_text("클릭하여 직접 넣거나 좌우로 끌어 바꿉니다");
                    });
                    ui.horizontal(|ui| {
                        for (label, mb) in [("100MB", 100), ("300MB", 300), ("500MB", 500), ("1GB", 1024)] {
                            let b = egui::Button::new(label).selected(self.cache_max_mb == mb);
                            if ui.add_sized([80.0, 26.0], b).clicked() {
                                self.cache_max_mb = mb;
                            }
                        }
                    });
                    ui.add(egui::Label::new(egui::RichText::new(
                        "한도를 넘으면 다음 실행 때 오래 안 쓴 것부터 지워 한도의 80%로 줄입니다.",
                    ).size(12.0).color(muted)).wrap());
                    let limit = self.cache_max_mb * 1024 * 1024;
                    let used = self.cache_usage.load(Relaxed);
                    let busy = used == u64::MAX;
                    let ratio = if busy { 0.0 } else { (used as f32 / limit as f32).min(1.0) };
                    let over = !busy && used > limit;
                    let color = if over { Color32::from_rgb(220, 90, 70) } else { Color32::from_rgb(80, 150, 230) };
                    ui.label(egui::RichText::new(if busy {
                        "사용량 확인 중…".to_string()
                    } else {
                        format!(
                            "사용 중: {} / {}  ({:.0}%){}",
                            fmt_size(used),
                            fmt_size(limit),
                            used as f32 / limit as f32 * 100.0,
                            if over { "  한도 초과" } else { "" }
                        )
                    }).color(if busy { muted } else { color }));
                    ui.add(egui::ProgressBar::new(ratio).desired_width(354.0).desired_height(8.0).fill(color));
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(!busy, egui::Button::new("지금 줄이기"))
                            .on_hover_text("한도를 넘었을 때만 80%로 줄입니다")
                            .clicked()
                        {
                            let (usage, c) = (self.cache_usage.clone(), ctx.clone());
                            usage.store(u64::MAX, Relaxed);
                            std::thread::spawn(move || {
                                crate::cache::trim(limit);
                                usage.store(crate::cache::usage(), Relaxed);
                                c.request_repaint();
                            });
                        }
                        if ui
                            .add_enabled(!busy, egui::Button::new(egui::RichText::new("모두 비우기").color(Color32::from_rgb(230, 110, 95))))
                            .clicked()
                        {
                            self.message = Some(match crate::cache::clear() {
                                Ok(()) => "썸네일 캐시를 비웠습니다".into(),
                                Err(e) => format!("캐시 삭제 실패: {e}"),
                            });
                            self.measure_cache(ctx);
                        }
                        if ui.add(egui::Button::image_and_text((self.icons.folder.id(), Vec2::splat(16.0)), "폴더 열기")).clicked() {
                            if let Some(d) = crate::cache::dir() {
                                let _ = std::process::Command::new("explorer").arg(d).spawn();
                            }
                        }
                    });
                    if let Some(d) = crate::cache::dir() {
                        ui.add(egui::Label::new(egui::RichText::new(d.display().to_string()).size(12.0).color(muted)).wrap());
                    }
                });
                egui::Frame::group(ui.style()).inner_margin(12).show(ui, |ui| {
                    ui.set_width(354.0);
                    ui.label(egui::RichText::new("스캔 기록 (지난번 대비)").size(15.0).strong());
                    ui.add(egui::Label::new(egui::RichText::new(
                        "스캔이 끝날 때마다 폴더 크기를 파일로 남겨, 다음 스캔 때 많이 커진 폴더를 찾습니다. \
                         깊이를 늘리고 최소 크기를 줄이면 더 자세해지지만 기록 파일이 커집니다.",
                    ).size(12.0).color(muted)).wrap());
                    ui.separator();
                    ui.checkbox(&mut self.history_on, "스캔할 때마다 기록 남기기");
                    ui.add_enabled_ui(self.history_on, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("폴더 깊이");
                            ui.add(egui::Slider::new(&mut self.history_depth, 1..=12).suffix(" 단계"));
                        });
                        ui.horizontal(|ui| {
                            ui.label("최소 크기");
                            ui.add(egui::DragValue::new(&mut self.history_min_mb).range(1..=10_240).speed(1).suffix(" MB"));
                            for mb in [1u64, 10, 100] {
                                let b = egui::Button::new(format!("{mb}MB")).selected(self.history_min_mb == mb);
                                if ui.add(b).clicked() {
                                    self.history_min_mb = mb;
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("보관 개수");
                            ui.add(egui::DragValue::new(&mut self.history_keep).range(1..=200).speed(1).suffix(" 개"));
                            ui.label(egui::RichText::new("경로마다, 오래된 것부터 지움").size(12.0).color(muted));
                        });
                        // what one snapshot costs with these settings, on the tree on screen
                        if let Some(t) = &self.tree {
                            let k = (self.generation, self.history_depth, self.history_min_mb);
                            if self.history_estimate.map(|e| e.0) != Some(k) {
                                let entries = crate::history::collect(t, self.history_detail());
                                let bytes = entries.iter().map(|e| e.0.len() as u64 + 24).sum::<u64>() + 120;
                                self.history_estimate = Some((k, (entries.len(), bytes)));
                            }
                            let (n, b) = self.history_estimate.unwrap().1;
                            ui.label(egui::RichText::new(format!(
                                "지금 화면의 {} 기준: 한 번에 폴더 {}개, 약 {}. 다 차면 {}",
                                t.root_path,
                                fmt_count(n as u64),
                                fmt_size(b),
                                fmt_size(b * self.history_keep as u64)
                            )).size(12.0));
                        }
                    });
                    let used = self.history_usage.load(Relaxed);
                    ui.horizontal(|ui| {
                        ui.label(if used == u64::MAX {
                            "기록 사용량 확인 중…".to_string()
                        } else {
                            format!("기록 전체: {}", fmt_size(used))
                        });
                        if ui
                            .add_enabled(used != u64::MAX, egui::Button::new(egui::RichText::new("모두 지우기").color(Color32::from_rgb(230, 110, 95))))
                            .clicked()
                        {
                            self.message = Some(match crate::history::clear() {
                                Ok(()) => "스캔 기록을 모두 지웠습니다".into(),
                                Err(e) => format!("기록 삭제 실패: {e}"),
                            });
                            self.last_snapshot = None;
                            self.changes = None;
                            self.measure_history(ctx);
                        }
                        if ui.add(egui::Button::image_and_text((self.icons.folder.id(), Vec2::splat(16.0)), "폴더 열기")).clicked() {
                            if let Some(d) = crate::history::base_dir() {
                                let _ = std::fs::create_dir_all(&d);
                                let _ = std::process::Command::new("explorer").arg(d).spawn();
                            }
                        }
                    });
                });
            });
        // "About" sits in the title bar, left of the close button (as in oddity-picview)
        if let Some(w) = shown {
            let at = egui::pos2(w.response.rect.right() - 92.0, w.response.rect.top() + 9.0);
            egui::Area::new(egui::Id::unique("settings_about"))
                .order(egui::Order::Foreground)
                .fixed_pos(at)
                .show(ctx, |ui| {
                    if ui
                        .add(egui::Button::new(egui::RichText::new("About").size(12.5)).corner_radius(4.0))
                        .on_hover_text("프로그램 정보")
                        .clicked()
                    {
                        self.show_about = true;
                    }
                });
        }
        // Esc closes the About window first, then the settings window
        let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        self.show_settings = open && !(esc && !self.show_about);
    }

    fn about_window(&mut self, ctx: &egui::Context) {
        if !self.show_about {
            return;
        }
        let muted = Color32::from_gray(150);
        let mut open = true;
        egui::Window::new(egui::RichText::new("About").size(17.0))
            .id(egui::Id::unique("about_window"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.set_width(300.0);
                ui.vertical_centered(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(48.0), Sense::hover());
                    icons::paint_glyph(ui.painter(), icons::Glyph::Drive, r, true);
                    ui.label(egui::RichText::new("oddity-diskmap").size(20.0).strong());
                    ui.label(egui::RichText::new(format!("버전 {}", env!("CARGO_PKG_VERSION"))).color(muted));
                });
                ui.add_space(6.0);
                ui.separator();
                egui::Grid::new("about_grid").num_columns(2).spacing(Vec2::new(16.0, 8.0)).show(ui, |ui| {
                    ui.label(egui::RichText::new("만든 사람").color(muted));
                    ui.label(AUTHOR);
                    ui.end_row();
                    ui.label(egui::RichText::new("GitHub").color(muted));
                    ui.hyperlink_to(REPO_URL.trim_start_matches("https://"), REPO_URL)
                        .on_hover_text("브라우저로 엽니다");
                    ui.end_row();
                    ui.label(egui::RichText::new("라이선스").color(muted));
                    ui.label("MIT");
                    ui.end_row();
                });
            });
        if !open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.show_about = false;
        }
    }

    // ---------------------------------------------------------------- status

    fn status(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some((free, total)) = self.free_space {
                ui.label(format!("사용 가능한 공간: {} (/ {})", fmt_size(free), fmt_size(total)));
                ui.separator();
            }
            if let Some(t) = &self.tree {
                let r = &t.nodes[t.root as usize];
                ui.label(format!("파일 {}개, 폴더 {}개", fmt_count(r.files), fmt_count(r.dirs)));
                ui.separator();
                if t.errors > 0 {
                    ui.colored_label(Color32::from_rgb(230, 100, 90), format!("{} 오류", fmt_count(t.errors)))
                        .on_hover_text("권한이 없어 열지 못한 폴더 수");
                    ui.separator();
                }
                let m = match t.method {
                    Method::Mft => "MFT",
                    Method::Walk => "일반",
                };
                ui.label(format!("{m} 방식, {:.2}초", t.elapsed.as_secs_f64()));
                ui.separator();
                ui.label(format!("클러스터 {} 바이트", fmt_count(t.cluster_size)));
                if let Some(s) = self.selected {
                    ui.separator();
                    ui.label(t.path_of(s));
                }
            }
            if let Some(m) = &self.message {
                ui.separator();
                ui.colored_label(Color32::from_rgb(230, 180, 80), m);
            }
        });
    }

    fn progress_view(&self, ui: &mut egui::Ui) {
        let Some(s) = &self.scan else { return };
        let p = &s.progress;
        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            let m = if s.method == Method::Mft { "MFT를 읽는 중" } else { "폴더를 스캔하는 중" };
            ui.heading(format!("{m}… {}", self.path));
            ui.add_space(12.0);
            ui.label(format!(
                "파일 {}개, 폴더 {}개, {:.1}초",
                fmt_count(p.files.load(Relaxed)),
                fmt_count(p.dirs.load(Relaxed)),
                s.start.elapsed().as_secs_f64()
            ));
            let total = p.bytes_total.load(Relaxed);
            if total > 0 {
                let done = p.bytes_done.load(Relaxed);
                ui.add(
                    egui::ProgressBar::new(done as f32 / total as f32)
                        .desired_width(400.0)
                        .text(format!("{} / {}", fmt_size(done), fmt_size(total))),
                );
            } else {
                ui.spinner();
            }
        });
    }

    // ---------------------------------------------------------------- table

    fn table(&mut self, ui: &mut egui::Ui) -> Vec<Action> {
        let mut actions = Vec::new();
        let Some(t) = &self.tree else {
            ui.centered_and_justified(|ui| {
                ui.label("드라이브 단추를 누르거나 경로를 넣고 스캔을 누르십시오.");
            });
            return actions;
        };
        let use_alloc = self.use_alloc;
        let sort = self.sort;
        let rows = &self.rows;
        let expanded = &self.expanded;
        let selected = self.selected;

        let mut tb = TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::initial(460.0).at_least(160.0).clip(true))
            .column(Column::initial(84.0).at_least(50.0))
            .column(Column::initial(84.0).at_least(50.0))
            .column(Column::initial(84.0).at_least(40.0))
            .column(Column::initial(74.0).at_least(40.0))
            .column(Column::initial(100.0).at_least(60.0))
            .column(Column::remainder().at_least(70.0));
        if let Some(r) = self.scroll_to.take() {
            tb = tb.scroll_to_row(r, Some(Align::Center));
        }

        let headers = [
            ("이름", Some(SortKey::Name)),
            ("크기", Some(SortKey::Size)),
            ("할당됨", Some(SortKey::Alloc)),
            ("파일", Some(SortKey::Files)),
            ("폴더", Some(SortKey::Dirs)),
            ("% 상위 폴더", None),
            ("수정한 날짜", Some(SortKey::Mtime)),
        ];
        tb.header(24.0, |mut h| {
            for (title, key) in headers {
                h.col(|ui| {
                    let mut text = title.to_string();
                    if key == Some(sort.0) {
                        text.push_str(if sort.1 { " ▼" } else { " ▲" });
                    }
                    let r = ui.add(egui::Label::new(egui::RichText::new(text).strong()).sense(Sense::click()));
                    if let Some(k) = key {
                        if r.clicked() {
                            actions.push(Action::Sort(k));
                        }
                    }
                });
            }
        })
        .body(|body| {
            body.rows(22.0, rows.len(), |mut row| {
                let (idx, depth) = rows[row.index()];
                let n = &t.nodes[idx as usize];
                row.set_selected(selected == Some(idx));
                let value = if use_alloc { n.alloc } else { n.size };
                let parent_value = if n.parent != NO_PARENT && idx != t.root {
                    t.value(n.parent, use_alloc)
                } else {
                    value
                };
                let share = if parent_value > 0 { value as f32 / parent_value as f32 } else { 0.0 };

                // name column
                row.col(|ui| {
                    ui.add_space(depth as f32 * 14.0);
                    let (ar, aresp) = ui.allocate_exact_size(Vec2::new(14.0, 14.0), Sense::click());
                    if n.is_dir && !n.children.is_empty() {
                        let c = ar.center();
                        let col = ui.visuals().text_color();
                        let pts = if expanded[idx as usize] {
                            vec![c + Vec2::new(-4.0, -2.0), c + Vec2::new(4.0, -2.0), c + Vec2::new(0.0, 3.0)]
                        } else {
                            vec![c + Vec2::new(-2.0, -4.0), c + Vec2::new(3.0, 0.0), c + Vec2::new(-2.0, 4.0)]
                        };
                        ui.painter().add(egui::Shape::convex_polygon(pts, col, Stroke::NONE));
                        if aresp.clicked() {
                            actions.push(Action::Toggle(idx));
                        }
                    }
                    // size badge with share-of-parent fill
                    let (br, _) = ui.allocate_exact_size(Vec2::new(78.0, 18.0), Sense::hover());
                    let p = ui.painter();
                    p.rect_filled(br, 2.0, Color32::from_gray(50));
                    let mut fill = br;
                    fill.set_width(br.width() * share.clamp(0.0, 1.0));
                    p.rect_filled(fill, 2.0, Color32::from_gray(95));
                    p.text(
                        br.right_center() - Vec2::new(4.0, 0.0),
                        Align2::RIGHT_CENTER,
                        fmt_size(value),
                        FontId::proportional(12.5),
                        Color32::WHITE,
                    );
                    // icon
                    let (ir, _) = ui.allocate_exact_size(Vec2::new(16.0, 14.0), Sense::hover());
                    let p = ui.painter();
                    if n.is_dir {
                        let body = Rect::from_min_max(ir.min + Vec2::new(1.0, 3.0), ir.max - Vec2::new(1.0, 1.0));
                        let tab = Rect::from_min_size(ir.min + Vec2::new(1.0, 1.0), Vec2::new(6.0, 3.0));
                        p.rect_filled(tab, 1.0, Color32::from_rgb(230, 180, 60));
                        p.rect_filled(body, 1.0, Color32::from_rgb(240, 196, 80));
                    } else {
                        let body = Rect::from_min_max(ir.min + Vec2::new(3.0, 0.0), ir.max - Vec2::new(3.0, 0.0));
                        p.rect_filled(body, 1.0, Color32::from_gray(225));
                    }
                    let name = if idx == t.root { t.root_path.as_str() } else { &n.name };
                    let text = if n.is_dir && depth <= 1 {
                        egui::RichText::new(name).strong()
                    } else {
                        egui::RichText::new(name)
                    };
                    ui.add(egui::Label::new(text).selectable(false).truncate());
                });
                let right = |ui: &mut egui::Ui, s: String| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(s);
                    });
                };
                row.col(|ui| right(ui, fmt_size(n.size)));
                row.col(|ui| right(ui, fmt_size(n.alloc)));
                row.col(|ui| right(ui, fmt_count(n.files)));
                row.col(|ui| right(ui, if n.is_dir { fmt_count(n.dirs) } else { String::new() }));
                row.col(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 18.0), Sense::hover());
                    let p = ui.painter();
                    p.rect_filled(r, 0.0, Color32::from_gray(40));
                    let mut f = r;
                    f.set_width(r.width() * share.clamp(0.0, 1.0));
                    p.rect_filled(f, 0.0, BAR_COLOR);
                    p.text(r.center(), Align2::CENTER_CENTER, format!("{:.1} %", share * 100.0),
                        FontId::proportional(12.5), Color32::WHITE);
                });
                row.col(|ui| {
                    ui.label(fmt_filetime(n.mtime));
                });

                let resp = row.response();
                if resp.hovered() && !n.is_dir && crate::video::is_video(&n.name) {
                    actions.push(Action::HoverVideo(idx));
                }
                if resp.clicked() {
                    actions.push(Action::Select(idx));
                }
                if resp.double_clicked() {
                    if n.is_dir {
                        actions.push(Action::Toggle(idx));
                    } else if crate::video::is_media(&n.name) {
                        actions.push(Action::Open(idx));
                    }
                }
                resp.context_menu(|ui| {
                    if n.is_dir && ui.button("트리맵에서 보기").clicked() {
                        actions.push(Action::MapRoot(idx));
                        ui.close();
                    }
                    if !n.is_dir && crate::video::is_media(&n.name) && ui.button("열기").clicked() {
                        actions.push(Action::Open(idx));
                        ui.close();
                    }
                    if ui.button("탐색기에서 열기").clicked() {
                        actions.push(Action::Explorer(idx));
                        ui.close();
                    }
                    if ui.button("경로 복사").clicked() {
                        actions.push(Action::Copy(idx));
                        ui.close();
                    }
                });
            });
        });
        actions
    }

    // ---------------------------------------------------------------- treemap

    fn treemap(&mut self, ui: &mut egui::Ui) -> Vec<Action> {
        let mut actions = Vec::new();
        let Some(t) = &self.tree else { return actions };

        ui.horizontal(|ui| {
            let chain = t.chain(self.map_root);
            if ui
                .add_enabled(self.map_root != t.root, egui::Button::new("⬆ 위로"))
                .clicked()
            {
                actions.push(Action::MapRoot(t.nodes[self.map_root as usize].parent));
            }
            for (k, &c) in chain.iter().enumerate() {
                if k > 0 {
                    ui.label("›");
                }
                let name = if c == t.root { t.root_path.as_str() } else { &t.nodes[c as usize].name };
                if ui.link(name).clicked() {
                    actions.push(Action::MapRoot(c));
                }
            }
            ui.label(format!("({})", fmt_size(t.value(self.map_root, self.use_alloc))));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                for d in (0..7u16).rev() {
                    let (r, _) = ui.allocate_exact_size(Vec2::new(54.0, 16.0), Sense::hover());
                    ui.painter().rect_filled(r, 2.0, level_color(d));
                    ui.painter().text(r.center(), Align2::CENTER_CENTER, format!("수준 {d}"),
                        FontId::proportional(11.0), Color32::WHITE);
                }
            });
        });

        let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click());
        if rect.width() < 10.0 || rect.height() < 10.0 {
            return actions;
        }
        let free = if self.map_root == t.root { self.free_space.map(|f| f.0) } else { None };
        let valid = matches!(&self.layout_cache,
            Some((g, root, r, a, _)) if *g == self.generation && *root == self.map_root && *r == rect && *a == self.use_alloc);
        if !valid {
            let items = treemap::layout(
                t,
                self.map_root,
                R { x: rect.min.x, y: rect.min.y, w: rect.width(), h: rect.height() },
                self.use_alloc,
                free,
            );
            self.layout_cache = Some((self.generation, self.map_root, rect, self.use_alloc, items));
        }
        let items = &self.layout_cache.as_ref().unwrap().4;

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_gray(20));
        let font = FontId::proportional(12.0);
        for it in items {
            let r = to_rect(it.rect);
            let (fill, label) = if it.idx == FREE {
                (FREE_COLOR, format!("사용 가능한 공간 ({})", fmt_size(free.unwrap_or(0))))
            } else {
                let n = &t.nodes[it.idx as usize];
                let name = if it.idx == t.root { t.root_path.as_str() } else { &n.name };
                let fill = if n.is_dir { level_color(it.depth) } else { FILE_COLOR };
                (fill, format!("{name} ({})", fmt_size(t.value(it.idx, self.use_alloc))))
            };
            painter.rect_filled(r, 0.0, fill);
            if r.width() > 2.0 && r.height() > 2.0 {
                painter.rect_stroke(r, 0.0, Stroke::new(1.0, Color32::from_black_alpha(140)), StrokeKind::Inside);
            }
            let show_label = it.labeled
                || (it.idx != FREE && r.width() > 50.0 && r.height() > 16.0);
            if show_label {
                painter.with_clip_rect(r.shrink(1.0).intersect(rect)).text(
                    r.min + Vec2::new(4.0, 2.0),
                    Align2::LEFT_TOP,
                    label,
                    font.clone(),
                    Color32::WHITE,
                );
            }
        }

        // hover: deepest item under the pointer
        let hovered = resp
            .hover_pos()
            .and_then(|p| items.iter().rev().find(|it| it.rect.contains(p.x, p.y)).copied());
        if let Some(sel) = self.selected {
            if let Some(it) = items.iter().find(|it| it.idx == sel) {
                painter.rect_stroke(to_rect(it.rect), 0.0, Stroke::new(2.0, Color32::YELLOW), StrokeKind::Inside);
            }
        }
        if let Some(h) = hovered {
            painter.rect_stroke(to_rect(h.rect), 0.0, Stroke::new(1.5, Color32::WHITE), StrokeKind::Inside);
            let tip = if h.idx == FREE {
                format!("사용 가능한 공간\n{}", fmt_size(free.unwrap_or(0)))
            } else {
                let n = &t.nodes[h.idx as usize];
                format!(
                    "{}\n크기 {}  할당됨 {}\n파일 {}개{}",
                    t.path_of(h.idx),
                    fmt_size(n.size),
                    fmt_size(n.alloc),
                    fmt_count(n.files),
                    if n.is_dir { format!(", 폴더 {}개", fmt_count(n.dirs)) } else { String::new() }
                )
            };
            let video = h.idx != FREE
                && self.video_preview
                && !t.nodes[h.idx as usize].is_dir
                && crate::video::is_video(&t.nodes[h.idx as usize].name);
            if video {
                actions.push(Action::HoverVideo(h.idx));
            } else {
                resp.clone().on_hover_text_at_pointer(tip);
            }
            if h.idx != FREE {
                if resp.double_clicked() {
                    let n = &t.nodes[h.idx as usize];
                    if !n.is_dir && crate::video::is_media(&n.name) {
                        actions.push(Action::Open(h.idx));
                    } else {
                        let target = if n.is_dir { h.idx } else { n.parent };
                        if target != NO_PARENT {
                            actions.push(Action::MapRoot(target));
                        }
                    }
                } else if resp.clicked() {
                    actions.push(Action::Reveal(h.idx));
                }
            }
        }
        if resp.secondary_clicked() && self.map_root != t.root {
            actions.push(Action::MapRoot(t.nodes[self.map_root as usize].parent));
        }
        actions
    }
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &Saved {
            pref: self.pref,
            use_alloc: self.use_alloc,
            video_preview: self.video_preview,
            cache_max_mb: self.cache_max_mb,
            history_on: self.history_on,
            history_depth: self.history_depth,
            history_min_mb: self.history_min_mb,
            history_keep: self.history_keep,
        });
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_scan();
        if self.scan.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        let ctx = ui.ctx().clone();
        self.hover_now = None;

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(4.0);
            self.toolbar(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| self.status(ui));

        let mut actions = Vec::new();
        if self.scan.is_some() {
            egui::CentralPanel::default().show(ui, |ui| self.progress_view(ui));
        } else {
            if self.tree.is_some() {
                egui::Panel::bottom("treemap")
                    .resizable(true)
                    .default_size(340.0)
                    .min_size(120.0)
                    .show(ui, |ui| actions.extend(self.treemap(ui)));
            }
            egui::CentralPanel::default().show(ui, |ui| actions.extend(self.table(ui)));
        }
        self.changes_window(&ctx);
        self.settings_window(&ctx);
        self.about_window(&ctx);
        self.apply(actions, &ctx);
        self.video_preview_popup(&ctx);
    }
}
