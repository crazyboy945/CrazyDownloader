//! 矢量图标：只用 egui 的画布 API 绘制，不依赖任何图片资源。
//!
//! 设计稿统一按 32×32 网格画（文件/状态类按 16×16、12×12），
//! 实际绘制时按目标矩形等比缩放，因此同一套图标既能当工具栏大图标，
//! 也能当列表里的小图标。

use egui::{
    epaint::RectShape, pos2, Color32, CornerRadius, Painter, Pos2, Rect, Shape, Stroke, StrokeKind,
};

use crate::common::task::{Protocol, TaskState};

// ---------- 配色 ----------
pub const GREEN: Color32 = Color32::from_rgb(46, 158, 68);
pub const GREEN_DARK: Color32 = Color32::from_rgb(32, 122, 50);
pub const RED: Color32 = Color32::from_rgb(212, 52, 52);
pub const RED_DARK: Color32 = Color32::from_rgb(160, 32, 32);
pub const BLUE: Color32 = Color32::from_rgb(47, 127, 209);
pub const BLUE_LIGHT: Color32 = Color32::from_rgb(104, 164, 222);
pub const GRAY: Color32 = Color32::from_rgb(138, 147, 157);
pub const GRAY_DARK: Color32 = Color32::from_rgb(90, 100, 110);
const PAGE: Color32 = Color32::from_rgb(255, 255, 255);
const PAGE_EDGE: Color32 = Color32::from_rgb(146, 156, 168);
const FOLD: Color32 = Color32::from_rgb(219, 226, 234);
const YELLOW: Color32 = Color32::from_rgb(226, 172, 44);
const YELLOW_LIGHT: Color32 = Color32::from_rgb(245, 201, 94);
const PURPLE: Color32 = Color32::from_rgb(138, 92, 190);
const VIDEO_BG: Color32 = Color32::from_rgb(58, 74, 90);
const WHITE: Color32 = Color32::from_rgb(255, 255, 255);

/// 所有用到的图标。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    // 工具栏
    AddTask,
    Resume,
    Stop,
    StopAll,
    Delete,
    ClearDone,
    Settings,
    Logs,
    SeedFile,
    StartAll,
    PauseAll,
    OpenFolder,
    About,
    Logo,
    // 分类树
    Folder,
    FolderOpen,
    // 文件类型
    FileGeneric,
    FileZip,
    FileDoc,
    FileMusic,
    FileApp,
    FileVideo,
    FileTorrent,
    // 状态
    StatusOk,
    StatusActive,
    StatusPaused,
    StatusQueued,
    StatusError,
    // 其它
    ChevronRight,
    ChevronDown,
}

/// 把 32×32 设计稿坐标映射到实际矩形。
struct Grid {
    origin: Pos2,
    k: f32,
}

impl Grid {
    fn new(rect: Rect) -> Self {
        let k = rect.width().min(rect.height()) / 32.0;
        Self {
            origin: rect.min,
            k,
        }
    }

    fn p(&self, x: f32, y: f32) -> Pos2 {
        pos2(self.origin.x + x * self.k, self.origin.y + y * self.k)
    }

    fn r(&self, x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
        Rect::from_min_max(self.p(x0, y0), self.p(x1, y1))
    }

    fn len(&self, v: f32) -> f32 {
        v * self.k
    }
}

/// 在给定矩形里画一个图标（自动等比缩放）。
pub fn paint(painter: &Painter, rect: Rect, icon: Icon) {
    let g = Grid::new(rect);
    match icon {
        Icon::AddTask => add_task(painter, &g),
        Icon::Resume => resume(painter, &g),
        Icon::Stop => stop(painter, &g, RED),
        Icon::StopAll => stop(painter, &g, GRAY),
        Icon::Delete => delete(painter, &g),
        Icon::ClearDone => clear_done(painter, &g),
        Icon::Settings => settings(painter, &g),
        Icon::Logs => logs(painter, &g),
        Icon::SeedFile => seed_file(painter, &g),
        Icon::StartAll => start_all(painter, &g),
        Icon::PauseAll => pause_all(painter, &g),
        Icon::OpenFolder => open_folder(painter, &g),
        Icon::About => about(painter, &g),
        Icon::Logo => logo(painter, &g),
        Icon::Folder => folder(painter, &g, false),
        Icon::FolderOpen => folder(painter, &g, true),
        Icon::FileGeneric => file_page(painter, &g, 0),
        Icon::FileZip => file_zip(painter, &g),
        Icon::FileDoc => file_doc(painter, &g),
        Icon::FileMusic => file_music(painter, &g),
        Icon::FileApp => file_app(painter, &g),
        Icon::FileVideo => file_video(painter, &g),
        Icon::FileTorrent => file_torrent(painter, &g),
        Icon::StatusOk => status_ok(painter, &g),
        Icon::StatusActive => status_active(painter, &g),
        Icon::StatusPaused => status_paused(painter, &g),
        Icon::StatusQueued => status_queued(painter, &g),
        Icon::StatusError => status_error(painter, &g),
        Icon::ChevronRight => chevron(painter, &g, false),
        Icon::ChevronDown => chevron(painter, &g, true),
    }
}

