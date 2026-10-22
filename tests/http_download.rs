//! HTTP 下载引擎的端到端测试。
//!
//! 用一个最小 HTTP/1.1 服务器（支持 Range）验证：
//! - 多连接分片下载产出的字节与源文件完全一致
//! - 中断后按区间续传，不会留下空洞
//! - 服务端不支持 Range 时退化为单流下载

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rust_dm::common::limits::TokenBucket;
use rust_dm::engine::http::{HttpDownloader, HttpOptions};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// 测试用资源内容：伪随机但确定，便于校验字节级正确。
fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 31 + 7) % 251) as u8).collect()
}

/// 极简 HTTP/1.1 服务器：只服务固定资源，支持单区间 Range。
///
/// 返回监听地址与请求计数。关闭 `shutdown` 后 accept 循环退出。
async fn serve(
    body: Vec<u8>,
    support_range: bool,
    requests: Arc<AtomicUsize>,
) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        loop {
            let (mut sock, _) = tokio::select! {
                r = listener.accept() => r.unwrap(),
                _ = &mut rx => break,
            };
            let body = body.clone();
            let support_range = support_range;
            let requests = requests.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let mut raw = Vec::new();
                // 读到请求头结束
                loop {
                    let n = match sock.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    raw.extend_from_slice(&buf[..n]);
                    if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                requests.fetch_add(1, Ordering::Relaxed);
                let head = String::from_utf8_lossy(&raw).to_string();

                let range = if support_range {
                    head.lines()
                        .find_map(|l| {
                            let l = l.to_ascii_lowercase();
                            l.strip_prefix("range:")
                                .map(|v| v.trim().to_string())
                        })
                        .and_then(|v| v.strip_prefix("bytes=").map(|s| s.to_string()))
                } else {
                    None
                };

                let total = body.len() as u64;
                let (status, start, end) = match range {
                    Some(r) => {
                        let (s, e) = r.split_once('-').unwrap_or((r.as_str(), ""));
                        let s: u64 = s.trim().parse().unwrap_or(0);
                        let e: u64 = if e.trim().is_empty() {
                            total.saturating_sub(1)
                        } else {
                            e.trim().parse().unwrap_or(total - 1)
                        };
                        let e = e.min(total.saturating_sub(1));
                        if s > e || s >= total {
                            ("416 Range Not Satisfiable", 0, 0)
                        } else {
                            ("206 Partial Content", s, e)
                        }
                    }
                    None => ("200 OK", 0, total.saturating_sub(1)),
                };

                if status.starts_with("416") {
                    let resp = "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.flush().await;
                    return;
                }

                let slice = &body[start as usize..=end as usize];
                let mut head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    slice.len()
                );
                // 不支持 Range 的服务器不能声明 Accept-Ranges，否则客户端会误判
                if support_range {
                    head.push_str("Accept-Ranges: bytes\r\n");
                }
                if status.starts_with("206") {
                    head.push_str(&format!("Content-Range: bytes {start}-{end}/{total}\r\n"));
                }
                head.push_str("\r\n");

                let _ = sock.write_all(head.as_bytes()).await;
                // 分块写出，模拟真实网络的分片到达
                for c in slice.chunks(4096) {
                    if sock.write_all(c).await.is_err() {
                        break;
                    }
                }
                let _ = sock.flush().await;
                // 保持连接直到对端关闭（reqwest 复用连接）
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(300),
                    sock.read(&mut [0u8; 1]),
                )
                .await;
            });
        }
    });

    (format!("http://{addr}/file.bin"), tx)
}

fn opts(connections: usize) -> HttpOptions {
    HttpOptions {
        connections,
        retries: 2,
        timeout_secs: 20,
        resume: true,
        throttle_chunk: 64 * 1024,
    }
}

async fn run_download(url: &str, target: &Path, connections: usize) -> anyhow::Result<()> {
    let dl = HttpDownloader::new(opts(connections), Arc::new(TokenBucket::new()))?;
    dl.download(
        url,
        target,
        Arc::new(|_, _| {}),
        move |_m: String| {},
    )
    .await
}

fn tmpdir(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("rust-dm-test-{name}"));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// 多连接分片下载：结果必须与源字节完全一致，且不留 .part / 续传残留。
#[tokio::test]
async fn multi_connection_download_is_byte_exact() {
    let data = payload(300_000);
    let (url, _shutdown) = serve(data.clone(), true, Arc::new(AtomicUsize::new(0))).await;
    let dir = tmpdir("multi");
    let target = dir.join("file.bin");

    run_download(&url, &target, 8).await.unwrap();

    assert_eq!(std::fs::read(&target).unwrap(), data, "分片下载结果不一致");
    assert!(target.exists(), "正式文件应存在");
    assert!(!dir.join("file.bin.part").exists(), ".part 应已重命名");
    assert!(
        !dir.join("file.bin.rust-dm-resume").exists(),
        "完成后应清理续传元数据"
    );
}

/// 不支持 Range 的服务器：退化为单流下载，结果同样正确。
#[tokio::test]
async fn falls_back_to_single_stream_without_range() {
    let data = payload(50_000);
    let (url, _shutdown) = serve(data.clone(), false, Arc::new(AtomicUsize::new(0))).await;
    let dir = tmpdir("norange");
    let target = dir.join("file.bin");

    run_download(&url, &target, 8).await.unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), data);
}

