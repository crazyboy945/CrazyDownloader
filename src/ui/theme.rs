//! IDM 风格主题：配色与尺寸常量。
//!
//! 参考 Internet Download Manager 6.x 的经典浅色皮肤：
//! 浅灰窗口 + 大图标工具栏 + 白色列表 + 蓝色选中行 + 绿色"完成"状态。

use egui::Color32;

/// 配色。
#[derive(Clone, Copy)]
pub struct Theme {
    // 窗口与面板
    pub window_bg: Color32,
    pub menu_bg: Color32,
    /// 工具栏的上下渐变
    pub toolbar_top: Color32,
    pub toolbar_bottom: Color32,
    pub toolbar_border: Color32,

    // 工具栏按钮
    pub btn_hover: Color32,
    pub btn_hover_border: Color32,
    pub btn_active: Color32,
    pub btn_active_border: Color32,
    pub btn_disabled_alpha: u8,

    // 左侧分类面板
    pub sidebar_bg: Color32,
    pub sidebar_border: Color32,
    pub tree_selected: Color32,
    pub tree_hover: Color32,

    // 列表
    pub list_bg: Color32,
    pub header_bg: Color32,
    pub header_border: Color32,
    pub grid: Color32,
    pub row_hover: Color32,
    pub row_selected: Color32,
    pub row_selected_text: Color32,
    /// 下载中的行：进度底色的填充部分
    pub row_progress: Color32,
    pub row_progress_bg: Color32,

    // 底部状态栏
    pub status_bg: Color32,

    // 文字
    pub text: Color32,
    pub text_dim: Color32,
    pub text_dim_select: Color32,

    // 语义色
    pub ok: Color32,
    pub info: Color32,
    pub warn: Color32,
    pub err: Color32,
    pub accent: Color32,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            window_bg: Color32::from_rgb(240, 240, 240),
            menu_bg: Color32::from_rgb(248, 248, 248),
            toolbar_top: Color32::from_rgb(252, 252, 253),
            toolbar_bottom: Color32::from_rgb(232, 236, 240),
            toolbar_border: Color32::from_rgb(198, 203, 209),

            btn_hover: Color32::from_rgb(234, 243, 253),
            btn_hover_border: Color32::from_rgb(168, 200, 232),
            btn_active: Color32::from_rgb(212, 231, 248),
            btn_active_border: Color32::from_rgb(120, 168, 214),
            btn_disabled_alpha: 90,

            sidebar_bg: Color32::from_rgb(255, 255, 255),
            sidebar_border: Color32::from_rgb(212, 212, 212),
            tree_selected: Color32::from_rgb(206, 226, 246),
            tree_hover: Color32::from_rgb(238, 246, 253),

            list_bg: Color32::from_rgb(255, 255, 255),
            header_bg: Color32::from_rgb(242, 242, 242),
            header_border: Color32::from_rgb(207, 207, 207),
            grid: Color32::from_rgb(236, 236, 236),
            row_hover: Color32::from_rgb(240, 247, 255),
            row_selected: Color32::from_rgb(61, 127, 196),
            row_selected_text: Color32::from_rgb(255, 255, 255),
            // 进度底色要很淡：IDM 的进度只是给整行蒙一层浅色，
            // 太重会和"选中行"的蓝色撞在一起，看不出层级。
            row_progress: Color32::from_rgb(238, 246, 253),
            row_progress_bg: Color32::from_rgb(253, 254, 255),

            status_bg: Color32::from_rgb(240, 240, 240),

            text: Color32::from_rgb(24, 24, 24),
            text_dim: Color32::from_rgb(110, 110, 110),
            text_dim_select: Color32::from_rgb(226, 236, 248),

            ok: Color32::from_rgb(22, 138, 46),
            info: Color32::from_rgb(24, 105, 190),
            warn: Color32::from_rgb(190, 122, 0),
            err: Color32::from_rgb(198, 40, 40),
            accent: Color32::from_rgb(42, 111, 189),
        }
    }
}

/// 尺寸与字体。
pub struct Metrics;

impl Metrics {
    pub const MENU_H: f32 = 24.0;
    /// 大图标工具栏高度（32px 图标 + 文字 + 内边距）
    pub const TOOLBAR_H: f32 = 74.0;
    pub const TOOLBAR_ICON: f32 = 32.0;
    pub const TOOLBAR_BTN_W: f32 = 68.0;
    pub const SIDEBAR_W: f32 = 196.0;
    pub const TREE_ROW_H: f32 = 20.0;
    pub const LIST_HEADER_H: f32 = 22.0;
    pub const ROW_H: f32 = 20.0;
    pub const STATUS_H: f32 = 22.0;
    pub const FILE_ICON: f32 = 16.0;
    pub const STATUS_ICON: f32 = 12.0;

    pub const FONT_MENU: f32 = 13.0;
    pub const FONT_TOOLBAR: f32 = 11.0;
    pub const FONT_TREE: f32 = 12.0;
    pub const FONT_HEADER: f32 = 12.0;
    pub const FONT_ROW: f32 = 12.0;
    pub const FONT_STATUS: f32 = 12.0;
}