// ---------- 基础形状 ----------

fn page(painter: &Painter, g: &Grid, x0: f32, y0: f32, x1: f32, y1: f32) {
    let r = g.r(x0, y0, x1, y1);
    painter.rect_filled(r, CornerRadius::same(1), PAGE);
    painter.rect_stroke(
        r,
        CornerRadius::same(1),
        Stroke::new(g.len(0.8).max(1.0), PAGE_EDGE),
        StrokeKind::Inside,
    );
    // 右上角折角
    painter.add(Shape::convex_polygon(
        vec![g.p(x1 - 5.0, y0), g.p(x1, y0 + 5.0), g.p(x1 - 5.0, y0 + 5.0)],
        FOLD,
        Stroke::NONE,
    ));
}

fn octagon(g: &Grid, cx: f32, cy: f32, radius: f32, fill: Color32) -> Shape {
    let pts: Vec<Pos2> = (0..8)
        .map(|i| {
            let a = std::f32::consts::TAU * (i as f32 + 0.5) / 8.0;
            g.p(cx + radius * a.cos(), cy + radius * a.sin())
        })
        .collect();
    Shape::convex_polygon(pts, fill, Stroke::NONE)
}

fn plus(painter: &Painter, g: &Grid, cx: f32, cy: f32, arm: f32, width: f32, color: Color32) {
    let s = Stroke::new(g.len(width), color);
    painter.line_segment([g.p(cx - arm, cy), g.p(cx + arm, cy)], s);
    painter.line_segment([g.p(cx, cy - arm), g.p(cx, cy + arm)], s);
}

fn down_arrow(painter: &Painter, g: &Grid, cx: f32, top: f32, bottom: f32, half_w: f32, color: Color32) {
    let shaft_w = half_w * 0.55;
    painter.rect_filled(
        g.r(cx - shaft_w, top, cx + shaft_w, bottom - half_w * 1.4),
        CornerRadius::ZERO,
        color,
    );
    painter.add(Shape::convex_polygon(
        vec![
            g.p(cx - half_w, bottom - half_w * 1.6),
            g.p(cx + half_w, bottom - half_w * 1.6),
            g.p(cx, bottom),
        ],
        color,
        Stroke::NONE,
    ));
}

fn check(painter: &Painter, g: &Grid, color: Color32, width: f32) {
    let s = Stroke::new(g.len(width), color);
    painter.add(Shape::line(
        vec![g.p(9.0, 16.5), g.p(13.5, 21.5), g.p(23.0, 10.5)],
        s,
    ));
}

// ---------- 工具栏图标 ----------

fn add_task(painter: &Painter, g: &Grid) {
    page(painter, g, 5.0, 2.5, 22.0, 25.0);
    // 页面上的横线，暗示"内容"
    for y in [9.0f32, 13.0, 17.0] {
        painter.line_segment(
            [g.p(8.5, y), g.p(18.5, y)],
            Stroke::new(g.len(1.2), Color32::from_rgb(196, 204, 214)),
        );
    }
    let c = g.p(23.5, 22.5);
    painter.circle_filled(c, g.len(8.6), WHITE);
    painter.circle_filled(c, g.len(7.6), GREEN);
    plus(painter, g, 23.5, 22.5, 4.2, 2.4, WHITE);
}

fn resume(painter: &Painter, g: &Grid) {
    painter.circle_filled(g.p(16.0, 16.0), g.len(13.5), GREEN);
    painter.circle_stroke(
        g.p(16.0, 16.0),
        g.len(13.5),
        Stroke::new(g.len(1.0), GREEN_DARK),
    );
    down_arrow(painter, g, 16.0, 7.0, 25.0, 6.0, WHITE);
}

