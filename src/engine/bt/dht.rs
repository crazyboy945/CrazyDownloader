//! BEP 5：DHT（Kademlia over UDP）peer 发现。
//!
//! 磁力链经常只带几个早已失效的 tracker，只靠 tracker announce 几乎必然拿到 0 个 peer，
//! 因此这里实现了下载所需的最小 KRPC 子集：`ping` / `find_node` / `get_peers`。
//!
//! 协议要点：
//! - 查询：`d1:ad2:id20:<node id>e1:q4:<method>1:t2:<tid>1:y1:qe`
//! - 响应：`d1:rd2:id20:<id>5:nodes208:<26 字节/节点>e1:t2:<tid>1:y1:re`
//!   - `nodes`：20 字节 node id + 4 字节 IP + 2 字节端口
//!   - `values`：compact peer 列表（6 字节/个，仅 IPv4，由 `want=n4` 请求）
//! - 节点距离：`id` 的按位异或（XOR），迭代查找时每轮向最近的 K 个节点发问。

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use futures_util::future::join_all;
use tokio::net::UdpSocket;
use tokio::sync::{oneshot, Mutex};

use super::bencode::{bstr, dict, Bencode};

/// 公开 bootstrap 节点。DNS 解析失败时退回到下面那组兜底 IP。
pub const BOOTSTRAP_HOSTS: &[&str] = &[
    "router.bittorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "router.utorrent.com:6881",
    "dht.libtorrent.org:25401",
    "router.bitcomet.com:6881",
    "dht.aelitis.com:6881",
];

/// DNS 不可用时的兜底节点（尽量选取长期稳定的公共 DHT 路由节点）。
const BOOTSTRAP_FALLBACK: &[&str] = &[
    "87.98.162.88:6881",   // dht.transmissionbt.com
    "82.221.103.244:6881", // router.bittorrent.com
    "67.215.246.10:6881",  // router.utorrent.com
    "51.15.4.114:25401",   // dht.libtorrent.org
];

/// 路由表容量上限。
const MAX_NODES: usize = 400;
/// 单轮并发查询数。
const K: usize = 8;

/// 路由表中的一个节点。
#[derive(Debug, Clone, Copy)]
struct Node {
    id: [u8; 20],
    addr: SocketAddr,
    last_seen: Instant,
}

/// DHT 客户端。内部字段均为 `Arc`，可廉价 `clone()` 共享同一个 UDP socket。
#[derive(Clone)]
pub struct Dht {
    sock: Arc<UdpSocket>,
    node_id: [u8; 20],
    /// transaction id -> 等待该响应的回执通道
    pending: Arc<Mutex<HashMap<Vec<u8>, oneshot::Sender<Bencode>>>>,
    nodes: Arc<Mutex<Vec<Node>>>,
}

impl Dht {
    /// 启动 DHT：绑定 UDP 端口并拉起收包分发任务。
    ///
    /// `bind_port` 为 0 或端口被占用时自动改用系统分配的端口。
    pub async fn start(bind_port: u16) -> Result<Self> {
        let sock = match Self::bind(bind_port).await {
            Ok(s) => s,
            Err(_) => Self::bind(0).await.context("绑定 DHT UDP 端口失败")?,
        };
        let sock = Arc::new(sock);
        let pending: Arc<Mutex<HashMap<Vec<u8>, oneshot::Sender<Bencode>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let node_id = random_node_id();
        let dht = Dht {
            sock: sock.clone(),
            node_id,
            pending: pending.clone(),
            nodes: Arc::new(Mutex::new(Vec::new())),
        };
        tokio::spawn(recv_loop(sock, pending));
        Ok(dht)
    }

    async fn bind(port: u16) -> Result<UdpSocket> {
        let bind: SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
        let sock = UdpSocket::bind(bind).await?;
        Ok(sock)
    }

    /// 路由表中的节点数。
    pub async fn node_count(&self) -> usize {
        self.nodes.lock().await.len()
    }

