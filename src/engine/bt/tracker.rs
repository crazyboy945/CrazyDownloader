//! Tracker 通信：HTTP(S) tracker（BEP 3 / BEP 23）与 UDP tracker（BEP 15）。

use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::net::UdpSocket;

use super::bencode::Bencode;

/// tracker 请求超时。公网 tracker 经常性无响应，超时必须可控，
/// 否则整个任务会卡在单次 HTTP 请求上。
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(25);
/// UDP tracker 起始超时（BEP 15 建议 15s，实战 4s 起步更快）
const UDP_TIMEOUT: Duration = Duration::from_secs(4);
/// UDP tracker 最大重试次数（每次超时翻倍）
const UDP_TRIES: usize = 3;

/// 默认 tracker：magnet 链接没带 tracker 或 tracker 全挂时的兜底。
pub const DEFAULT_TRACKERS: &[&str] = &[
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.demonii.com:1337/announce",
    "udp://open.stealth.si:80/announce",
    "udp://tracker.openbittorrent.com:6969/announce",
    "udp://exodus.desync.com:6969/announce",
    "udp://tracker.torrent.eu.org:451/announce",
    "udp://tracker.moeking.me:6969/announce",
    "https://tracker.tamersunion.org:443/announce",
    "http://tracker.bt4g.com:2095/announce",
    "http://vps02.net.orel.ru:80/announce",
];

/// 构造带超时与 User-Agent 的 tracker HTTP 客户端。
///
/// 不少 tracker 会直接拒绝没有 UA 或使用默认 UA 的请求。
pub fn tracker_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent(concat!("rust-dm/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("构造 tracker HTTP 客户端失败")
}

/// 单个 peer 的地址与能力。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerAddr {
    pub ip: std::net::IpAddr,
    pub port: u16,
}

/// tracker 返回的响应。
#[derive(Debug, Clone, Default)]
pub struct AnnounceResult {
    pub peers: Vec<PeerAddr>,
    pub interval: u64,
    pub min_interval: u64,
    /// 从 peer 列表推断的种子数（BEP 48）
    pub complete: Option<u64>,
    /// 未完成数
    pub incomplete: Option<u64>,
}

/// 公告参数。
#[derive(Debug, Clone)]
pub struct AnnounceParams {
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub port: u16,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub event: &'static str, // started | stopped | none
    pub numwant: u32,
}

/// 向 tracker 公告。
pub async fn announce(
    tracker_url: &str,
    params: &AnnounceParams,
    client: &reqwest::Client,
) -> Result<AnnounceResult> {
    let lower = tracker_url.to_ascii_lowercase();
    if lower.starts_with("udp://") {
        return announce_udp(tracker_url, params).await;
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return announce_http(tracker_url, params, client).await;
    }
    bail!("不支持的 tracker 协议：{tracker_url}")
}

/// HTTP(S) tracker（BEP 3），支持 compact peer 格式。
async fn announce_http(
    tracker_url: &str,
    p: &AnnounceParams,
    client: &reqwest::Client,
) -> Result<AnnounceResult> {
    let url = build_http_url(tracker_url, p);
    // 无论调用方给的 client 有没有设超时，这里再兜一层，避免永久挂起
    let resp = tokio::time::timeout(HTTP_TIMEOUT, client.get(&url).send())
        .await
        .with_context(|| format!("tracker 请求超时：{tracker_url}"))?
        .with_context(|| format!("tracker 请求失败：{tracker_url}"))?;
    if !resp.status().is_success() {
        bail!("tracker 返回 {}", resp.status().as_u16());
    }
    let body = resp.bytes().await.context("读取 tracker 响应失败")?;
    parse_http_response(&body)
}

/// 拼装 HTTP tracker 的 announce URL。
///
/// 注意：每个参数之间都必须有 `&`，否则 `peer_id` 之后的参数会被并进上一个值里
/// （曾经的 bug：URL 变成 `...peer_id=XXXport=6881uploaded=0...`，tracker 直接拒绝）。
pub fn build_http_url(tracker_url: &str, p: &AnnounceParams) -> String {
    let params = [
        format!("info_hash={}", percent_encode(&p.info_hash)),
        format!("peer_id={}", percent_encode(&p.peer_id)),
        format!("port={}", p.port),
        format!("uploaded={}", p.uploaded),
        format!("downloaded={}", p.downloaded),
        format!("left={}", p.left),
        format!("event={}", p.event),
        format!("numwant={}", p.numwant),
        "compact=1".to_string(),
    ];
    let sep = if tracker_url.contains('?') { '&' } else { '?' };
    format!("{tracker_url}{sep}{}", params.join("&"))
}