fn stop(painter: &Painter, g: &Grid, color: Color32) {
    painter.add(octagon(g, 16.0, 16.0, 13.5, color));
    painter.rect_filled(
        g.r(11.0, 11.0, 21.0, 21.0),
        CornerRadius::same(1),
        WHITE,
    );
}

fn delete(painter: &Painter, g: &Grid) {
    // 桶身
    painter.add(Shape::convex_polygon(
        vec![
            g.p(9.5, 11.5),
            g.p(22.5, 11.5),
            g.p(21.0, 27.0),
            g.p(11.0, 27.0),
        ],
        Color32::from_rgb(222, 228, 234),
        Stroke::new(g.len(0.9), Color32::from_rgb(150, 160, 172)),
    ));
    for x in [13.5f32, 16.0, 18.5] {
        painter.line_segment(
            [g.p(x, 14.0), g.p(x, 24.0)],
            Stroke::new(g.len(1.0), Color32::from_rgb(168, 178, 190)),
        );
    }
    // 盖与提手
    painter.rect_filled(
        g.r(7.5, 8.5, 24.5, 12.0),
        CornerRadius::same(1),
        RED,
    );
    painter.rect_filled(
        g.r(13.0, 5.0, 19.0, 8.5),
        CornerRadius::same(1),
        RED_DARK,
    );
}

fn clear_done(painter: &Painter, g: &Grid) {
    page(painter, g, 5.0, 2.5, 23.0, 28.5);
    for y in [8.0f32, 12.0] {
        painter.line_segment(
            [g.p(8.5, y), g.p(19.0, y)],
            Stroke::new(g.len(1.2), Color32::from_rgb(198, 206, 216)),
        );
    }
    check(painter, g, GREEN, 2.8);
}

fn settings(painter: &Painter, g: &Grid) {
    let c = g.p(16.0, 16.0);
    // 齿
    for i in 0..8 {
        let a = std::f32::consts::TAU * i as f32 / 8.0;
        let (s, sn) = a.sin_cos();
        painter.line_segment(
            [
                pos2(c.x + s * g.len(10.0), c.y + sn * g.len(10.0)),
                pos2(c.x + s * g.len(15.0), c.y + sn * g.len(15.0)),
            ],
            Stroke::new(g.len(4.0), GRAY),
        );
    }
    painter.circle_filled(c, g.len(11.0), GRAY);
    painter.circle_filled(c, g.len(8.6), Color32::from_rgb(244, 246, 249));
    painter.circle_filled(c, g.len(4.2), GRAY_DARK);
}

fn logs(painter: &Painter, g: &Grid) {
    // 一张带横线的纸 + 右下角放大镜，表示"查看日志"
    page(painter, g, 5.0, 2.5, 24.0, 28.0);
    for (i, y) in [8.5f32, 13.0, 17.5, 22.0].iter().enumerate() {
        let x1 = if i == 3 { 16.0 } else { 21.0 };
        painter.line_segment(
            [g.p(8.5, *y), g.p(x1, *y)],
            Stroke::new(g.len(1.4), Color32::from_rgb(120, 150, 185)),
        );
    }
    let c = g.p(23.0, 23.0);
    painter.circle_filled(c, g.len(6.6), Color32::WHITE);
    painter.circle_stroke(c, g.len(6.0), Stroke::new(g.len(2.2), BLUE));
    painter.line_segment(
        [pos2(c.x + g.len(4.0), c.y + g.len(4.0)), g.p(29.5, 29.5)],
        Stroke::new(g.len(2.4), BLUE),
    );
}

fn seed_file(painter: &Painter, g: &Grid) {
    // 文件夹
    painter.rect_filled(
        g.r(3.5, 8.0, 11.0, 11.0),
        CornerRadius::same(1),
        YELLOW,
    );
    painter.rect_filled(
        g.r(3.5, 10.0, 20.5, 26.0),
        CornerRadius::same(1),
        YELLOW,
    );
    painter.rect_filled(
        g.r(4.6, 12.0, 19.4, 24.8),
        CornerRadius::same(0),
        YELLOW_LIGHT,
    );
    // 绿色下载角标
    let c = g.p(23.0, 21.5);
    painter.circle_filled(c, g.len(8.2), WHITE);
    painter.circle_filled(c, g.len(7.2), GREEN);
    down_arrow(painter, g, 23.0, 16.0, 27.0, 4.4, WHITE);
}