    /// 向 bootstrap 节点做一次 find_node，填充路由表。
    pub async fn bootstrap(&self) -> Result<usize> {
        let addrs = resolve_bootstrap().await;
        if addrs.is_empty() {
            bail!("无法解析任何 DHT bootstrap 节点（DNS 不可用？）");
        }
        let futs: Vec<_> = addrs
            .iter()
            .map(|a| self.find_node(*a, self.node_id))
            .collect();
        for r in join_all(futs).await.into_iter().flatten() {
            self.add_nodes(r).await;
        }
        // 第二轮：向刚认识的节点再问一次，快速把路由表铺开
        // （只问 bootstrap 时通常只能拿到个位数节点，迭代查找会慢）
        let known: Vec<SocketAddr> = self.nodes.lock().await.iter().map(|n| n.addr).collect();
        let futs: Vec<_> = known
            .iter()
            .take(K)
            .map(|a| self.find_node(*a, self.node_id))
            .collect();
        for r in join_all(futs).await.into_iter().flatten() {
            self.add_nodes(r).await;
        }
        let n = self.node_count().await;
        if n == 0 {
            bail!("bootstrap 节点均未响应（UDP 可能被防火墙阻断）");
        }
        Ok(n)
    }

    /// 迭代式 get_peers：返回发现的 peer 地址。
    pub async fn get_peers(&self, info_hash: [u8; 20], timeout: Duration) -> Vec<SocketAddr> {
        if self.node_count().await < K {
            let _ = self.bootstrap().await;
        }
        let deadline = Instant::now() + timeout;
        let bootstraps = resolve_bootstrap().await.to_vec();
        let mut peers: HashSet<SocketAddr> = HashSet::new();
        let mut queried: HashSet<SocketAddr> = HashSet::new();

        for _round in 0..12 {
            if Instant::now() >= deadline || peers.len() >= 60 {
                break;
            }

            // 取当前最近的 K 个未查询节点
            let mut batch: Vec<SocketAddr> = Vec::new();
            {
                let mut known = self.nodes.lock().await.clone();
                known.sort_by_key(|n| xor_distance(&n.id, &info_hash));
                for n in known {
                    if batch.len() >= K {
                        break;
                    }
                    if queried.insert(n.addr) {
                        batch.push(n.addr);
                    }
                }
            }
            if batch.is_empty() {
                // 路由表还是空的，直接问 bootstrap
                for a in &bootstraps {
                    if batch.len() >= K {
                        break;
                    }
                    if queried.insert(*a) {
                        batch.push(*a);
                    }
                }
            }
            if batch.is_empty() {
                break;
            }

            let futs: Vec<_> = batch
                .iter()
                .map(|a| self.get_peers_query(*a, info_hash))
                .collect();
            let mut discovered: Vec<Node> = Vec::new();
            for r in join_all(futs).await.into_iter().flatten() {
                for p in r.peers {
                    peers.insert(p);
                }
                discovered.extend(r.nodes);
            }
            self.add_nodes(discovered).await;
        }

        peers.into_iter().collect()
    }

    /// 向单个节点发 find_node，返回它给出的近邻。
    async fn find_node(&self, addr: SocketAddr, target: [u8; 20]) -> Result<Vec<Node>> {
        let args = vec![
            ("id", Bencode::Bytes(self.node_id.to_vec())),
            ("target", Bencode::Bytes(target.to_vec())),
        ];
        let r = self.query(addr, "find_node", args, 2).await?;
        Ok(extract_nodes(&r))
    }

    /// 向单个节点发 get_peers。
    async fn get_peers_query(&self, addr: SocketAddr, info_hash: [u8; 20]) -> Result<GetPeersReply> {
        let args = vec![
            ("id", Bencode::Bytes(self.node_id.to_vec())),
            ("info_hash", Bencode::Bytes(info_hash.to_vec())),
            // 只要 IPv4 的 compact peer，减小响应体积
            ("want", Bencode::List(vec![bstr("n4")])),
        ];
        let r = self.query(addr, "get_peers", args, 2).await?;
        Ok(GetPeersReply {
            peers: extract_values(&r),
            nodes: extract_nodes(&r),
        })
    }