/// 解析 HTTP tracker 响应。
pub fn parse_http_response(body: &[u8]) -> Result<AnnounceResult> {
    let (v, _) = Bencode::decode(body).context("解析 tracker 响应失败")?;

    // 错误检查
    if let Some(failure) = v.get("failure reason").and_then(|x| x.as_str()) {
        bail!("tracker 返回错误：{failure}");
    }

    let interval = v.get("interval").and_then(|x| x.as_int()).unwrap_or(1800) as u64;
    let min_interval = v
        .get("min interval")
        .and_then(|x| x.as_int())
        .unwrap_or(0) as u64;
    let complete = v.get("complete").and_then(|x| x.as_int()).map(|x| x as u64);
    let incomplete = v.get("incomplete").and_then(|x| x.as_int()).map(|x| x as u64);

    let mut peers = Vec::new();
    if let Some(raw) = v.get("peers") {
        match raw {
            // compact 格式：6 字节一个 peer（4 字节 IP + 2 字节端口）
            Bencode::Bytes(b) => {
                let mut i = 0usize;
                while i + 6 <= b.len() {
                    let ip = std::net::Ipv4Addr::new(b[i], b[i + 1], b[i + 2], b[i + 3]);
                    let port = u16::from_be_bytes([b[i + 4], b[i + 5]]);
                    peers.push(PeerAddr {
                        ip: ip.into(),
                        port,
                    });
                    i += 6;
                }
            }
            // 非 compact：peer 字典列表
            Bencode::List(items) => {
                for it in items {
                    if let Some(ip) = it.get("ip").and_then(|x| x.as_str()) {
                        let port = it.get("port").and_then(|x| x.as_int()).unwrap_or(0) as u16;
                        if let Ok(addr) = ip.parse::<std::net::IpAddr>() {
                            peers.push(PeerAddr { ip: addr, port });
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Ok(AnnounceResult {
        peers,
        interval,
        min_interval,
        complete,
        incomplete,
    })
}

/// UDP tracker（BEP 15）：connect + announce 两步握手。
pub async fn announce_udp(tracker_url: &str, p: &AnnounceParams) -> Result<AnnounceResult> {
    // "udp://" 是 6 个字符；曾经写成 `get(..7)`，导致所有 udp tracker 都被判为非法地址
    let rest = match tracker_url.get(..6) {
        Some(prefix) if prefix.eq_ignore_ascii_case("udp://") => &tracker_url[6..],
        _ => bail!("不是合法的 udp tracker 地址：{tracker_url}"),
    };
    let (hostport, _) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/announce"),
    };
    let addr: std::net::SocketAddr = tokio::net::lookup_host(hostport)
        .await
        .context("解析 tracker 地址失败")?
        .next()
        .context("tracker 无可用地址")?;

    let bind: std::net::SocketAddr = if addr.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    };
    let sock = UdpSocket::bind(bind).await?;
    sock.connect(addr).await?;

    // Step 1: connect request，protocol_id = 0x41727101980
    let transaction_id: u32 = rand::random();
    let mut req = Vec::with_capacity(16);
    req.extend_from_slice(&0x4172_7101_980i64.to_be_bytes());
    req.extend_from_slice(&transaction_id.to_be_bytes());

    let buf = udp_transaction(&sock, &req, transaction_id).await?;
    // udp_transaction 已校验 transaction_id，这里再确认 action=0（connect），
    // 避免把其它响应当成 conn_id
    let action = i32::from_be_bytes(buf[0..4].try_into().unwrap());
    if action != 0 {
        bail!(
            "UDP tracker connect 响应异常：action={}（tracker {hostport} 可能不是 UDP tracker）",
            action
        );
    }
    let conn_id = i64::from_be_bytes(buf[8..16].try_into().unwrap());

    // Step 2: announce request
    let announce_tid: u32 = rand::random();
    let mut req = Vec::with_capacity(98);
    req.extend_from_slice(&conn_id.to_be_bytes());
    req.extend_from_slice(&announce_tid.to_be_bytes());
    req.extend_from_slice(&p.info_hash);
    req.extend_from_slice(&p.peer_id);
    req.extend_from_slice(&p.downloaded.to_be_bytes());
    req.extend_from_slice(&p.left.to_be_bytes());
    req.extend_from_slice(&p.uploaded.to_be_bytes());
    req.extend_from_slice(&match p.event {
        "completed" => 2u32,
        "started" => 1u32,
        "stopped" => 3u32,
        _ => 0u32,
    }
    .to_be_bytes());
    req.extend_from_slice(&0u32.to_be_bytes()); // IP
    req.extend_from_slice(&0u32.to_be_bytes()); // key
    req.extend_from_slice(&(-1i32).to_be_bytes()); // num_want: 尽可能多
    req.extend_from_slice(&p.port.to_be_bytes());

    let resp = udp_transaction(&sock, &req, announce_tid).await?;
    parse_udp_response(&resp)
}

/// UDP 请求-响应：按 BEP 15 超时重试（超时逐次翻倍），并校验 transaction_id。
///
/// 直接 `recv()` 会在 tracker 不可达时永久挂起，必须带超时。
async fn udp_transaction(sock: &UdpSocket, req: &[u8], tid: u32) -> Result<Vec<u8>> {
    let mut wait = UDP_TIMEOUT;
    let mut last = anyhow::anyhow!("UDP tracker 无响应");
    for _ in 0..UDP_TRIES {
        if let Err(e) = sock.send(req).await {
            last = e.into();
            wait *= 2;
            continue;
        }
        let mut buf = vec![0u8; 2048];
        match tokio::time::timeout(wait, sock.recv(&mut buf)).await {
            Ok(Ok(n)) => {
                if n < 8 {
                    last = anyhow::anyhow!("UDP 响应过短（{n} 字节）");
                    wait *= 2;
                    continue;
                }
                let rtid = u32::from_be_bytes(buf[4..8].try_into().unwrap());
                // transaction_id 不匹配说明是迟到/串台的响应，继续等
                if rtid != tid {
                    wait *= 2;
                    continue;
                }
                // action=3 是错误响应，直接失败（connect 与 announce 共用此函数，
                // 因此这里只看 action，不做完整的 announce 解析）
                let action = i32::from_be_bytes(buf[0..4].try_into().unwrap());
                if action == 3 {
                    let msg = String::from_utf8_lossy(&buf[8..n]);
                    bail!("UDP tracker 错误：{msg}");
                }
                buf.truncate(n);
                return Ok(buf);
            }
            Ok(Err(e)) => last = e.into(),
            Err(_) => last = anyhow::anyhow!("UDP tracker 超时（{:?}）", wait),
        }
        wait *= 2;
    }
    Err(last)
}

/// 解析 UDP tracker 响应。
pub fn parse_udp_response(buf: &[u8]) -> Result<AnnounceResult> {
    if buf.len() < 8 {
        bail!("UDP 响应过短");
    }
    let action = i32::from_be_bytes(buf[0..4].try_into().unwrap());
    if action == 3 {
        let msg = String::from_utf8_lossy(&buf[8..]);
        bail!("UDP tracker 错误：{msg}");
    }
    if action != 1 {
        bail!("非预期的 UDP action：{action}");
    }
    if buf.len() < 20 {
        bail!("UDP announce 响应过短");
    }
    let interval = i32::from_be_bytes(buf[8..12].try_into().unwrap()).max(0) as u64;
    let leechers = i32::from_be_bytes(buf[12..16].try_into().unwrap()).max(0) as u64;
    let seeds = i32::from_be_bytes(buf[16..20].try_into().unwrap()).max(0) as u64;

    let mut peers = Vec::new();
    let body = &buf[20..];
    let mut i = 0usize;
    while i + 6 <= body.len() {
        let ip = std::net::Ipv4Addr::new(body[i], body[i + 1], body[i + 2], body[i + 3]);
        let port = u16::from_be_bytes([body[i + 4], body[i + 5]]);
        peers.push(PeerAddr {
            ip: ip.into(),
            port,
        });
        i += 6;
    }

    Ok(AnnounceResult {
        peers,
        interval,
        min_interval: 0,
        complete: Some(seeds),
        incomplete: Some(leechers),
    })
}

/// percent-encode 二进制数据（用于 info_hash / peer_id）。
fn percent_encode(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len() * 3);
    for &b in data {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                s.push(b as char)
            }
            _ => s.push_str(&format!("%{b:02X}")),
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_compact_peer_list() {
        let mut peers = Vec::new();
        peers.push(192u8);
        peers.push(168);
        peers.push(1);
        peers.push(10);
        peers.extend_from_slice(&6881u16.to_be_bytes());
        peers.push(10u8);
        peers.push(0);
        peers.push(0);
        peers.push(1);
        peers.extend_from_slice(&51413u16.to_be_bytes());

        let body = super::super::bencode::dict(vec![
            ("interval", Bencode::Int(1800)),
            ("complete", Bencode::Int(5)),
            ("peers", Bencode::Bytes(peers)),
        ])
        .encode();

        let r = parse_http_response(&body).unwrap();
        assert_eq!(r.peers.len(), 2);
        assert_eq!(r.peers[0].port, 6881);
        assert_eq!(r.peers[1].port, 51413);
        assert_eq!(r.complete, Some(5));
    }

    #[test]
    fn parses_non_compact_peer_list() {
        let body = super::super::bencode::dict(vec![(
            "peers",
            Bencode::List(vec![super::super::bencode::dict(vec![
                ("ip", super::super::bencode::bstr("10.0.0.5")),
                ("port", Bencode::Int(6969)),
            ])]),
        )])
        .encode();
        let r = parse_http_response(&body).unwrap();
        assert_eq!(r.peers.len(), 1);
        assert_eq!(r.peers[0].port, 6969);
    }

    #[test]
    fn surfaces_failure_reason() {
        let body = super::super::bencode::dict(vec![(
            "failure reason",
            super::super::bencode::bstr("tracker down"),
        )])
        .encode();
        let err = parse_http_response(&body).unwrap_err();
        assert!(err.to_string().contains("tracker down"));
    }

    #[test]
    fn parses_udp_announce_response() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&1i32.to_be_bytes()); // action = announce
        buf.extend_from_slice(&0i32.to_be_bytes()); // transaction
        buf.extend_from_slice(&1800i32.to_be_bytes()); // interval
        buf.extend_from_slice(&3i32.to_be_bytes()); // leechers
        buf.extend_from_slice(&7i32.to_be_bytes()); // seeds
        buf.extend_from_slice(&[127, 0, 0, 1]);
        buf.extend_from_slice(&6881u16.to_be_bytes());

        let r = parse_udp_response(&buf).unwrap();
        assert_eq!(r.interval, 1800);
        assert_eq!(r.complete, Some(7));
        assert_eq!(r.peers.len(), 1);
    }

    #[test]
    fn rejects_connect_error_action() {
        // action=3 表示 tracker 返回错误消息，不应被当成连接成功
        let mut buf = Vec::new();
        buf.extend_from_slice(&3i32.to_be_bytes());
        buf.extend_from_slice(&0i32.to_be_bytes());
        buf.extend_from_slice(b"go away");
        let err = parse_udp_response(&buf).unwrap_err();
        assert!(err.to_string().contains("go away"));
    }

    #[test]
    fn rejects_short_udp_response() {
        assert!(parse_udp_response(&[0u8, 1, 2]).is_err());
    }

    #[test]
    fn builds_url_with_encoded_info_hash() {
        let p = AnnounceParams {
            info_hash: [0u8; 20],
            peer_id: [1u8; 20],
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: 100,
            event: "started",
            numwant: 50,
        };
        let url = build_http_url("http://tr/announce", &p);
        assert!(url.starts_with("http://tr/announce?info_hash="));
        assert!(url.contains("compact=1"));
        assert!(url.contains("left=100"));
    }

    #[test]
    fn accepts_udp_tracker_urls() {
        // 回归测试：曾经用 `get(..7)` 判断前缀（"udp://" 只有 6 个字符），
        // 结果所有 udp tracker 都被当成非法地址，磁力链只能靠 magnet 自带 tracker。
        let p = AnnounceParams {
            info_hash: [0u8; 20],
            peer_id: [1u8; 20],
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: 0,
            event: "started",
            numwant: 50,
        };
        // 目标地址不可达也没关系：只要不是“非法地址”就说明前缀判断正确
        let err = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(announce("udp://127.0.0.1:1/announce", &p, &reqwest::Client::new()))
            .unwrap_err()
            .to_string();
        assert!(
            !err.contains("不是合法的 udp tracker"),
            "udp tracker 地址被误判：{err}"
        );
    }

    #[test]
    fn url_params_are_separated_by_ampersand() {
        // 回归测试：曾经 peer_id 之后的参数全部缺少 `&`，被拼进上一个值里，
        // tracker 收到的 peer_id 变成 "XXXport=6881uploaded=0..."，直接拒绝请求。
        let p = AnnounceParams {
            info_hash: [0xABu8; 20],
            peer_id: [0xCDu8; 20],
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: 100,
            event: "started",
            numwant: 50,
        };
        let url = build_http_url("http://tr/announce", &p);
        let query = url.split_once('?').unwrap().1;
        for param in [
            "info_hash=",
            "peer_id=",
            "&port=6881",
            "&uploaded=0",
            "&downloaded=0",
            "&left=100",
            "&event=started",
            "&numwant=50",
            "&compact=1",
        ] {
            assert!(query.contains(param), "URL 缺少 {param}：{url}");
        }
        assert_eq!(query.matches('&').count(), 8, "9 个参数应有 8 个 &：{url}");
        assert!(!query.contains("uploaded=0left"), "参数被粘连：{url}");
    }

    #[test]
    fn url_with_existing_query_uses_ampersand() {
        let p = AnnounceParams {
            info_hash: [0u8; 20],
            peer_id: [1u8; 20],
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: 0,
            event: "started",
            numwant: 50,
        };
        let url = build_http_url("http://tr/announce?passkey=abc", &p);
        assert!(url.starts_with("http://tr/announce?passkey=abc&info_hash="));
    }
}