fn start_all(painter: &Painter, g: &Grid) {
    painter.add(Shape::convex_polygon(
        vec![g.p(6.0, 7.5), g.p(6.0, 24.5), g.p(17.0, 16.0)],
        BLUE,
        Stroke::NONE,
    ));
    painter.add(Shape::convex_polygon(
        vec![g.p(16.0, 7.5), g.p(16.0, 24.5), g.p(27.0, 16.0)],
        BLUE_LIGHT,
        Stroke::NONE,
    ));
}

fn pause_all(painter: &Painter, g: &Grid) {
    for x in [8.0f32, 18.0] {
        painter.rect_filled(
            g.r(x, 6.5, x + 6.0, 25.5),
            CornerRadius::same(1),
            GRAY_DARK,
        );
    }
}

fn open_folder(painter: &Painter, g: &Grid) {
    painter.add(Shape::convex_polygon(
        vec![
            g.p(3.0, 26.0),
            g.p(7.0, 13.0),
            g.p(29.0, 13.0),
            g.p(25.0, 26.0),
        ],
        YELLOW_LIGHT,
        Stroke::new(g.len(0.9), Color32::from_rgb(196, 146, 34)),
    ));
    painter.rect_filled(
        g.r(3.0, 6.0, 13.0, 13.5),
        CornerRadius::same(1),
        YELLOW,
    );
    painter.rect_filled(g.r(3.0, 12.0, 27.0, 15.0), CornerRadius::ZERO, YELLOW);
}

fn about(painter: &Painter, g: &Grid) {
    let c = g.p(16.0, 16.0);
    painter.circle_filled(c, g.len(13.0), BLUE);
    painter.circle_stroke(c, g.len(13.0), Stroke::new(g.len(1.0), Color32::from_rgb(32, 96, 168)));
    painter.circle_filled(g.p(16.0, 9.5), g.len(2.0), WHITE);
    painter.rect_filled(
        g.r(14.0, 13.0, 18.0, 24.0),
        CornerRadius::same(1),
        WHITE,
    );
}

fn logo(painter: &Painter, g: &Grid) {
    painter.rect_filled(
        g.r(2.0, 2.0, 30.0, 30.0),
        CornerRadius::same(6),
        BLUE,
    );
    down_arrow(painter, g, 16.0, 7.0, 24.0, 7.0, WHITE);
    painter.rect_filled(
        g.r(7.0, 25.0, 25.0, 27.6),
        CornerRadius::same(1),
        Color32::from_rgb(120, 178, 232),
    );
}

fn folder(painter: &Painter, g: &Grid, open: bool) {
    if open {
        painter.add(Shape::convex_polygon(
            vec![
                g.p(2.0, 27.0),
                g.p(6.0, 15.0),
                g.p(30.0, 15.0),
                g.p(26.0, 27.0),
            ],
            YELLOW_LIGHT,
            Stroke::NONE,
        ));
    }
    painter.rect_filled(
        g.r(2.0, 7.0, 12.0, 13.0),
        CornerRadius::same(1),
        YELLOW,
    );
    painter.rect_filled(
        g.r(2.0, 11.0, 26.0, 27.0),
        CornerRadius::same(1),
        YELLOW,
    );
    if !open {
        painter.rect_filled(
            g.r(3.4, 13.0, 24.6, 25.6),
            CornerRadius::ZERO,
            YELLOW_LIGHT,
        );
    }
}

// ---------- 文件类型图标 ----------

fn file_page(painter: &Painter, g: &Grid, lines: usize) {
    page(painter, g, 6.0, 2.0, 26.0, 30.0);
    for i in 0..lines {
        let y = 10.0 + i as f32 * 4.5;
        painter.line_segment(
            [g.p(10.0, y), g.p(22.0, y)],
            Stroke::new(g.len(1.4), Color32::from_rgb(186, 196, 208)),
        );
    }
}

fn file_zip(painter: &Painter, g: &Grid) {
    file_page(painter, g, 0);
    // 拉链
    for i in 0..5 {
        let y = 8.0 + i as f32 * 3.0;
        painter.rect_filled(g.r(14.5, y, 17.5, y + 1.8), CornerRadius::ZERO, GRAY_DARK);
    }
    painter.rect_filled(g.r(6.6, 22.0, 25.4, 29.4), CornerRadius::ZERO, YELLOW);
}

