//! GUI：参照 Internet Download Manager 6.x 的经典布局重绘。
//!
//! 结构：
//! - 菜单栏：任务 / 文件 / 下载 / 视图 / 帮助
//! - 大图标工具栏：图标在上、文字在下（IDM 风格）
//! - 左侧分类树：全部下载（压缩包/文档/音乐/程序/视频）、未完成、已完成、BT、失败
//! - 右侧文件列表：文件名 / 大小 / 状态 / 剩余时间 / 传输速率（迷你折线）/ 上次尝试 / 描述
//! - 底部状态栏 + 可展开的日志面板
//! - 弹窗：添加任务、参数设置、关于、剪贴板链接确认

mod filelist;
mod icons;
mod sidebar;
mod theme;
mod toolbar;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Align, Color32, Layout, RichText, Stroke};

use crate::common::clipboard::{self, ClipboardWatcher};
use crate::common::task::{Config, LogLevel, SortKey, Source, Task, TaskState};
use crate::common::util::human_rate;
use crate::engine::Engine;

pub use sidebar::Category;
use theme::{Metrics, Theme};

/// 系统中文字体的候选路径（按优先级）。
pub const CJK_FONT_CANDIDATES: &[&str] = &[
    "C:\\Windows\\Fonts\\msyh.ttc",
    "C:\\Windows\\Fonts\\msyhh.ttf",
    "C:\\Windows\\Fonts\\msyhl.ttc",
    "C:\\Windows\\Fonts\\Deng.ttf",
    "C:\\Windows\\Fonts\\simhei.ttf",
    "C:\\Windows\\Fonts\\simsun.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/System/Library/Fonts/PingFang.ttc",
    "/Library/Fonts/Arial Unicode.ttf",
];

/// 找到第一个可读的候选中文字体，返回 (字体名, 文件字节)。
pub fn load_cjk_font() -> Option<(String, Vec<u8>)> {
    for path in CJK_FONT_CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            if !bytes.is_empty() {
                let name = path
                    .rsplit(['\\', '/'])
                    .next()
                    .unwrap_or(path)
                    .to_string();
                return Some((name, bytes));
            }
        }
    }
    None
}

/// 把中文字体装进 egui 的字体库（必须，否则界面中文是方块）。
pub fn setup_fonts(ctx: &egui::Context) {
    let Some((name, bytes)) = load_cjk_font() else {
        eprintln!(
            "[rust-dm] 未找到中文字体，界面中文可能显示为方块。\
             请安装微软雅黑/黑体等含 CJK 字形的字体。"
        );
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert(name.clone(), Arc::new(egui::FontData::from_owned(bytes)));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, name.clone());
    }
    ctx.set_fonts(fonts);
}

/// 应用层状态。
pub struct App {
    engine: Arc<Engine>,
    theme: Theme,

    // 列表状态
    selected: Option<u64>,
    category: Category,
    tree_expanded: bool,
    sort_key: SortKey,
    sort_asc: bool,
    /// 每个任务最近若干次速度采样，用于迷你折线图
    speed_hist: HashMap<u64, VecDeque<u32>>,
    last_sample: Instant,

    // 弹窗
    add_open: bool,
    add_url: String,
    add_dir: String,
    add_start_now: bool,
    add_error: Option<String>,

    opts_open: bool,
    cfg_draft: Config,

    log_open: bool,
    about_open: bool,

    // 剪贴板监听
    clipboard: ClipboardWatcher,
    clip_prompt: Option<String>,
    clip_ignored: HashSet<String>,
}

impl App {
    pub fn new(engine: Arc<Engine>) -> Self {
        let cfg = engine.config();
        Self {
            engine,
            theme: Theme::default(),
            selected: None,
            category: Category::All,
            tree_expanded: true,
            sort_key: SortKey::Name,
            sort_asc: true,
            speed_hist: HashMap::new(),
            last_sample: Instant::now(),
            add_open: false,
            add_url: String::new(),
            add_dir: cfg.download_dir.to_string_lossy().to_string(),
            add_start_now: true,
            add_error: None,
            opts_open: false,
            cfg_draft: cfg.clone(),
            log_open: false,
            about_open: false,
            clipboard: ClipboardWatcher::spawn(true),
            clip_prompt: None,
            clip_ignored: HashSet::new(),
        }
    }

