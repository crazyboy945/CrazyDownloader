//! 单独探测 tracker 可达性：用给定 info_hash 向一组 tracker 公告，打印每个 tracker 的结果。
//!
//! 用法：
//! ```text
//! cargo run --example tracker_probe -- <info_hash_hex> [tracker_url ...]
//! ```
//! 不给 tracker 时使用内置默认列表。

use std::time::Instant;


use rust_dm::engine::bt::tracker::{announce, tracker_client, AnnounceParams, DEFAULT_TRACKERS};

fn parse_info_hash(s: &str) -> [u8; 20] {
    let s = s.trim();
    let hex = s.strip_prefix("urn:btih:").unwrap_or(s);
    if hex.len() == 40 {
        let mut out = [0u8; 20];
        for i in 0..20 {
            out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("info_hash 不是合法十六进制");
        }
        out
    } else if hex.len() == 32 {
        // base32 形式的 btih
        const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
        let up = hex.to_ascii_uppercase();
        let b = up.as_bytes();
        let mut bits = Vec::with_capacity(160);
        for &c in b {
            let v = ALPHA.iter().position(|&x| x == c).expect("base32 非法字符") as u8;
            for k in (0..5).rev() {
                bits.push((v >> k) & 1);
            }
        }
        let mut out = [0u8; 20];
        for i in 0..20 {
            let mut byte = 0u8;
            for k in 0..8 {
                byte = (byte << 1) | bits[i * 8 + k];
            }
            out[i] = byte;
        }
        out
    } else {
        panic!("info_hash 长度不对：{}", hex.len());
    }
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let info_hash = parse_info_hash(&args.next().unwrap_or_else(|| {
        // 默认用 Big Buck Bunny 的公开 infohash，仅用于探测 tracker 是否可达
        "dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c".to_string()
    }));
    let trackers: Vec<String> = if args.len() > 0 {
        args.collect()
    } else {
        DEFAULT_TRACKERS.iter().map(|s| s.to_string()).collect()
    };

    let peer_id: [u8; 20] = {
        let mut out = [0u8; 20];
        out[..8].copy_from_slice(b"-RD0001-");
        for b in out[8..].iter_mut() {
            *b = b'0' + (rand::random::<u8>() % 10);
        }
        out
    };

    let client = tracker_client().expect("构造 HTTP 客户端失败");
    println!("info_hash = {}", hex(&info_hash));
    println!("peer_id   = {}", String::from_utf8_lossy(&peer_id));
    println!("探测 {} 个 tracker\n", trackers.len());

    // 并发探测：串行时一个死 tracker 会拖满超时
    let futs = trackers.iter().map(|t| {
        let t = t.clone();
        let client = client.clone();
        async move {
            let p = AnnounceParams {
                info_hash,
                peer_id,
                port: 6881,
                uploaded: 0,
                downloaded: 0,
                left: 1_000_000,
                event: "started",
                numwant: 50,
            };
            let start = Instant::now();
            let r = tokio::time::timeout(std::time::Duration::from_secs(15), announce(&t, &p, &client)).await;
            (t, start.elapsed().as_millis(), r)
        }
    });
    let mut results = Vec::new();
    for item in futures_util::future::join_all(futs).await {
        results.push(item);
    }
    results.sort_by_key(|(_, ms, r)| (matches!(r, Ok(Ok(_))) == false, *ms));
    for (t, ms, r) in results {
        match r {
            Ok(Ok(r)) => println!(
                "  OK   {ms:>6}ms  peers={:<4} seeds={:?} leech={:?}  {t}",
                r.peers.len(),
                r.complete,
                r.incomplete,
            ),
            Ok(Err(e)) => println!("  FAIL {ms:>6}ms  {:<44} {t}", e.to_string()),
            Err(_) => println!("  TIMEOUT {ms:>6}ms  {:<44} {t}", "超过 15s 无结果"),
        }
    }

}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
