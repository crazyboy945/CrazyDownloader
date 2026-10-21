//! IDM 风格的大图标工具栏：图标在上、文字在下，整行浅灰渐变。

use egui::{
    pos2, vec2, Align2, Color32, CornerRadius, FontId, Painter, Rect, Sense, Stroke, StrokeKind,
};

use super::icons::{self, Icon};
use super::theme::Metrics;
use super::App;

/// 工具栏按钮对应的动作。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolAction {
    Add,
    Resume,
    Pause,
    PauseAll,
    Delete,
    ClearDone,
    Settings,
    OpenDir,
    Seed,
    Logs,
    StartAll,
    StopAll,
    About,
}

struct Item {
    icon: Icon,
    label: &'static str,
    action: ToolAction,
    needs_selection: bool,
}

const ITEMS: &[Item] = &[
    Item { icon: Icon::AddTask, label: "新建任务", action: ToolAction::Add, needs_selection: false },
    Item { icon: Icon::Resume, label: "继续", action: ToolAction::Resume, needs_selection: true },
    Item { icon: Icon::Stop, label: "暂停", action: ToolAction::Pause, needs_selection: true },
    Item { icon: Icon::StopAll, label: "全部暂停", action: ToolAction::PauseAll, needs_selection: false },
    Item { icon: Icon::Delete, label: "删除", action: ToolAction::Delete, needs_selection: true },
    Item { icon: Icon::ClearDone, label: "清除完成", action: ToolAction::ClearDone, needs_selection: false },
    Item { icon: Icon::Settings, label: "设置", action: ToolAction::Settings, needs_selection: false },
    Item { icon: Icon::OpenFolder, label: "打开目录", action: ToolAction::OpenDir, needs_selection: true },
    Item { icon: Icon::SeedFile, label: "种子文件", action: ToolAction::Seed, needs_selection: false },
    Item { icon: Icon::StartAll, label: "全部开始", action: ToolAction::StartAll, needs_selection: false },
    Item { icon: Icon::Logs, label: "日志", action: ToolAction::Logs, needs_selection: false },
    Item { icon: Icon::About, label: "关于", action: ToolAction::About, needs_selection: false },
];

impl App {
    pub(super) fn toolbar(&mut self, ctx: &egui::Context) {
        let t = self.theme;
        let has_sel = self.selected.is_some();
        let mut action: Option<ToolAction> = None;

        egui::TopBottomPanel::top("toolbar")
            .exact_height(Metrics::TOOLBAR_H)
            .frame(egui::Frame::NONE)
            .show(ctx, |ui| {
                let rect = ui.max_rect();
                paint_gradient(
                    ui.painter(),
                    rect,
                    t.toolbar_top,
                    t.toolbar_bottom,
                );
                ui.painter().line_segment(
                    [rect.left_bottom(), rect.right_bottom()],
                    Stroke::new(1.0, t.toolbar_border),
                );

                let mut x = rect.left() + 6.0;
                let btn_h = rect.height() - 10.0;
                for item in ITEMS {
                    let enabled = !item.needs_selection || has_sel;
                    let btn = Rect::from_min_size(
                        pos2(x, rect.top() + 5.0),
                        vec2(Metrics::TOOLBAR_BTN_W, btn_h),
                    );
                    let resp = ui
                        .interact(btn, ui.make_persistent_id(item.label), Sense::click())
                        .on_hover_text(item.label);

                    // 悬停 / 按下 背景
                    if enabled && resp.hovered() {
                        ui.painter().rect(
                            btn,
                            CornerRadius::same(3),
                            if resp.is_pointer_button_down_on() {
                                t.btn_active
                            } else {
                                t.btn_hover
                            },
                            Stroke::new(
                                1.0,
                                if resp.is_pointer_button_down_on() {
                                    t.btn_active_border
                                } else {
                                    t.btn_hover_border
                                },
                            ),
                            StrokeKind::Inside,
                        );
                    }

                    // 图标（32×32）
                    let icon_rect = Rect::from_center_size(
                        pos2(btn.center().x, btn.top() + 2.0 + Metrics::TOOLBAR_ICON / 2.0),
                        vec2(Metrics::TOOLBAR_ICON, Metrics::TOOLBAR_ICON),
                    );
                    if enabled {
                        icons::paint(ui.painter(), icon_rect, item.icon);
                    } else {
                        // 禁用态：整体降低不透明度
                        let mut p = ui.painter().clone();
                        p.set_opacity(0.40);
                        icons::paint(&p, icon_rect, item.icon);
                    }

                    // 文字
                    let color = if enabled {
                        t.text
                    } else {
                        Color32::from_rgb(160, 165, 172)
                    };
                    ui.painter().text(
                        pos2(btn.center().x, btn.bottom() - 9.0),
                        Align2::CENTER_CENTER,
                        item.label,
                        FontId::proportional(Metrics::FONT_TOOLBAR),
                        color,
                    );

                    if enabled && resp.clicked() {
                        action = Some(item.action);
                    }
                    x += Metrics::TOOLBAR_BTN_W + 2.0;
                }
            });

        if let Some(a) = action {
            self.dispatch(a);
        }
    }

    fn dispatch(&mut self, action: ToolAction) {
        match action {
            ToolAction::Add => self.add_open = true,
            ToolAction::Resume => {
                if let Some(id) = self.selected {
                    self.engine.resume_task(id);
                }
            }
            ToolAction::Pause => {
                if let Some(id) = self.selected {
                    self.engine.pause_task(id);
                }
            }
            ToolAction::PauseAll => self.engine.pause_all(),
            ToolAction::Delete => {
                if let Some(id) = self.selected {
                    self.engine.remove_task(id);
                    self.selected = None;
                }
            }
            ToolAction::ClearDone => self.engine.clear_finished(),
            ToolAction::Settings => {
                self.cfg_draft = self.engine.config();
                self.opts_open = true;
            }
            ToolAction::OpenDir => {
                if let Some(task) = self.selected_task() {
                    super::open_folder(&task.save_dir);
                }
            }
            ToolAction::Seed => self.open_torrent_picker(),
            ToolAction::Logs => self.log_open = !self.log_open,
            ToolAction::StartAll => self.engine.resume_all(),
            ToolAction::StopAll => self.engine.pause_all(),
            ToolAction::About => self.about_open = true,
        }
    }
}

/// 用若干横条模拟竖向渐变（egui 没有内置渐变填充）。
fn paint_gradient(painter: &Painter, rect: Rect, top: Color32, bottom: Color32) {
    const BANDS: usize = 24;
    for i in 0..BANDS {
        let t0 = i as f32 / BANDS as f32;
        let t1 = (i + 1) as f32 / BANDS as f32;
        let c = lerp_color(top, bottom, (t0 + t1) * 0.5);
        painter.rect_filled(
            Rect::from_min_max(
                pos2(rect.left(), rect.top() + rect.height() * t0),
                pos2(rect.right(), rect.top() + rect.height() * t1 + 0.5),
            ),
            CornerRadius::ZERO,
            c,
        );
    }
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}