fn file_doc(painter: &Painter, g: &Grid) {
    file_page(painter, g, 0);
    for i in 0..4 {
        let y = 10.0 + i as f32 * 4.2;
        let x1 = if i == 3 { 17.0 } else { 22.0 };
        painter.line_segment(
            [g.p(10.0, y), g.p(x1, y)],
            Stroke::new(g.len(1.6), Color32::from_rgb(96, 140, 196)),
        );
    }
}

fn file_music(painter: &Painter, g: &Grid) {
    file_page(painter, g, 0);
    painter.rect_filled(g.r(9.0, 9.0, 11.0, 21.0), CornerRadius::ZERO, PURPLE);
    painter.rect_filled(g.r(17.0, 7.0, 19.0, 19.0), CornerRadius::ZERO, PURPLE);
    painter.rect_filled(g.r(9.0, 7.0, 19.0, 10.0), CornerRadius::ZERO, PURPLE);
    painter.circle_filled(g.p(7.8, 21.0), g.len(2.6), PURPLE);
    painter.circle_filled(g.p(15.8, 19.0), g.len(2.6), PURPLE);
}

fn file_app(painter: &Painter, g: &Grid) {
    painter.rect_filled(g.r(3.0, 4.0, 29.0, 28.0), CornerRadius::same(2), BLUE);
    painter.rect_filled(g.r(3.0, 4.0, 29.0, 9.0), CornerRadius::same(2), Color32::from_rgb(32, 96, 168));
    painter.rect_filled(g.r(6.0, 11.5, 26.0, 24.5), CornerRadius::same(1), Color32::from_rgb(220, 233, 247));
    painter.rect_filled(g.r(8.0, 14.0, 24.0, 16.0), CornerRadius::ZERO, BLUE_LIGHT);
    painter.rect_filled(g.r(8.0, 18.0, 20.0, 20.0), CornerRadius::ZERO, BLUE_LIGHT);
}

fn file_video(painter: &Painter, g: &Grid) {
    painter.rect_filled(g.r(2.0, 5.0, 30.0, 27.0), CornerRadius::same(2), VIDEO_BG);
    painter.add(Shape::convex_polygon(
        vec![g.p(12.0, 10.0), g.p(12.0, 22.0), g.p(22.0, 16.0)],
        WHITE,
        Stroke::NONE,
    ));
    // 胶片齿孔
    for x in [4.0f32, 26.0] {
        for y in [7.0f32, 12.0, 17.0, 22.0] {
            painter.rect_filled(g.r(x, y, x + 2.0, y + 2.0), CornerRadius::ZERO, Color32::from_rgb(120, 140, 160));
        }
    }
}

fn file_torrent(painter: &Painter, g: &Grid) {
    file_page(painter, g, 0);
    let c = g.p(16.0, 20.0);
    painter.circle_filled(c, g.len(8.4), GREEN);
    down_arrow(painter, g, 16.0, 14.0, 23.0, 4.0, WHITE);
    // 底部一条弧，像"做种"
    let pts: Vec<Pos2> = (0..9)
        .map(|i| {
            let a = std::f32::consts::PI * (0.15 + 0.7 * i as f32 / 8.0);
            pos2(c.x + a.cos() * g.len(6.0), c.y + a.sin() * g.len(6.0))
        })
        .collect();
    painter.add(Shape::line(pts, Stroke::new(g.len(1.2), Color32::from_rgb(200, 245, 205))));
}

// ---------- 状态图标 ----------

fn status_ok(painter: &Painter, g: &Grid) {
    painter.circle_filled(g.p(16.0, 16.0), g.len(14.0), GREEN);
    painter.add(Shape::line(
        vec![g.p(8.0, 16.5), g.p(13.5, 22.5), g.p(24.0, 9.5)],
        Stroke::new(g.len(3.6), WHITE),
    ));
}

fn status_active(painter: &Painter, g: &Grid) {
    painter.circle_filled(g.p(16.0, 16.0), g.len(14.0), BLUE);
    down_arrow(painter, g, 16.0, 6.0, 26.0, 7.0, WHITE);
}

fn status_paused(painter: &Painter, g: &Grid) {
    painter.circle_filled(g.p(16.0, 16.0), g.len(14.0), GRAY);
    for x in [10.5f32, 18.0] {
        painter.rect_filled(
            g.r(x, 9.0, x + 4.0, 23.0),
            CornerRadius::same(1),
            WHITE,
        );
    }
}