/// 中断后续传：先取消一次留下残缺 .part，再下载一次仍要得到完整正确文件。
#[tokio::test]
async fn resumes_after_cancel_without_corruption() {
    let data = payload(400_000);
    let (url, _shutdown) = serve(data.clone(), true, Arc::new(AtomicUsize::new(0))).await;
    let dir = tmpdir("resume");
    let target = dir.join("file.bin");

    // 第一次：下载约 100 KB 后取消，留下部分完成的 .part 与续传元数据
    {
        let dl = HttpDownloader::new(opts(4), Arc::new(TokenBucket::new())).unwrap();
        let flag = dl.cancel_flag();
        let r = dl
            .download(
                &url,
                &target,
                Arc::new(move |done: u64, _total: u64| {
                    if done >= 100_000 {
                        flag.store(true, Ordering::Relaxed);
                    }
                }),
                |_m: String| {},
            )
            .await;
        assert!(r.is_err(), "取消后必须报错，不能伪装成成功");
        assert!(!target.exists(), "取消后不得生成正式文件");
        assert!(
            dir.join("file.bin.part").exists(),
            "取消后应保留 .part 供续传"
        );
    }

    // 中断后必须留下续传元数据，否则说明进度没落盘
    let resumed = std::fs::read_to_string(dir.join("file.bin.rust-dm-resume")).unwrap_or_default();
    assert!(
        resumed.starts_with("total=400000"),
        "应生成续传元数据，实际：{resumed:?}"
    );

    // 第二次：正常下载，续传元数据 + .part 应被复用并补齐
    let dl = HttpDownloader::new(opts(4), Arc::new(TokenBucket::new())).unwrap();
    dl.download(&url, &target, Arc::new(|_, _| {}), |_m: String| {})
        .await
        .unwrap();

    assert_eq!(
        std::fs::read(&target).unwrap(),
        data,
        "续传后文件内容必须完整正确"
    );
}

/// 续传元数据的区间规划：空洞必须被完整覆盖且不重叠。
#[test]
fn plan_covers_all_gaps() {
    use rust_dm::engine::http as h;
    // 借助公开接口间接验证：写一份续传元数据，读取后规划剩余分片
    let dir = tmpdir("plan");
    let target = dir.join("file.bin");
    h::write_resume_ranges(&target, 1000, &[(0, 199), (500, 599)]);
    let ranges = h::read_resume_ranges(&target, 1000);
    let covered: u64 = ranges.iter().map(|(s, e)| e - s + 1).sum();
    assert_eq!(covered, 300, "已完成区间总量应为 300");

    // 剩余分片必须恰好覆盖 [200,499] 与 [600,999]，不重不漏
    let (segs, done) = h::plan_remaining(1000, &ranges, 4);
    assert_eq!(done, 300, "已下载字节数应为 300");
    let seg_bytes: u64 = segs.iter().map(|s| s.len()).sum();
    assert_eq!(seg_bytes, 700, "剩余分片总量应为 700");
    let mut all: Vec<(u64, u64)> = segs.iter().map(|s| (s.start, s.end)).collect();
    all.sort_unstable();
    // 空洞之间本来就不连续（500-999 中的 600 起是新空洞），
    // 因此只要求「不重叠、不越界、恰好落在空洞内」
    for w in all.windows(2) {
        assert!(w[0].1 < w[1].0, "分片不得重叠：{:?}", w);
    }
    assert_eq!(all.first().unwrap().0, 200, "应从第一个空洞起点开始");
    assert_eq!(all.last().unwrap().1, 999, "应覆盖到文件末尾");
    for (s, e) in &all {
        let in_gap = (*s >= 200 && *e <= 499) || (*s >= 600 && *e <= 999);
        assert!(in_gap, "分片 {s}-{e} 落在已完成区域，会重复下载");
    }

    // 总大小不匹配时元数据必须作废
    let stale = h::read_resume_ranges(&target, 2000);
    assert!(stale.is_empty(), "总大小变化后续传信息应失效");
}

/// 限速器不能把下载卡死：小文件在限速下也要能完成。
#[tokio::test]
async fn completes_under_rate_limit() {
    let data = payload(20_000);
    let (url, _shutdown) = serve(data.clone(), true, Arc::new(AtomicUsize::new(0))).await;
    let dir = tmpdir("limited");
    let target = dir.join("file.bin");

    let dl = HttpDownloader::new(opts(2), Arc::new(TokenBucket::new_with_rate(500_000))).unwrap();
    dl.download(&url, &target, Arc::new(|_, _| {}), |_m: String| {})
        .await
        .unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), data);
}

/// 回归测试：目标文件已存在时（同名文件重复下载），
/// 下载到 100% 后**不能**因为"重命名覆盖失败"而报错。
///
/// Windows 的 rename 不会覆盖已存在的文件，必须先处理旧文件。
#[tokio::test]
async fn overwrites_existing_target_without_error() {
    let body = payload(64 * 1024);
    let requests = Arc::new(AtomicUsize::new(0));
    let (url, _stop) = serve(body.clone(), true, requests).await;

    let dir = tmpdir("overwrite");
    let target = dir.join("same.bin");
    // 模拟"之前已经下过一次"：目标文件已存在
    std::fs::write(&target, b"old content").unwrap();

    run_download(&url, &target, 4)
        .await
        .expect("同名文件已存在时也必须下载成功");

    let got = std::fs::read(&target).unwrap();
    assert_eq!(got.len(), body.len(), "应写入新下载的内容");
    assert_eq!(got, body, "内容应与源完全一致");
    // 旧文件不应被静默丢弃：要么保留为备份，要么明确被替换
    let _ = std::fs::remove_dir_all(&dir);
}