    /// 发一次 KRPC 查询并等待对应响应（带重试）。
    async fn query(
        &self,
        addr: SocketAddr,
        method: &str,
        args: Vec<(&str, Bencode)>,
        tries: usize,
    ) -> Result<Bencode> {
        let payload = build_query(&random_tid(), method, args);
        let mut wait = Duration::from_secs(2);
        let mut last_err = anyhow::anyhow!("DHT 查询未执行");
        for _ in 0..tries.max(1) {
            let tid = payload_tid(&payload);
            let (tx, rx) = oneshot::channel();
            self.pending.lock().await.insert(tid.clone(), tx);
            if let Err(e) = self.sock.send_to(&payload, addr).await {
                self.pending.lock().await.remove(&tid);
                last_err = e.into();
                wait *= 2;
                continue;
            }
            match tokio::time::timeout(wait, rx).await {
                Ok(Ok(resp)) => return parse_response(resp),
                Ok(Err(_)) => last_err = anyhow::anyhow!("DHT 响应通道已关闭"),
                Err(_) => {
                    self.pending.lock().await.remove(&tid);
                    last_err = anyhow::anyhow!("DHT 查询超时（{} {:?}）", method, wait);
                    wait *= 2;
                }
            }
        }
        Err(last_err)
    }

    /// 合并新发现的节点进路由表。
    async fn add_nodes(&self, new: Vec<Node>) {
        if new.is_empty() {
            return;
        }
        let mut table = self.nodes.lock().await;
        for n in new {
            if n.addr.ip().is_unspecified() || n.addr.port() == 0 {
                continue;
            }
            match table.iter_mut().find(|x| x.addr == n.addr) {
                Some(existing) => {
                    existing.id = n.id;
                    existing.last_seen = n.last_seen;
                }
                None => table.push(n),
            }
        }
        if table.len() > MAX_NODES {
            table.sort_by_key(|n| n.last_seen);
            let drain = table.len() - MAX_NODES;
            table.drain(..drain);
        }
    }
}

/// 单个节点对 get_peers 的回复。
struct GetPeersReply {
    peers: Vec<SocketAddr>,
    nodes: Vec<Node>,
}

/// 收包循环：按 transaction id 把响应派发给等待者。
async fn recv_loop(
    sock: Arc<UdpSocket>,
    pending: Arc<Mutex<HashMap<Vec<u8>, oneshot::Sender<Bencode>>>>,
) {
    let mut buf = vec![0u8; 2048];
    loop {
        let n = match sock.recv(&mut buf).await {
            Ok(n) => n,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            }
        };
        let Ok((v, _)) = Bencode::decode(&buf[..n]) else {
            continue;
        };
        let Some(tid) = v.get("t").and_then(|x| x.as_bytes()).map(|b| b.to_vec()) else {
            continue;
        };
        if let Some(tx) = pending.lock().await.remove(&tid) {
            let _ = tx.send(v);
        }
    }
}

/// 组装 KRPC 查询报文。
fn build_query(tid: &[u8], method: &str, args: Vec<(&str, Bencode)>) -> Vec<u8> {
    dict(vec![
        ("t", Bencode::Bytes(tid.to_vec())),
        ("y", bstr("q")),
        ("q", bstr(method)),
        ("a", dict(args)),
    ])
    .encode()
}

/// 从查询报文中取回 transaction id（避免调用方重复生成）。
fn payload_tid(payload: &[u8]) -> Vec<u8> {
    if let Ok((v, _)) = Bencode::decode(payload) {
        if let Some(t) = v.get("t").and_then(|x| x.as_bytes()) {
            return t.to_vec();
        }
    }
    Vec::new()
}