    /// 读一次任务快照。
    fn snapshot_tasks(&self) -> Vec<Task> {
        let s = self.engine.state.lock().unwrap();
        s.tasks
            .iter()
            .filter_map(|t| t.lock().ok().map(|t| t.clone()))
            .collect()
    }

    /// 采样速度，供迷你折线使用。每 500ms 一次。
    fn sample_speeds(&mut self, tasks: &[Task]) {
        if self.last_sample.elapsed() < Duration::from_millis(450) {
            return;
        }
        self.last_sample = Instant::now();
        for t in tasks {
            let h = self.speed_hist.entry(t.id).or_default();
            h.push_back(t.speed_down.min(u32::MAX as u64) as u32);
            while h.len() > 40 {
                h.pop_front();
            }
        }
        // 清理已删除任务的历史
        let alive: HashSet<u64> = tasks.iter().map(|t| t.id).collect();
        self.speed_hist.retain(|id, _| alive.contains(id));
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.apply_style(ctx);
        self.poll_clipboard(ctx);
        self.handle_dropped_files(ctx);
        self.shortcuts(ctx);

        // ---- 面板（顺序决定布局）----
        self.menu_bar(ctx);
        self.toolbar(ctx);
        self.status_bar(ctx);
        if self.log_open {
            self.log_panel(ctx);
        }

        egui::SidePanel::left("categories")
            .exact_width(Metrics::SIDEBAR_W)
            .resizable(false)
            .frame(egui::Frame::NONE.fill(self.theme.sidebar_bg))
            .show(ctx, |ui| {
                self.sidebar(ui);
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(self.theme.list_bg))
            .show(ctx, |ui| {
                self.file_list(ui);
            });

        // ---- 弹窗 ----
        self.add_dialog(ctx);
        self.options_dialog(ctx);
        self.about_dialog(ctx);
        self.clipboard_prompt(ctx);

        ctx.request_repaint_after(Duration::from_millis(400));
    }
}

impl App {
    /// 统一把 egui 的默认样式调成 IDM 的观感。
    fn apply_style(&mut self, ctx: &egui::Context) {
        let mut style = (*ctx.style()).clone();
        let t = self.theme;
        style.visuals.panel_fill = t.window_bg;
        style.visuals.window_fill = t.menu_bg;
        style.visuals.extreme_bg_color = t.list_bg;
        style.visuals.faint_bg_color = t.header_bg;
        style.visuals.widgets.noninteractive.bg_fill = t.window_bg;
        style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(240, 240, 240);
        style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(190, 190, 190));
        style.visuals.widgets.hovered.bg_fill = t.btn_hover;
        style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, t.btn_hover_border);
        style.visuals.widgets.active.bg_fill = t.btn_active;
        style.visuals.widgets.active.bg_stroke = Stroke::new(1.0, t.btn_active_border);
        // 弹窗标题栏用的是 widgets.open.weak_bg_fill，默认是深灰，必须改浅
        style.visuals.widgets.open.weak_bg_fill = t.header_bg;
        style.visuals.widgets.open.bg_fill = t.header_bg;
        style.visuals.widgets.open.bg_stroke = Stroke::new(1.0, t.header_border);
        style.visuals.widgets.open.fg_stroke = Stroke::new(1.0, t.text);
        style.visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, t.text);
        style.visuals.selection.bg_fill = t.row_selected;
        style.visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);
        style.visuals.hyperlink_color = t.accent;
        style.visuals.warn_fg_color = t.warn;
        style.visuals.error_fg_color = t.err;
        style.visuals.window_stroke = Stroke::new(1.0, Color32::from_rgb(150, 160, 170));
        style.visuals.window_corner_radius = egui::CornerRadius::same(3);
        style.spacing.item_spacing = egui::vec2(6.0, 4.0);
        style.spacing.button_padding = egui::vec2(8.0, 3.0);
        ctx.set_style(style);
    }

    // ---------- 菜单栏 ----------
    fn menu_bar(&mut self, ctx: &egui::Context) {
        let t = self.theme;
        egui::TopBottomPanel::top("menu_bar")
            .exact_height(Metrics::MENU_H)
            .frame(
                egui::Frame::NONE
                    .fill(t.menu_bg)
                    .inner_margin(egui::Margin::symmetric(4, 1)),
            )
            .show(ctx, |ui| {
                egui::menu::bar(ui, |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;

                    ui.menu_button(RichText::new("任务(T)").size(Metrics::FONT_MENU), |ui| {
                        if ui.button("新建任务…").clicked() {
                            self.add_open = true;
                            ui.close_menu();
                        }
                        if ui.button("从剪贴板添加").clicked() {
                            self.add_from_clipboard();
                            ui.close_menu();
                        }
                        if ui.button("打开种子文件…").clicked() {
                            self.open_torrent_picker();
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("退出").clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                    ui.menu_button(RichText::new("文件(F)").size(Metrics::FONT_MENU), |ui| {
                        if ui.button("打开种子文件…").clicked() {
                            self.open_torrent_picker();
                            ui.close_menu();
                        }
                        if ui.button("打开下载目录").clicked() {
                            open_folder(&self.engine.config().download_dir);
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("参数设置…").clicked() {
                            self.cfg_draft = self.engine.config();
                            self.opts_open = true;
                            ui.close_menu();
                        }
                    });
                    ui.menu_button(RichText::new("下载(D)").size(Metrics::FONT_MENU), |ui| {
                        if ui.button("开始").clicked() {
                            if let Some(id) = self.selected {
                                self.engine.resume_task(id);
                            }
                            ui.close_menu();
                        }
                        if ui.button("暂停").clicked() {
                            if let Some(id) = self.selected {
                                self.engine.pause_task(id);
                            }
                            ui.close_menu();
                        }
                        if ui.button("打开所在目录").clicked() {
                            if let Some(task) = self.selected_task() {
                                open_folder(&task.save_dir);
                            }
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("全部开始").clicked() {
                            self.engine.resume_all();
                            ui.close_menu();
                        }
                        if ui.button("全部暂停").clicked() {
                            self.engine.pause_all();
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("移除任务").clicked() {
                            if let Some(id) = self.selected {
                                self.engine.remove_task(id);
                                self.selected = None;
                            }
                            ui.close_menu();
                        }
                        if ui.button("清除已完成").clicked() {
                            self.engine.clear_finished();
                            ui.close_menu();
                        }
                    });
                    ui.menu_button(RichText::new("视图(V)").size(Metrics::FONT_MENU), |ui| {
                        if ui.checkbox(&mut self.log_open, "显示日志面板").changed() {
                            ui.close_menu();
                        }
                        if ui.checkbox(&mut self.tree_expanded, "展开分类").changed() {
                            ui.close_menu();
                        }
                        ui.separator();
                        let mut watch = self.clipboard.enabled();
                        if ui.checkbox(&mut watch, "监视剪贴板中的下载链接").changed() {
                            self.clipboard.set_enabled(watch);
                        }
                    });
                    ui.menu_button(RichText::new("帮助(H)").size(Metrics::FONT_MENU), |ui| {
                        if ui.button("关于 rust-dm").clicked() {
                            self.about_open = true;
                            ui.close_menu();
                        }
                    });
                });
            });
    }

    // ---------- 底部状态栏 ----------
    fn status_bar(&mut self, ctx: &egui::Context) {
        let t = self.theme;
        let (down, up, active, peers) = self.engine.global_stats();
        egui::TopBottomPanel::bottom("status_bar")
            .exact_height(Metrics::STATUS_H)
            .frame(
                egui::Frame::NONE
                    .fill(t.status_bg)
                    .inner_margin(egui::Margin::symmetric(8, 2)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 12.0;
                    ui.label(
                        RichText::new(format!("↓ {}", human_rate(down as f64)))
                            .size(Metrics::FONT_STATUS)
                            .color(t.info)
                            .strong(),
                    );
                    ui.label(
                        RichText::new(format!("↑ {}", human_rate(up as f64)))
                            .size(Metrics::FONT_STATUS)
                            .color(t.ok)
                            .strong(),
                    );
                    ui.label(
                        RichText::new(format!("活动任务 {active}"))
                            .size(Metrics::FONT_STATUS)
                            .color(t.text_dim),
                    );
                    ui.label(
                        RichText::new(format!("连接 {peers}"))
                            .size(Metrics::FONT_STATUS)
                            .color(t.text_dim),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let cfg = self.engine.config();
                        let limit = if cfg.speed_limit_down == 0 {
                            "不限速".to_string()
                        } else {
                            format!("限速 {} KB/s", cfg.speed_limit_down / 1024)
                        };
                        ui.label(
                            RichText::new(limit)
                                .size(Metrics::FONT_STATUS)
                                .color(t.text_dim),
                        );
                        if self.clipboard.enabled() {
                            ui.label(
                                RichText::new("剪贴板监视：开")
                                    .size(Metrics::FONT_STATUS)
                                    .color(t.text_dim),
                            );
                        }
                    });
                });
            });
    }

    // ---------- 日志面板 ----------
    fn log_panel(&mut self, ctx: &egui::Context) {
        let t = self.theme;
        let mut open = self.log_open;
        egui::TopBottomPanel::bottom("log_panel")
            .exact_height(150.0)
            .frame(
                egui::Frame::NONE
                    .fill(Color32::WHITE)
                    .stroke(Stroke::new(1.0, t.header_border))
                    .inner_margin(egui::Margin::symmetric(6, 3)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("运行日志")
                            .size(Metrics::FONT_HEADER)
                            .strong()
                            .color(t.text),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("关闭").clicked() {
                            open = false;
                        }
                    });
                });
                ui.separator();
                let msgs: Vec<(LogLevel, String)> = {
                    let s = self.engine.state.lock().unwrap();
                    s.logs
                        .entries
                        .iter()
                        .rev()
                        .take(60)
                        .map(|e| (e.level, e.message.clone()))
                        .collect()
                };
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(false)
                    .show(ui, |ui| {
                        for (level, msg) in msgs.iter().rev() {
                            let col = match level {
                                LogLevel::Info => t.text,
                                LogLevel::Warn => t.warn,
                                LogLevel::Error => t.err,
                            };
                            ui.label(
                                RichText::new(format!("[{}] {}", level.label(), msg))
                                    .size(Metrics::FONT_STATUS)
                                    .color(col),
                            );
                        }
                    });
            });
        self.log_open = open;
    }

    // ---------- 公用小工具 ----------
    fn selected_task(&self) -> Option<Task> {
        let id = self.selected?;
        self.snapshot_tasks().into_iter().find(|t| t.id == id)
    }

    fn add_from_clipboard(&mut self) {
        match clipboard::read_now().and_then(|t| clipboard::detect_download_link(&t)) {
            Some(link) => {
                self.add_url = link;
                self.add_dir = self.engine.config().download_dir.to_string_lossy().to_string();
                self.add_error = None;
                self.add_open = true;
            }
            None => {
                self.engine
                    .log_info("剪贴板里没有识别到下载链接".to_string());
            }
        }
    }

    fn start_from_link(&mut self, link: &str) {
        let cfg = self.engine.config();
        match Source::parse(link, &cfg.ftp_user, &cfg.ftp_password) {
            Ok(src) => {
                let dir = cfg.download_dir.clone();
                if !dir.exists() {
                    if let Err(e) = std::fs::create_dir_all(&dir) {
                        self.engine
                            .log_error(format!("无法创建目录 {}：{e}", dir.display()));
                        return;
                    }
                }
                self.engine.add_task(src, dir);
                self.engine
                    .log_info(format!("从剪贴板添加任务：{}", shorten(link, 80)));
            }
            Err(e) => self.engine.log_error(format!("无法添加链接：{e}")),
        }
    }

    fn open_torrent_picker(&mut self) {
        let dir = self.engine.config().download_dir;
        let Some(path) = pick_torrent_file(&dir.to_string_lossy()) else {
            return;
        };
        self.add_url = path.to_string_lossy().to_string();
        self.add_dir = dir.to_string_lossy().to_string();
        self.add_error = None;
        self.add_open = true;
    }

    fn submit_add(&mut self) {
        let cfg = self.engine.config();
        match Source::parse(&self.add_url, &cfg.ftp_user, &cfg.ftp_password) {
            Ok(src) => {
                let dir = if self.add_dir.trim().is_empty() {
                    cfg.download_dir.clone()
                } else {
                    PathBuf::from(&self.add_dir)
                };
                if !dir.exists() {
                    if let Err(e) = std::fs::create_dir_all(&dir) {
                        self.add_error = Some(format!("无法创建目录：{e}"));
                        self.add_open = true;
                        return;
                    }
                }
                self.engine.add_task(src, dir);
                self.add_url.clear();
            }
            Err(e) => {
                self.add_error = Some(e);
                self.add_open = true;
            }
        }
    }

    // ---------- 剪贴板 ----------
    fn poll_clipboard(&mut self, ctx: &egui::Context) {
        if !self.clipboard.available() {
            return;
        }
        while let Some(link) = self.clipboard.try_recv() {
            if self.clip_prompt.is_some() || self.clip_ignored.contains(&link) {
                continue;
            }
            if self.add_open && self.add_url.trim() == link.trim() {
                continue;
            }
            self.clip_prompt = Some(link);
            ctx.request_repaint();
        }
    }

    fn clipboard_prompt(&mut self, ctx: &egui::Context) {
        let Some(link) = self.clip_prompt.clone() else {
            return;
        };
        let t = self.theme;
        let dir = self.engine.config().download_dir;
        let mut action = 0u8;

        egui::Window::new("检测到下载链接")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, -40.0])
            .show(ctx, |ui| {
                ui.set_width(520.0);
                ui.add_space(4.0);
                ui.label(RichText::new("剪贴板中检测到可下载的链接，要现在下载吗？").strong());
                ui.add_space(6.0);
                ui.label(RichText::new(shorten(&link, 180)).monospace());
                ui.label(
                    RichText::new(classify(&link))
                        .color(t.text_dim)
                        .size(Metrics::FONT_STATUS),
                );
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!("保存到：{}", dir.display()))
                        .color(t.text_dim)
                        .size(Metrics::FONT_STATUS),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("立即下载").clicked() {
                        action = 1;
                    }
                    if ui.button("选择目录…").clicked() {
                        action = 2;
                    }
                    if ui.button("忽略").clicked() {
                        action = 3;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let mut watch = self.clipboard.enabled();
                        if ui.checkbox(&mut watch, "继续监视").changed() {
                            self.clipboard.set_enabled(watch);
                        }
                    });
                });
            });

        match action {
            1 => {
                self.clip_prompt = None;
                self.start_from_link(&link);
            }
            2 => {
                self.clip_prompt = None;
                self.add_url = link;
                self.add_dir = dir.to_string_lossy().to_string();
                self.add_error = None;
                self.add_open = true;
            }
            3 => {
                self.clip_ignored.insert(link.clone());
                self.clip_prompt = None;
                self.engine
                    .log_info(format!("已忽略剪贴板链接：{}", shorten(&link, 80)));
            }
            _ => {}
        }
    }

    // ---------- 快捷键 ----------
    fn shortcuts(&mut self, ctx: &egui::Context) {
        if self.add_open || self.opts_open || self.clip_prompt.is_some() {
            return;
        }
        let (add, open_torrent, remove, toggle_pause) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::CTRL, egui::Key::N),
                i.consume_key(egui::Modifiers::CTRL, egui::Key::O),
                i.consume_key(egui::Modifiers::NONE, egui::Key::Delete),
                i.consume_key(egui::Modifiers::NONE, egui::Key::Space),
            )
        });
        if add {
            self.add_open = true;
        }
        if open_torrent {
            self.open_torrent_picker();
        }
        if let Some(id) = self.selected {
            if remove {
                self.engine.remove_task(id);
                self.selected = None;
            }
            if toggle_pause {
                let paused = self
                    .snapshot_tasks()
                    .iter()
                    .find(|t| t.id == id)
                    .map(|t| t.state == TaskState::Paused)
                    .unwrap_or(false);
                if paused {
                    self.engine.resume_task(id);
                } else {
                    self.engine.pause_task(id);
                }
            }
        }
    }

    // ---------- 拖放 ----------
    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if dropped.is_empty() {
            return;
        }
        let torrent = dropped.iter().find(|p| {
            p.extension()
                .map(|e| e.eq_ignore_ascii_case("torrent"))
                .unwrap_or(false)
        });
        match torrent {
            Some(p) => {
                self.add_url = p.to_string_lossy().to_string();
                self.add_dir = self.engine.config().download_dir.to_string_lossy().to_string();
                self.add_error = None;
                self.add_open = true;
                ctx.request_repaint();
            }
            None => self
                .engine
                .log_info("拖入的文件不是 .torrent，已忽略（BT 任务请选择种子文件）"),
        }
    }

    // ---------- 对话框：添加任务 ----------
    fn add_dialog(&mut self, ctx: &egui::Context) {
        if !self.add_open {
            return;
        }
        let t = self.theme;
        let mut open = true;
        let mut close = false;
        let mut do_add = false;

        egui::Window::new("添加下载任务")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, -40.0])
            .show(ctx, |ui| {
                ui.set_width(520.0);
                ui.add_space(4.0);
                ui.label(RichText::new("下载地址 / 种子路径").strong());
                ui.horizontal(|ui| {
                    let w = (ui.available_width() - 104.0).max(180.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.add_url)
                            .hint_text("https://… / ftp://… / magnet:?xt=… / C:\\x.torrent")
                            .desired_width(w),
                    );
                    if ui
                        .button("选择种子…")
                        .on_hover_text("打开文件对话框选择 .torrent")
                        .clicked()
                    {
                        if let Some(p) = pick_torrent_file(&self.add_dir) {
                            self.add_url = p.to_string_lossy().to_string();
                            self.add_error = None;
                        }
                    }
                });
                ui.label(
                    RichText::new(classify(&self.add_url))
                        .color(t.text_dim)
                        .size(Metrics::FONT_STATUS),
                );

                ui.add_space(8.0);
                ui.label(RichText::new("保存到").strong());
                ui.horizontal(|ui| {
                    let mut d = self.add_dir.clone();
                    let w = (ui.available_width() - 76.0).max(180.0);
                    if ui
                        .add(egui::TextEdit::singleline(&mut d).desired_width(w))
                        .changed()
                    {
                        self.add_dir = d;
                    }
                    if ui.button("浏览…").clicked() {
                        if let Some(p) = pick_folder(&self.add_dir) {
                            self.add_dir = p.to_string_lossy().to_string();
                        }
                    }
                });

                ui.add_space(8.0);
                ui.checkbox(&mut self.add_start_now, "添加后立即开始下载");

                if let Some(err) = &self.add_error {
                    ui.add_space(4.0);
                    ui.label(RichText::new(err).color(t.err));
                }

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.add_space(300.0);
                    if ui.button("取消").clicked() {
                        self.add_error = None;
                        close = true;
                    }
                    let ok = ui
                        .add_enabled(
                            !self.add_url.trim().is_empty(),
                            egui::Button::new("开始下载"),
                        )
                        .clicked();
                    if ok {
                        self.add_error = None;
                        do_add = true;
                        close = true;
                    }
                });
            });

        if close {
            open = false;
        }
        self.add_open = open;
        if do_add {
            self.submit_add();
        }
    }

    // ---------- 对话框：参数设置 ----------
    fn options_dialog(&mut self, ctx: &egui::Context) {
        if !self.opts_open {
            return;
        }
        let t = self.theme;
        let mut open = true;
        let mut close = false;
        let mut apply = false;
        egui::Window::new("参数设置")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(440.0);
                let c = &mut self.cfg_draft;

                ui.label(RichText::new("默认保存目录").strong());
                ui.horizontal(|ui| {
                    let mut d = c.download_dir.to_string_lossy().to_string();
                    let w = (ui.available_width() - 76.0).max(180.0);
                    if ui
                        .add(egui::TextEdit::singleline(&mut d).desired_width(w))
                        .changed()
                    {
                        c.download_dir = PathBuf::from(d);
                    }
                    if ui.button("浏览…").clicked() {
                        if let Some(p) = pick_folder(&c.download_dir.to_string_lossy()) {
                            c.download_dir = p;
                        }
                    }
                });

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("下载限速 (KB/s，0=不限)");
                    ui.add(
                        egui::DragValue::new(&mut c.speed_limit_down)
                            .speed(256)
                            .range(0..=1024 * 1024 * 1024),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("HTTP 连接数");
                    ui.add(egui::DragValue::new(&mut c.http_connections).range(1..=32));
                });
                ui.horizontal(|ui| {
                    ui.label("BT 最大 peer 数");
                    ui.add(egui::DragValue::new(&mut c.bt_max_peers).range(1..=200));
                });
                ui.horizontal(|ui| {
                    ui.label("BT 监听端口");
                    let mut p = c.bt_port;
                    if ui
                        .add(egui::DragValue::new(&mut p).range(1024..=65535))
                        .changed()
                    {
                        c.bt_port = p;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("同时进行的 BT 任务");
                    ui.add(egui::DragValue::new(&mut c.max_bt_tasks).range(1..=10));
                });
                ui.checkbox(&mut c.bt_seed_after_complete, "BT 完成后自动做种");

                ui.separator();
                ui.label(RichText::new("FTP 默认账号").strong());
                ui.horizontal(|ui| {
                    ui.label("用户");
                    ui.add(egui::TextEdit::singleline(&mut c.ftp_user).desired_width(140.0));
                    ui.label("密码");
                    ui.add(
                        egui::TextEdit::singleline(&mut c.ftp_password)
                            .password(true)
                            .desired_width(140.0),
                    );
                });
                let _ = t;

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.add_space(240.0);
                    if ui.button("取消").clicked() {
                        close = true;
                    }
                    if ui.button("应用").clicked() {
                        apply = true;
                        close = true;
                    }
                });
            });
        if close {
            open = false;
        }
        self.opts_open = open;
        if apply {
            self.engine.update_config(self.cfg_draft.clone());
        }
    }

    // ---------- 对话框：关于 ----------
    fn about_dialog(&mut self, ctx: &egui::Context) {
        if !self.about_open {
            return;
        }
        let t = self.theme;
        let mut open = true;
        egui::Window::new("关于")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(400.0);
                ui.horizontal(|ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(40.0, 40.0), egui::Sense::hover());
                    icons::paint(ui.painter(), rect, icons::Icon::Logo);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("rust-dm")
                                .size(16.0)
                                .strong()
                                .color(t.text),
                        );
                        ui.label(
                            RichText::new(format!("版本 {}", env!("CARGO_PKG_VERSION")))
                                .color(t.text_dim),
                        );
                    });
                });
                ui.add_space(8.0);
                ui.separator();
                ui.label("Rust 编写的多协议下载工具");
                ui.label("支持 HTTP / HTTPS、FTP、BitTorrent（magnet 与 .torrent）");
                ui.label("界面参照 Internet Download Manager 布局");
                ui.add_space(6.0);
                ui.label(
                    RichText::new("提示：复制下载链接后会自动弹窗询问是否下载")
                        .color(t.text_dim)
                        .size(Metrics::FONT_STATUS),
                );
            });
        self.about_open = open;
    }
}

