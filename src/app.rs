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
use std::sync::mpsc::{channel, Receiver};
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
}

impl Default for Saved {
    fn default() -> Self {
        Saved { pref: Pref::Auto, use_alloc: false }
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
}

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

    // ---------------------------------------------------------------- toolbar

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        use icons::{tool_button, Glyph};
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let scanning = self.scan.is_some();
            for d in self.drives.clone() {
                let r = tool_button(ui, Glyph::Drive, &d.root[..2], !scanning, false).on_hover_text(format!(
                    "{} 훑기 ({})\n사용 가능 {} / 전체 {}",
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
                    .hint_text("훑을 경로")
                    .align(Align2::LEFT_CENTER),
            );
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !scanning {
                self.start_scan();
            }
            if tool_button(ui, Glyph::Tex(&self.icons.folder), "폴더", !scanning, false)
                .on_hover_text("훑을 폴더 고르기")
                .clicked()
            {
                if let Some(p) = rfd::FileDialog::new().set_directory(&self.path).pick_folder() {
                    self.path = p.to_string_lossy().into_owned();
                    self.start_scan();
                }
            }
            if scanning {
                if tool_button(ui, Glyph::Stop, "중지", true, false).on_hover_text("훑기 멈추기").clicked() {
                    if let Some(s) = &self.scan {
                        s.progress.cancel.store(true, Relaxed);
                    }
                }
            } else if tool_button(ui, Glyph::Tex(&self.icons.scan), "훑기", true, false)
                .on_hover_text("경로 칸의 폴더를 훑기 (Enter)")
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
                }
            });
        });
    }

    // ---------------------------------------------------------------- settings

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
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_width(380.0);
                ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
                egui::Frame::group(ui.style()).inner_margin(12).show(ui, |ui| {
                    ui.set_width(354.0);
                    ui.label(egui::RichText::new("훑는 방식").size(15.0).strong());
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
                    let state = if self.admin { "지금 관리자 권한으로 실행 중입니다." } else { "지금은 관리자 권한이 없어 일반 방식으로 훑습니다." };
                    ui.label(egui::RichText::new(state).size(12.0));
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
            let m = if s.method == Method::Mft { "MFT를 읽는 중" } else { "폴더를 훑는 중" };
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
                ui.label("드라이브 단추를 누르거나 경로를 넣고 ▶ 훑기를 누르십시오.");
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
                if resp.clicked() {
                    actions.push(Action::Select(idx));
                }
                if resp.double_clicked() && n.is_dir {
                    actions.push(Action::Toggle(idx));
                }
                resp.context_menu(|ui| {
                    if n.is_dir && ui.button("트리맵에서 보기").clicked() {
                        actions.push(Action::MapRoot(idx));
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
            resp.clone().on_hover_text_at_pointer(tip);
            if h.idx != FREE {
                if resp.double_clicked() {
                    let n = &t.nodes[h.idx as usize];
                    let target = if n.is_dir { h.idx } else { n.parent };
                    if target != NO_PARENT {
                        actions.push(Action::MapRoot(target));
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
        eframe::set_value(storage, eframe::APP_KEY, &Saved { pref: self.pref, use_alloc: self.use_alloc });
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_scan();
        if self.scan.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        let ctx = ui.ctx().clone();

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
        self.settings_window(&ctx);
        self.about_window(&ctx);
        self.apply(actions, &ctx);
    }
}