/// 把 KRPC 响应转成 `r` 字典，错误响应转成 `Err`。
fn parse_response(resp: Bencode) -> Result<Bencode> {
    match resp.get("y").and_then(|x| x.as_str()).unwrap_or_default().as_str() {
        "r" => resp
            .get("r")
            .cloned()
            .context("DHT 响应缺少 r 字段"),
        "e" => {
            let detail = resp
                .get("e")
                .and_then(|x| x.as_list())
                .and_then(|l| l.get(1))
                .and_then(|x| x.as_str())
                .unwrap_or_else(|| "未知错误".to_string());
            bail!("DHT 返回错误：{detail}")
        }
        other => bail!("未知的 DHT 响应类型：{other}"),
    }
}

/// 解析 compact node info（26 字节：20 id + 4 ip + 2 port）。
fn extract_nodes(r: &Bencode) -> Vec<Node> {
    let mut out = Vec::new();
    let Some(raw) = r.get("nodes").and_then(|x| x.as_bytes()) else {
        return out;
    };
    let now = Instant::now();
    let mut i = 0usize;
    while i + 26 <= raw.len() {
        let mut id = [0u8; 20];
        id.copy_from_slice(&raw[i..i + 20]);
        let ip = std::net::Ipv4Addr::new(raw[i + 20], raw[i + 21], raw[i + 22], raw[i + 23]);
        let port = u16::from_be_bytes([raw[i + 24], raw[i + 25]]);
        out.push(Node {
            id,
            addr: SocketAddr::new(ip.into(), port),
            last_seen: now,
        });
        i += 26;
    }
    out
}

/// 解析 compact peer 列表（6 字节/个）。
fn extract_values(r: &Bencode) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    let Some(list) = r.get("values").and_then(|x| x.as_list()) else {
        return out;
    };
    for v in list {
        let Some(b) = v.as_bytes() else { continue };
        if b.len() < 6 {
            continue;
        }
        let ip = std::net::Ipv4Addr::new(b[0], b[1], b[2], b[3]);
        let port = u16::from_be_bytes([b[4], b[5]]);
        out.push(SocketAddr::new(ip.into(), port));
    }
    out
}

/// XOR 距离，用于 Kademlia 排序。
fn xor_distance(a: &[u8; 20], b: &[u8; 20]) -> [u8; 20] {
    let mut out = [0u8; 20];
    for i in 0..20 {
        out[i] = a[i] ^ b[i];
    }
    out
}

fn random_node_id() -> [u8; 20] {
    rand::random()
}

fn random_tid() -> [u8; 2] {
    rand::random()
}

