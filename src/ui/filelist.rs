//! 右侧文件列表：自绘表格（表头排序、进度底色、迷你速率折线）。

use std::collections::VecDeque;

use egui::{pos2, vec2, Align2, Color32, CornerRadius, FontId, Painter, Pos2, Rect, Sense, Shape, Stroke, Ui};

use crate::common::task::{SortKey, Task, TaskState};
use crate::common::util::{format_datetime, human_duration, human_rate, human_size};

use super::icons;
use super::theme::{Metrics, Theme};
use super::{fit_text, hline, App};

/// 一列的定义。
struct Col {
    /// `None` 表示该列不可排序（描述）
    key: Option<SortKey>,
    label: &'static str,
    width: f32,
    right: bool,
}

const FIXED_W: f32 = 90.0 + 104.0 + 84.0 + 126.0 + 96.0;

fn columns(name_w: f32, desc_w: f32) -> Vec<Col> {
    vec![
        Col { key: Some(SortKey::Name), label: "文件名", width: name_w, right: false },
        Col { key: Some(SortKey::Size), label: "大小", width: 90.0, right: true },
        Col { key: Some(SortKey::Status), label: "状态", width: 104.0, right: false },
        Col { key: Some(SortKey::Eta), label: "剩余时间", width: 84.0, right: false },
        Col { key: Some(SortKey::SpeedDown), label: "传输速率", width: 126.0, right: false },
        Col { key: Some(SortKey::LastTry), label: "上次尝试", width: 96.0, right: false },
        Col { key: None, label: "描述", width: desc_w, right: false },
    ]
}

impl App {
    pub(super) fn file_list(&mut self, ui: &mut Ui) {
        let t = self.theme;
        let tasks = self.snapshot_tasks();
        self.sample_speeds(&tasks);
        let rows = self.visible_tasks(&tasks);

        let full = ui.max_rect();
        // 列宽：文件名和描述平分剩余空间，描述最少 110
        let avail = full.width();
        let desc_w = ((avail - FIXED_W) * 0.32).clamp(110.0, 260.0);
        let name_w = (avail - FIXED_W - desc_w).max(200.0);
        let cols = columns(name_w, desc_w);

        let mut new_selected: Option<u64> = None;
        let mut open_dir: Option<std::path::PathBuf> = None;
        let mut ctx_action: Option<(u8, u64)> = None; // 1=开始 2=暂停 3=移除
        let mut new_sort: Option<SortKey> = None;

        // ---- 表头 ----
        let head = Rect::from_min_size(full.min, vec2(full.width(), Metrics::LIST_HEADER_H));
        ui.painter().rect_filled(head, CornerRadius::ZERO, t.header_bg);
        hline(ui.painter(), head.left(), head.right(), head.bottom() - 0.5, t.header_border);

        let mut x = head.left();
        for col in &cols {
            let cell = Rect::from_min_size(
                pos2(x, head.top()),
                vec2(col.width, Metrics::LIST_HEADER_H),
            );
            // 列分隔
            if x > head.left() {
                ui.painter().line_segment(
                    [pos2(x, head.top() + 4.0), pos2(x, head.bottom() - 4.0)],
                    Stroke::new(1.0, Color32::from_rgb(214, 214, 214)),
                );
            }
            if let Some(key) = col.key {
                let resp = ui.interact(
                    cell,
                    ui.make_persistent_id(("hdr", col.label)),
                    Sense::click(),
                );
                let active = self.sort_key == key;
                if resp.hovered() {
                    ui.painter().rect_filled(
                        cell.shrink2(vec2(1.0, 2.0)),
                        CornerRadius::ZERO,
                        t.btn_hover,
                    );
                }
                // 标签固定用黑色，排序列的三角标用强调色（IDM 的做法）
                let anchor = if col.right {
                    cell.right_center() - vec2(8.0, 0.0)
                } else {
                    cell.left_center() + vec2(8.0, 0.0)
                };
                let align = if col.right {
                    Align2::RIGHT_CENTER
                } else {
                    Align2::LEFT_CENTER
                };
                let label_rect = ui.painter().text(
                    anchor,
                    align,
                    col.label,
                    FontId::proportional(Metrics::FONT_HEADER),
                    t.text,
                );
                if active {
                    let arrow = if self.sort_asc { "▲" } else { "▼" };
                    let ax = if col.right {
                        label_rect.left() - 2.0
                    } else {
                        label_rect.right() + 2.0
                    };
                    ui.painter().text(
                        pos2(ax, cell.center().y),
                        if col.right {
                            Align2::RIGHT_CENTER
                        } else {
                            Align2::LEFT_CENTER
                        },
                        arrow,
                        FontId::proportional(8.0),
                        t.accent,
                    );
                }
                if resp.clicked() {
                    new_sort = Some(key);
                }
            } else {
                let text_rect = cell.shrink2(vec2(8.0, 0.0));
                ui.painter().text(
                    text_rect.left_center(),
                    Align2::LEFT_CENTER,
                    col.label,
                    FontId::proportional(Metrics::FONT_HEADER),
                    t.text,
                );
            }
            x += col.width;
        }

        // ---- 表体 ----
        let body_top = head.bottom();
        let body_rect = Rect::from_min_max(pos2(full.left(), body_top), full.max);
        let mut body_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(body_rect)
                .layout(egui::Layout::top_down(egui::Align::LEFT)),
        );