// ---------- 文件对话框 ----------

/// 打开系统"选择文件夹"对话框。
pub fn pick_folder(start: &str) -> Option<PathBuf> {
    let mut dlg = rfd::FileDialog::new().set_title("选择文件夹");
    let start = Path::new(start.trim());
    if start.is_dir() {
        dlg = dlg.set_directory(start);
    }
    dlg.pick_folder()
}

/// 打开系统"选择文件"对话框，只挑 .torrent。
pub fn pick_torrent_file(start: &str) -> Option<PathBuf> {
    let mut dlg = rfd::FileDialog::new()
        .set_title("选择种子文件")
        .add_filter("BitTorrent 种子文件 (*.torrent)", &["torrent"])
        .add_filter("所有文件", &["*"]);
    let start = Path::new(start.trim());
    if start.is_dir() {
        dlg = dlg.set_directory(start);
    }
    dlg.pick_file()
}

// ---------- 小工具 ----------

/// 把过长的文本截断。
pub(crate) fn shorten(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

/// 根据输入判断协议类型，用于对话框提示。
pub(crate) fn classify(input: &str) -> &'static str {
    let s = input.trim().to_ascii_lowercase();
    if s.is_empty() {
        "支持 HTTP/HTTPS 直链、FTP 地址、magnet 链接，以及本地 .torrent 文件"
    } else if s.starts_with("magnet:") {
        "识别为：BitTorrent（magnet 链接）— 通过 DHT/Tracker 交换元数据"
    } else if s.starts_with("ftp://") {
        "识别为：FTP — 使用被动模式传输"
    } else if s.starts_with("http://") || s.starts_with("https://") {
        "识别为：HTTP(S) — 尝试多连接分段加速"
    } else if s.ends_with(".torrent") {
        "识别为：本地种子文件"
    } else {
        "无法识别协议，请检查地址是否正确"
    }
}