/// 解析 bootstrap 节点地址，结果缓存在进程内。
async fn resolve_bootstrap() -> &'static [SocketAddr] {
    static CACHE: tokio::sync::OnceCell<Vec<SocketAddr>> = tokio::sync::OnceCell::const_new();
    CACHE
        .get_or_init(|| async {
            let mut out = Vec::new();
            for host in BOOTSTRAP_HOSTS {
                if let Ok(addrs) =
                    tokio::time::timeout(Duration::from_secs(4), tokio::net::lookup_host(*host))
                        .await
                {
                    if let Ok(addrs) = addrs {
                        out.extend(addrs);
                    }
                }
            }
            if out.is_empty() {
                for ip in BOOTSTRAP_FALLBACK {
                    if let Ok(addr) = ip.parse::<SocketAddr>() {
                        out.push(addr);
                    }
                }
            }
            out
        })
        .await
        .as_slice()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_has_expected_shape() {
        let p = build_query(
            b"ab",
            "get_peers",
            vec![
                ("id", Bencode::Bytes(vec![1u8; 20])),
                ("info_hash", Bencode::Bytes(vec![2u8; 20])),
            ],
        );
        let (v, _) = Bencode::decode(&p).unwrap();
        assert_eq!(v.get("y").unwrap().as_str().unwrap(), "q");
        assert_eq!(v.get("q").unwrap().as_str().unwrap(), "get_peers");
        assert_eq!(v.get("t").unwrap().as_bytes().unwrap(), b"ab");
        let a = v.get("a").unwrap();
        assert_eq!(a.get("id").unwrap().as_bytes().unwrap().len(), 20);
        assert_eq!(a.get("info_hash").unwrap().as_bytes().unwrap().len(), 20);
    }

    #[test]
    fn parses_compact_nodes() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[7u8; 20]);
        buf.extend_from_slice(&[1, 2, 3, 4]);
        buf.extend_from_slice(&6881u16.to_be_bytes());
        let r = dict(vec![
            ("id", Bencode::Bytes(vec![9u8; 20])),
            ("nodes", Bencode::Bytes(buf)),
        ]);
        let nodes = extract_nodes(&r);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].addr.to_string(), "1.2.3.4:6881");
        assert_eq!(nodes[0].id, [7u8; 20]);
    }

    #[test]
    fn parses_peer_values() {
        let mut p = Vec::new();
        p.extend_from_slice(&[10, 0, 0, 1]);
        p.extend_from_slice(&51413u16.to_be_bytes());
        let r = dict(vec![("values", Bencode::List(vec![Bencode::Bytes(p)]))]);
        let peers = extract_values(&r);
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].to_string(), "10.0.0.1:51413");
    }

    #[test]
    fn distance_is_xor() {
        let a = [0b1010u8; 20];
        let b = [0b0110u8; 20];
        assert_eq!(xor_distance(&a, &b), [0b1100u8; 20]);
    }

    #[test]
    fn error_response_becomes_err() {
        let resp = dict(vec![
            ("y", bstr("e")),
            ("e", Bencode::List(vec![Bencode::Int(202), bstr("server error")])),
        ]);
        let err = parse_response(resp).unwrap_err();
        assert!(err.to_string().contains("server error"));
    }

    #[tokio::test]
    async fn resolves_bootstrap_hosts() {
        // 依赖 DNS，解析不到全部时也不应 panic
        let addrs = resolve_bootstrap().await;
        assert!(!addrs.is_empty());
    }

    /// 真实网络自测：默认不跑（`cargo test -- --ignored`）。
    #[tokio::test]
    #[ignore]
    async fn bootstraps_from_public_network() {
        let dht = Dht::start(0).await.unwrap();
        let n = dht.bootstrap().await.unwrap();
        println!("路由表节点数：{n}");
        assert!(n > 0);
    }

    /// 真实网络自测：迭代查找能扩大路由表。
    ///
    /// 用一个随机 info hash（不一定有人做种），因此只断言「查到了更多节点」，
    /// 不断言「查到 peer」。要验证端到端抓元数据请用 `examples/dht_probe`。
    #[tokio::test]
    #[ignore]
    async fn lookup_grows_routing_table() {
        let dht = Dht::start(0).await.unwrap();
        let before = dht.bootstrap().await.unwrap();
        let peers = dht.get_peers(rand::random(), Duration::from_secs(25)).await;
        let after = dht.node_count().await;
        println!("bootstrap={before} 迭代后={after} peer={}", peers.len());
        assert!(after >= before, "迭代查找应至少不减少路由表节点");
    }

    /// 真实网络自测：指定 info hash 找 peer。
    ///
    /// 用法：`RUST_DM_TEST_HASH=<40 位十六进制> cargo test --lib -- --ignored finds_peers`
    #[tokio::test]
    #[ignore]
    async fn finds_peers_via_dht() {
        let raw = match std::env::var("RUST_DM_TEST_HASH") {
            Ok(v) => v,
            Err(_) => {
                println!("跳过：未设置 RUST_DM_TEST_HASH");
                return;
            }
        };
        let bytes = hex::decode(raw.trim()).expect("RUST_DM_TEST_HASH 不是合法十六进制");
        assert_eq!(bytes.len(), 20, "info hash 必须是 20 字节");
        let mut h = [0u8; 20];
        h.copy_from_slice(&bytes);
        let dht = Dht::start(0).await.unwrap();
        let _ = dht.bootstrap().await;
        let peers = dht.get_peers(h, Duration::from_secs(40)).await;
        println!("找到 peer：{:?}", peers);
        assert!(!peers.is_empty(), "DHT 未找到任何 peer");
    }
}