        egui::ScrollArea::vertical()
            .id_salt("task_rows")
            .auto_shrink([false, false])
            .show(&mut body_ui, |ui| {
                let total_h = rows.len() as f32 * Metrics::ROW_H;
                let (content, _) = ui.allocate_exact_size(
                    vec2(ui.available_width(), total_h.max(1.0)),
                    Sense::hover(),
                );

                if rows.is_empty() {
                    let hint = if self.category == super::Category::All {
                        "暂无下载任务 — 点击「新建任务」或复制下载链接"
                    } else {
                        "该分类下暂无任务"
                    };
                    ui.painter().text(
                        pos2(content.center().x, content.top() + 28.0),
                        Align2::CENTER_CENTER,
                        hint,
                        FontId::proportional(13.0),
                        t.text_dim,
                    );
                    return;
                }

                for (i, task) in rows.iter().enumerate() {
                    let row = Rect::from_min_size(
                        pos2(content.left(), content.top() + i as f32 * Metrics::ROW_H),
                        vec2(content.width(), Metrics::ROW_H),
                    );
                    if !row.intersects(ui.clip_rect()) {
                        continue;
                    }
                    let resp = ui.interact(
                        row,
                        ui.make_persistent_id(("row", task.id)),
                        Sense::click(),
                    );
                    let selected = self.selected == Some(task.id);
                    self.paint_row(ui, row, task, &cols, selected, resp.hovered(), &t);

                    if resp.clicked() {
                        new_selected = Some(task.id);
                    }
                    if resp.double_clicked() {
                        new_selected = Some(task.id);
                        open_dir = Some(task.save_dir.clone());
                    }
                    resp.context_menu(|ui| {
                        if ui.button("开始 / 继续").clicked() {
                            ctx_action = Some((1, task.id));
                            ui.close_menu();
                        }
                        if ui.button("暂停").clicked() {
                            ctx_action = Some((2, task.id));
                            ui.close_menu();
                        }
                        if ui.button("打开所在目录").clicked() {
                            open_dir = Some(task.save_dir.clone());
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("移除任务").clicked() {
                            ctx_action = Some((3, task.id));
                            ui.close_menu();
                        }
                    });
                }
            });

        // ---- 收尾处理（避免与借用冲突，统一在绘制后应用）----
        if let Some(key) = new_sort {
            if self.sort_key == key {
                self.sort_asc = !self.sort_asc;
            } else {
                self.sort_key = key;
                self.sort_asc = true;
            }
        }
        if let Some(id) = new_selected {
            self.selected = Some(id);
        }
        if let Some(p) = open_dir {
            super::open_folder(&p);
        }
        if let Some((action, id)) = ctx_action {
            self.selected = Some(id);
            match action {
                1 => self.engine.resume_task(id),
                2 => self.engine.pause_task(id),
                _ => {
                    self.engine.remove_task(id);
                    self.selected = None;
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_row(
        &self,
        ui: &Ui,
        row: Rect,
        task: &Task,
        cols: &[Col],
        selected: bool,
        hovered: bool,
        t: &Theme,
    ) {
        let p = ui.painter();
        let progress = task.progress();
        let active = matches!(
            task.state,
            TaskState::Downloading | TaskState::Connecting | TaskState::Checking
        );

        // 底色
        if selected {
            p.rect_filled(row, CornerRadius::ZERO, t.row_selected);
        } else if active && progress > 0.0 {
            // 下载中：进度底色（左浅绿、右空）
            p.rect_filled(row, CornerRadius::ZERO, t.row_progress_bg);
            p.rect_filled(
                Rect::from_min_size(row.min, vec2(row.width() * progress, row.height())),
                CornerRadius::ZERO,
                t.row_progress,
            );
            if hovered {
                p.rect_filled(
                    row,
                    CornerRadius::ZERO,
                    Color32::from_rgba_unmultiplied(240, 247, 255, 90),
                );
            }
        } else if hovered {
            p.rect_filled(row, CornerRadius::ZERO, t.row_hover);
        }

        let text_color = if selected { t.row_selected_text } else { t.text };
        let dim_color = if selected { t.text_dim_select } else { t.text_dim };

        let mut x = row.left();
        for (ci, col) in cols.iter().enumerate() {
            let cell = Rect::from_min_size(
                pos2(x, row.top()),
                vec2(col.width, Metrics::ROW_H),
            );
            // 表体不画列分隔线（IDM 只在表头画），避免列表看起来像网格

            match ci {
                0 => {
                    // 文件名：类型图标 + 名称
                    let ic = Rect::from_min_size(
                        pos2(cell.left() + 4.0, cell.center().y - Metrics::FILE_ICON / 2.0),
                        vec2(Metrics::FILE_ICON, Metrics::FILE_ICON),
                    );
                    icons::paint(p, ic, icons::file_icon(task.protocol, &task.file_name));
                    let text_x = ic.right() + 6.0;
                    let max_w = cell.right() - text_x - 4.0;
                    let name = fit_text(ui, &task.name, Metrics::FONT_ROW, max_w);
                    p.text(
                        pos2(text_x, cell.center().y),
                        Align2::LEFT_CENTER,
                        name,
                        FontId::proportional(Metrics::FONT_ROW),
                        text_color,
                    );
                }
                1 => {
                    // 大小
                    let text = if task.total_bytes > 0 {
                        human_size(task.total_bytes)
                    } else {
                        "--".to_string()
                    };
                    p.text(
                        cell.right_center() - vec2(10.0, 0.0),
                        Align2::RIGHT_CENTER,
                        text,
                        FontId::proportional(Metrics::FONT_ROW),
                        text_color,
                    );
                }
                2 => {
                    // 状态：彩色图标 + 文字
                    let icon_rect = Rect::from_min_size(
                        pos2(cell.left() + 6.0, cell.center().y - Metrics::STATUS_ICON / 2.0),
                        vec2(Metrics::STATUS_ICON, Metrics::STATUS_ICON),
                    );
                    icons::paint(p, icon_rect, icons::status_icon(&task.state));
                    let color = if selected {
                        t.row_selected_text
                    } else {
                        icons::status_color(&task.state, t.text_dim)
                    };
                    let text = icons::status_text(&task.state, progress);
                    p.text(
                        pos2(icon_rect.right() + 6.0, cell.center().y),
                        Align2::LEFT_CENTER,
                        fit_text(ui, &text, Metrics::FONT_ROW, cell.width() - 34.0),
                        FontId::proportional(Metrics::FONT_ROW),
                        color,
                    );
                }
                3 => {
                    // 剩余时间
                    let text = if task.state == TaskState::Completed {
                        "--".to_string()
                    } else {
                        human_duration(task.eta)
                    };
                    p.text(
                        pos2(cell.left() + 8.0, cell.center().y),
                        Align2::LEFT_CENTER,
                        text,
                        FontId::proportional(Metrics::FONT_ROW),
                        text_color,
                    );
                }
                4 => {
                    // 传输速率：迷你折线 + 数字
                    let spark = Rect::from_min_size(
                        pos2(cell.left() + 8.0, cell.center().y - 6.0),
                        vec2(42.0, 12.0),
                    );
                    // 只有真的在传输时才画迷你图，否则一排"贴底直线"很脏
                    if active && task.speed_down > 0 {
                        if let Some(hist) = self.speed_hist.get(&task.id) {
                            let color = if selected { t.row_selected_text } else { t.ok };
                            if hist.iter().any(|v| *v > 0) {
                                paint_spark(p, spark, hist, color);
                            }
                        }
                    }
                    let text = if task.speed_down > 0 {
                        human_rate(task.speed_down as f64)
                    } else if task.speed_up > 0 {
                        format!("↑ {}", human_rate(task.speed_up as f64))
                    } else {
                        "--".to_string()
                    };
                    p.text(
                        pos2(spark.right() + 8.0, cell.center().y),
                        Align2::LEFT_CENTER,
                        fit_text(ui, &text, Metrics::FONT_ROW, cell.right() - spark.right() - 12.0),
                        FontId::proportional(Metrics::FONT_ROW),
                        if task.speed_down > 0 && !selected { t.info } else { text_color },
                    );
                }
                5 => {
                    // 上次尝试（用任务添加时间）
                    p.text(
                        pos2(cell.left() + 8.0, cell.center().y),
                        Align2::LEFT_CENTER,
                        format_datetime(task.created_at),
                        FontId::proportional(Metrics::FONT_ROW),
                        text_color,
                    );
                }
                _ => {
                    // 描述：优先显示最近一条状态消息，其次显示协议与保存目录
                    let desc = if !task.last_message.is_empty() {
                        task.last_message.clone()
                    } else if let TaskState::Failed(e) = &task.state {
                        e.clone()
                    } else {
                        format!("{} · {}", task.protocol.label(), task.save_dir.display())
                    };
                    p.text(
                        pos2(cell.left() + 8.0, cell.center().y),
                        Align2::LEFT_CENTER,
                        fit_text(ui, &desc, Metrics::FONT_ROW, cell.width() - 12.0),
                        FontId::proportional(Metrics::FONT_ROW),
                        dim_color,
                    );
                }
            }
            x += col.width;
        }

        // 行底分隔线
        if !selected {
            hline(
                ui.painter(),
                row.left(),
                row.right(),
                row.bottom() - 0.5,
                t.grid,
            );
        }
    }
}

/// 迷你速率折线（IDM 的 Transfer rate 列那种小图表）。
fn paint_spark(p: &Painter, rect: Rect, data: &VecDeque<u32>, color: Color32) {
    if data.len() < 2 {
        return;
    }
    let max = data.iter().copied().max().unwrap_or(0).max(1) as f32;
    let n = data.len();
    let pts: Vec<Pos2> = data
        .iter()
        .enumerate()
        .map(|(i, v)| {
            pos2(
                rect.left() + rect.width() * i as f32 / (n - 1) as f32,
                rect.bottom() - rect.height() * (*v as f32 / max),
            )
        })
        .collect();
    // 基线
    p.line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(160, 160, 160, 70)),
    );
    // 折线下方填一层半透明色，做出"面积图"的感觉（IDM 的 Transfer rate 小图）。
    // 用竖线填充而不是多边形：epaint 的多边形三角化只保证凸多边形正确。
    let fill = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 46);
    for pt in &pts {
        p.line_segment([*pt, pos2(pt.x, rect.bottom())], Stroke::new(1.0, fill));
    }
    p.add(Shape::line(pts, Stroke::new(1.2, color)));
}