/// 在资源管理器里定位到目录。
pub(crate) fn open_folder(p: &Path) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer").arg(p).spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(p).spawn();
    }
}

/// 按字体宽度截断文本并补省略号（避免表格串列）。
pub(crate) fn fit_text(ui: &egui::Ui, text: &str, size: f32, max_w: f32) -> String {
    let font = egui::FontId::proportional(size);
    let width = |s: &str| {
        ui.fonts(|f| {
            f.layout_no_wrap(s.to_owned(), font.clone(), Color32::BLACK)
                .size()
                .x
        })
    };
    if max_w <= 0.0 {
        return String::new();
    }
    let full = width(text);
    if full <= max_w {
        return text.to_string();
    }
    // 先按比例估算，再逐步收缩，最多几次测量
    let chars: Vec<char> = text.chars().collect();
    let mut keep = ((max_w / full) * chars.len() as f32).floor() as usize;
    keep = keep.min(chars.len());
    for _ in 0..3 {
        if keep == 0 {
            break;
        }
        let cand: String = chars[..keep].iter().collect::<String>() + "…";
        if width(&cand) <= max_w {
            return cand;
        }
        keep = keep.saturating_sub((keep / 8).max(1));
    }
    "…".to_string()
}

/// 画 1px 水平分隔线。
pub(crate) fn hline(painter: &egui::Painter, x0: f32, x1: f32, y: f32, color: Color32) {
    painter.line_segment(
        [egui::pos2(x0, y), egui::pos2(x1, y)],
        Stroke::new(1.0, color),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 界面全是中文，字体必须真的包含中文字形，否则就是一片方块。
    #[test]
    fn cjk_font_covers_chinese_glyphs() {
        let (name, bytes) = load_cjk_font().expect("本机应能找到中文字体");
        let face = ttf_parser::Face::parse(&bytes, 0).expect("字体应能被解析");
        for ch in ['中', '文', '下', '载', '速', '度', '任', '务', '分', '类'] {
            assert!(
                face.glyph_index(ch).is_some(),
                "字体 {name} 缺少字形 {ch}，界面会显示成方块"
            );
        }
    }

    #[test]
    fn classifies_protocols() {
        assert!(classify("magnet:?xt=urn:btih:00").contains("BitTorrent"));
        assert!(classify("https://a/b.zip").contains("HTTP"));
        assert!(classify("ftp://a/b.zip").contains("FTP"));
        assert!(classify("C:\\x.torrent").contains("种子"));
        assert!(classify("").contains("支持"));
    }

    #[test]
    fn shorten_keeps_short_text() {
        assert_eq!(shorten("abc", 10), "abc");
        assert_eq!(shorten("abcdef", 4).chars().count(), 4);
    }
}
