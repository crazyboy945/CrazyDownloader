//! 左侧分类树（对应 IDM 的 Categories 面板）。

use egui::{pos2, vec2, Align2, CornerRadius, FontId, Rect, Sense, Stroke, Ui};

use crate::common::task::{Protocol, Task, TaskState};

use super::icons::{self, Icon};
use super::theme::Metrics;
use super::{hline, App};

/// 分类。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Category {
    All,
    Compressed,
    Documents,
    Music,
    Programs,
    Video,
    Unfinished,
    Finished,
    Bt,
    Failed,
}

/// 文件大类（按扩展名判断）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileKind {
    Compressed,
    Documents,
    Music,
    Programs,
    Video,
    Other,
}

/// 按文件名扩展名判断大类。
pub fn kind_of(name: &str) -> FileKind {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "zst" | "cab" => FileKind::Compressed,
        "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "pdf" | "txt" | "rtf" | "epub"
        | "md" | "csv" => FileKind::Documents,
        "mp3" | "flac" | "wav" | "aac" | "m4a" | "ogg" | "ape" | "wma" => FileKind::Music,
        "exe" | "msi" | "apk" | "dmg" | "deb" | "rpm" | "iso" | "bin" | "appx" => FileKind::Programs,
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "ts" | "rmvb" | "m4v" | "mpg"
        | "mpeg" => FileKind::Video,
        _ => FileKind::Other,
    }
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::All => "全部下载",
            Category::Compressed => "压缩包",
            Category::Documents => "文档",
            Category::Music => "音乐",
            Category::Programs => "程序",
            Category::Video => "视频",
            Category::Unfinished => "未完成",
            Category::Finished => "已完成",
            Category::Bt => "BT 任务",
            Category::Failed => "失败",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Category::All => Icon::FolderOpen,
            Category::Compressed => Icon::FileZip,
            Category::Documents => Icon::FileDoc,
            Category::Music => Icon::FileMusic,
            Category::Programs => Icon::FileApp,
            Category::Video => Icon::FileVideo,
            Category::Unfinished => Icon::Folder,
            Category::Finished => Icon::Folder,
            Category::Bt => Icon::FileTorrent,
            Category::Failed => Icon::StatusError,
        }
    }

    /// 该任务是否属于此分类。
    pub fn matches(self, t: &Task) -> bool {
        match self {
            Category::All => true,
            Category::Unfinished => !t.state.is_finished(),
            Category::Finished => t.state == TaskState::Completed,
            Category::Bt => t.protocol == Protocol::BitTorrent,
            Category::Failed => matches!(t.state, TaskState::Failed(_)),
            Category::Compressed => kind_of(&t.file_name) == FileKind::Compressed,
            Category::Documents => kind_of(&t.file_name) == FileKind::Documents,
            Category::Music => kind_of(&t.file_name) == FileKind::Music,
            Category::Programs => kind_of(&t.file_name) == FileKind::Programs,
            Category::Video => kind_of(&t.file_name) == FileKind::Video,
        }
    }
}

impl App {
    pub(super) fn sidebar(&mut self, ui: &mut Ui) {
        let t = self.theme;
        let full = ui.max_rect();

        // 顶部标题（与右侧表头同高，视觉上对齐）
        let head = Rect::from_min_size(full.min, vec2(full.width(), Metrics::LIST_HEADER_H));
        ui.painter().rect_filled(head, CornerRadius::ZERO, t.header_bg);
        hline(
            ui.painter(),
            full.left(),
            full.right(),
            head.bottom() - 0.5,
            t.header_border,
        );
        ui.painter().text(
            pos2(head.left() + 8.0, head.center().y),
            Align2::LEFT_CENTER,
            "分类",
            FontId::proportional(Metrics::FONT_HEADER),
            t.text,
        );

        // 右侧分割线
        ui.painter().line_segment(
            [pos2(full.right() - 0.5, full.top()), pos2(full.right() - 0.5, full.bottom())],
            Stroke::new(1.0, t.sidebar_border),
        );

        // 行数据：顶层四项 + 展开后的五个子分类
        let mut rows: Vec<(Category, bool)> = Vec::new();
        rows.push((Category::All, true));
        if self.tree_expanded {
            for c in [
                Category::Compressed,
                Category::Documents,
                Category::Music,
                Category::Programs,
                Category::Video,
            ] {
                rows.push((c, false));
            }
        }
        rows.push((Category::Unfinished, true));
        rows.push((Category::Finished, true));
        rows.push((Category::Bt, true));
        rows.push((Category::Failed, true));

        let mut y = head.bottom() + 2.0;
        for (cat, top_level) in rows {
            let row = Rect::from_min_size(
                pos2(full.left(), y),
                vec2(full.width(), Metrics::TREE_ROW_H),
            );
            let resp = ui.interact(
                row,
                ui.make_persistent_id(("cat", cat.label())),
                Sense::click(),
            );

            if self.category == cat {
                ui.painter()
                    .rect_filled(row, CornerRadius::ZERO, t.tree_selected);
            } else if resp.hovered() {
                ui.painter()
                    .rect_filled(row, CornerRadius::ZERO, t.tree_hover);
            }

            let indent = if top_level { 4.0 } else { 20.0 };
            let mut x = row.left() + indent;

            // 顶层「全部下载」左侧的展开箭头
            if cat == Category::All {
                let chev = Rect::from_min_size(pos2(x, row.top() + 4.0), vec2(12.0, 12.0));
                let cresp =
                    ui.interact(chev, ui.make_persistent_id("cat_expand"), Sense::click());
                icons::paint(
                    ui.painter(),
                    chev,
                    if self.tree_expanded {
                        Icon::ChevronDown
                    } else {
                        Icon::ChevronRight
                    },
                );
                if cresp.clicked() {
                    self.tree_expanded = !self.tree_expanded;
                }
                x += 16.0;
            }

            // 图标
            let ic = Rect::from_min_size(
                pos2(x, row.top() + (Metrics::TREE_ROW_H - Metrics::FILE_ICON) / 2.0),
                vec2(Metrics::FILE_ICON, Metrics::FILE_ICON),
            );
            icons::paint(ui.painter(), ic, cat.icon());
            x += Metrics::FILE_ICON + 6.0;

            // 文字
            ui.painter().text(
                pos2(x, row.center().y),
                Align2::LEFT_CENTER,
                cat.label(),
                FontId::proportional(Metrics::FONT_TREE),
                t.text,
            );

            if resp.clicked() {
                self.category = cat;
            }
            y += Metrics::TREE_ROW_H;
        }

        // 底部留白保持白色
        let _ = ui.allocate_space(vec2(full.width(), 0.0));
    }

    /// 当前分类 + 排序后的任务列表。
    pub(super) fn visible_tasks(&self, tasks: &[Task]) -> Vec<Task> {
        let mut out: Vec<Task> = tasks
            .iter()
            .filter(|t| self.category.matches(t))
            .cloned()
            .collect();
        out.sort_by(|a, b| {
            let ord = self.sort_key.cmp(a, b);
            if self.sort_asc {
                ord
            } else {
                ord.reverse()
            }
        });
        out
    }
}

