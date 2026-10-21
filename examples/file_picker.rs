//! 单独验证系统文件/目录对话框（rfd）能否正常弹出。
//!
//! 用法：
//! ```text
//! cargo run --example file_picker -- torrent   # 选择 .torrent
//! cargo run --example file_picker -- folder    # 选择目录
//! ```
//!
//! 这只是排查工具：正常使用时对话框由界面上的「📄 种子文件」「浏览…」按钮触发。

fn main() {
    let which = std::env::args().nth(1).unwrap_or_else(|| "torrent".into());
    let start = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
    let picked = match which.as_str() {
        "folder" => rust_dm::ui::pick_folder(&start),
        _ => rust_dm::ui::pick_torrent_file(&start),
    };
    match picked {
        Some(p) => println!("已选择：{}", p.display()),
        None => println!("已取消（未选择任何文件）"),
    }
}