fn status_queued(painter: &Painter, g: &Grid) {
    let c = g.p(16.0, 16.0);
    painter.circle_filled(c, g.len(14.0), Color32::from_rgb(196, 203, 211));
    painter.circle_stroke(c, g.len(14.0), Stroke::new(g.len(1.0), GRAY));
    painter.line_segment([c, g.p(16.0, 8.0)], Stroke::new(g.len(2.4), WHITE));
    painter.line_segment([c, g.p(22.0, 19.0)], Stroke::new(g.len(2.4), WHITE));
}

fn status_error(painter: &Painter, g: &Grid) {
    painter.circle_filled(g.p(16.0, 16.0), g.len(14.0), RED);
    let s = Stroke::new(g.len(3.4), WHITE);
    painter.line_segment([g.p(10.0, 10.0), g.p(22.0, 22.0)], s);
    painter.line_segment([g.p(22.0, 10.0), g.p(10.0, 22.0)], s);
}

fn chevron(painter: &Painter, g: &Grid, down: bool) {
    let pts = if down {
        vec![g.p(10.0, 13.0), g.p(22.0, 13.0), g.p(16.0, 20.0)]
    } else {
        vec![g.p(13.0, 10.0), g.p(20.0, 16.0), g.p(13.0, 22.0)]
    };
    painter.add(Shape::convex_polygon(pts, GRAY_DARK, Stroke::NONE));
}

// ---------- 业务映射 ----------

/// 按扩展名 / 协议选择文件图标。
pub fn file_icon(protocol: Protocol, name: &str) -> Icon {
    if protocol == Protocol::BitTorrent {
        return Icon::FileTorrent;
    }
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "zst" | "cab" => Icon::FileZip,
        "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "pdf" | "txt" | "rtf" | "epub"
        | "md" | "csv" => Icon::FileDoc,
        "mp3" | "flac" | "wav" | "aac" | "m4a" | "ogg" | "ape" | "wma" => Icon::FileMusic,
        "exe" | "msi" | "apk" | "dmg" | "deb" | "rpm" | "iso" | "bin" | "appx" => Icon::FileApp,
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "ts" | "rmvb" | "m4v"
        | "mpg" | "mpeg" => Icon::FileVideo,
        _ => Icon::FileGeneric,
    }
}

/// 按任务状态选状态图标。
pub fn status_icon(state: &TaskState) -> Icon {
    match state {
        TaskState::Completed => Icon::StatusOk,
        TaskState::Downloading | TaskState::Connecting | TaskState::Checking => Icon::StatusActive,
        TaskState::Seeding => Icon::StatusOk,
        TaskState::Paused => Icon::StatusPaused,
        TaskState::Queued => Icon::StatusQueued,
        TaskState::Failed(_) => Icon::StatusError,
    }
}

/// 状态文字（IDM 用 Complete / 百分比 / Queued 这类短语）。
pub fn status_text(state: &TaskState, progress: f32) -> String {
    match state {
        TaskState::Completed => "已完成".to_string(),
        TaskState::Seeding => "做种中".to_string(),
        TaskState::Downloading => format!("{:.0}%", progress * 100.0),
        TaskState::Connecting => "连接中".to_string(),
        TaskState::Checking => "校验中".to_string(),
        TaskState::Paused => format!("已暂停 {:.0}%", progress * 100.0),
        TaskState::Queued => "排队".to_string(),
        TaskState::Failed(_) => "失败".to_string(),
    }
}

/// 状态文字颜色（选中行上统一用白字之外的颜色）。
pub fn status_color(state: &TaskState, text: Color32) -> Color32 {
    match state {
        // IDM 里 "Complete" 的文字是黑色，绿色由左侧的小图标承担
        TaskState::Completed | TaskState::Seeding => text,
        TaskState::Downloading | TaskState::Connecting | TaskState::Checking => BLUE,
        TaskState::Failed(_) => RED_DARK,
        _ => text,
    }
}

/// 未使用但保留：给矩形画一层淡色底（用于悬停等）。
#[allow(dead_code)]
pub fn tint(painter: &Painter, rect: Rect, color: Color32) {
    painter.add(RectShape::filled(rect, CornerRadius::ZERO, color));
}
