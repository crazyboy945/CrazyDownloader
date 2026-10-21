//! Rust 下载工具 —— 多协议下载器（HTTP / HTTPS / FTP / BitTorrent）。
//!
//! GUI 使用 eframe/egui，布局参考经典 BitTorrent 客户端。

use std::sync::Arc;

use rust_dm::common::task::Config;
use rust_dm::engine;
use rust_dm::ui;

fn main() -> eframe::Result<()> {
    let config = Config::default();

    // 引擎跑在独立运行时里，UI 线程只读取快照
    let engine = match engine::Engine::new(config) {
        Ok(e) => Arc::new(e),
        Err(e) => {
            eprintln!("启动下载引擎失败：{e}");
            std::process::exit(1);
        }
    };

    // 后台速度统计。必须交给引擎自建的 tokio 运行时——
    // 此处还没有进入任何 runtime 上下文，直接 tokio::spawn 会 panic。
    engine::spawn_ticker(engine.state.clone());

    // 界面预览用：只在设置 RUST_DM_DEMO=1 时塞一批假任务，方便截图核对排版。
    if std::env::var("RUST_DM_DEMO").is_ok() {
        demo::seed(&engine);
    }

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Rust 下载工具 — rust-dm")
            .with_inner_size([1120.0, 680.0])
            .with_min_inner_size([860.0, 520.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };

    eframe::run_native(
        "Rust 下载工具",
        native_options,
        Box::new(move |cc| {
            // 必须在建窗口前挂好中文字体，否则界面全是方块
            ui::setup_fonts(&cc.egui_ctx);
            Ok(Box::new(ui::App::new(engine.clone())))
        }),
    )
}

/// 界面预览用的假数据（`RUST_DM_DEMO=1` 时启用）。
///
/// 直接往引擎状态里塞任务快照，不会真的发起下载；仅用于截图核对排版。
mod demo {
    use std::sync::{Arc, Mutex};

    use rust_dm::common::task::{Source, Task, TaskState};
    use rust_dm::engine::Engine;

    struct Sample {
        name: &'static str,
        total: u64,
        done: u64,
        state: u8, // 0 完成 1 下载中 2 暂停 3 失败 4 排队
        speed: u64,
        err: Option<&'static str>,
    }

    const SAMPLES: &[Sample] = &[
        Sample { name: "Winter Landscape - Desktop Wallpapers.zip", total: 1_310_720, done: 1_310_720, state: 0, speed: 0, err: None },
        Sample { name: "Firefox Setup 58.0.2.exe", total: 38_912_000, done: 38_912_000, state: 0, speed: 0, err: None },
        Sample { name: "Time Lapse Video Of Milky Way.mp4", total: 7_340_032, done: 7_340_032, state: 0, speed: 0, err: None },
        Sample { name: "video_4.mp4", total: 10_066_329, done: 10_066_329, state: 0, speed: 0, err: None },
        Sample { name: "video_6.mp4", total: 4_194_304, done: 4_194_304, state: 0, speed: 0, err: None },
        Sample { name: "Time Lapse Of Stars In The Night Sky.mkv", total: 8_493_056, done: 8_493_056, state: 0, speed: 0, err: None },
        Sample { name: "(11) GoPro Line of the Winter 2017.pdf", total: 15_925_248, done: 15_925_248, state: 0, speed: 0, err: None },
        Sample { name: "idm_jt.zip", total: 895_000_000, done: 895_000_000, state: 0, speed: 0, err: None },
        Sample { name: "idm_al.zip", total: 38_797_312, done: 38_797_312, state: 0, speed: 0, err: None },
        Sample { name: "Skype-8.55.0.141.exe", total: 69_763_072, done: 69_763_072, state: 0, speed: 0, err: None },
        Sample { name: "QuickTimeInstaller.exe", total: 41_943_040, done: 41_943_040, state: 0, speed: 0, err: None },
        Sample { name: "The Underground Subway Documentary.mp4", total: 48_234_496, done: 48_234_496, state: 0, speed: 0, err: None },
        Sample { name: "idman637build8beta.exe", total: 10_033_152, done: 3_200_000, state: 1, speed: 4_300_000, err: None },
        Sample { name: "watch.htm", total: 312_500_000, done: 312_500_000, state: 0, speed: 0, err: None },
        Sample { name: "10 Best Snowboarding Destinations.flac", total: 61_000_000, done: 57_000_000, state: 1, speed: 1_900_000, err: None },
        Sample { name: "GoPro-Let Me Take You.mp4", total: 68_000_000, done: 48_000_000, state: 1, speed: 760_000, err: None },
        Sample { name: "(11) GoPro-Line of the Winter.mp4", total: 53_000_000, done: 22_000_000, state: 2, speed: 0, err: None },
        Sample { name: "ubuntu-24.04.3-desktop-amd64.iso", total: 6_345_887_744, done: 1_200_000_000, state: 3, speed: 0, err: None },
    ];

    pub fn seed(engine: &Engine) {
        let cfg = engine.config();
        let mut s = engine.state.lock().unwrap();
        for (i, sample) in SAMPLES.iter().enumerate() {
            let url = format!("https://demo.invalid/{}", sample.name.replace(' ', "%20"));
            let src = Source::Http { url };
            let mut task = Task::new(
                s.next_id,
                sample.name.to_string(),
                src,
                cfg.download_dir.clone(),
                s.next_seq,
            );
            s.next_id += 1;
            s.next_seq += 1;
            task.total_bytes = sample.total;
            task.downloaded = sample.done;
            task.speed_down = sample.speed;
            task.state = match sample.state {
                0 => TaskState::Completed,
                1 => TaskState::Downloading,
                2 => TaskState::Paused,
                3 => match sample.err {
                    Some(e) => TaskState::Failed(e.to_string()),
                    None => TaskState::Failed("元数据获取超时：共尝试 120 个 peer".into()),
                },
                _ => TaskState::Queued,
            };
            task.eta = if task.speed_down > 0 {
                Some((task.total_bytes - task.downloaded) / task.speed_down)
            } else {
                None
            };
            task.last_message = match sample.state {
                0 => "下载完成".to_string(),
                1 => format!("已连接 {} 个 peer", 12 + i % 7),
                2 => "已暂停".to_string(),
                3 => "元数据获取超时".to_string(),
                _ => "排队中".to_string(),
            };
            let _ = i;
            s.tasks.push(Arc::new(Mutex::new(task)));
        }
    }
}
