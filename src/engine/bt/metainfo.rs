//! .torrent 元数据与 magnet 链接解析。

use anyhow::{bail, Context, Result};
use sha1::{Digest, Sha1};

use super::bencode::Bencode;

/// 单个文件在种子中的位置信息。
#[derive(Debug, Clone)]
pub struct TorrentFile {
    /// 相对路径（torrent 内声明的路径，可能包含子目录）
    pub path: Vec<String>,
    pub length: u64,
}

/// 解析后的种子元数据。
#[derive(Debug, Clone)]
pub struct Metainfo {
    /// 20 字节 info hash
    pub info_hash: [u8; 20],
    pub name: String,
    pub piece_length: u32,
    /// 每个 piece 的 SHA1，长度应为 piece_count * 20
    pub pieces: Vec<u8>,
    pub total_length: u64,
    pub files: Vec<TorrentFile>,
    /// 多文件种子为 true
    pub is_multi: bool,
    pub announce: Vec<String>,
    /// 分层 tracker（BEP 12）
    pub announce_list: Vec<Vec<String>>,
    pub comment: Option<String>,
    pub created_by: Option<String>,
    pub creation_date: Option<i64>,
    /// URL-list（BEP 19）
    pub web_seeds: Vec<String>,
    /// private 标志（BEP 27），为 1 时不应向 DHT 报告
    pub is_private: bool,
    pub node: Option<String>,
}

impl Metainfo {
    /// 分片总数。
    pub fn piece_count(&self) -> usize {
        self.pieces.len() / 20
    }

    /// 第 index 个分片的哈希。
    pub fn piece_hash(&self, index: usize) -> Option<[u8; 20]> {
        if index >= self.piece_count() {
            return None;
        }
        let mut h = [0u8; 20];
        h.copy_from_slice(&self.pieces[index * 20..index * 20 + 20]);
        Some(h)
    }

    /// 某个分片在文件中的字节区间长度。
    pub fn piece_len(&self, index: usize) -> u32 {
        let start = index as u64 * self.piece_length as u64;
        if start >= self.total_length {
            return 0;
        }
        let remain = self.total_length - start;
        remain.min(self.piece_length as u64) as u32
    }

    /// 内容落盘后的根目录名（单文件时为文件名，多文件时为 torrent 名）。
    pub fn root_name(&self) -> String {
        self.name.clone()
    }

    /// 本地保存路径。
    pub fn save_path(&self, dir: &std::path::Path) -> std::path::PathBuf {
        dir.join(sanitize(&self.name))
    }

    /// 计算种子内容的 SHA1（校验已有文件）。
    pub fn expected_hashes(&self) -> Vec<[u8; 20]> {
        (0..self.piece_count())
            .filter_map(|i| self.piece_hash(i))
            .collect()
    }
}

fn sanitize(name: &str) -> String {
    crate::common::util::sanitize_file_name(name)
}

/// 解析 .torrent 文件内容。
///
/// 同时兼容 BEP 9 的裸 info 字典（ut_metadata 返回的元数据就是这样一段字节，
/// 没有外层的 announce / info 包装）。
pub fn parse_torrent(raw: &[u8]) -> Result<Metainfo> {
    // 先整体解析
    let (root, _) = Bencode::decode(raw).context("解析 .torrent 文件失败")?;
    let root = root.as_dict().context("torrent 顶层必须是字典")?;

    // 关键：info hash 必须是对 info 字典**原始字节**求 SHA1，
    // 因此这里重新在原始数据中定位 info 段的字节区间，而不是重新编码。
    let (info_hash, info, root) = match root.get(b"info".as_slice()) {
        Some(v) => {
            let info_span = find_info_span(raw)?;
            let mut hasher = Sha1::new();
            hasher.update(&raw[info_span.0..info_span.1]);
            let info_hash: [u8; 20] = hasher.finalize().into();
            (
                info_hash,
                v.as_dict().context("info 字段必须是字典")?,
                Some(root),
            )
        }
        // 裸 info 字典：整段字节就是 info，没有外层字段
        None => {
            let mut hasher = Sha1::new();
            hasher.update(raw);
            (hasher.finalize().into(), root, None)
        }
    };

    build_metainfo(info_hash, info, root)
}

/// 解析 BEP 9（ut_metadata）返回的裸 info 字典字节。
pub fn parse_info_dict(raw: &[u8]) -> Result<Metainfo> {
    parse_torrent(raw)
}

/// 从 info 字典构造 `Metainfo`。
///
/// `root` 为 `None` 时（裸 info 字典）跳过 announce / comment 等外层字段。
fn build_metainfo(
    info_hash: [u8; 20],
    info: &std::collections::BTreeMap<Vec<u8>, Bencode>,
    root: Option<&std::collections::BTreeMap<Vec<u8>, Bencode>>,
) -> Result<Metainfo> {
    let name = info
        .get(b"name".as_slice())
        .and_then(|v| v.as_str())
        .context("info 缺少 name")?;

    let piece_length = info
        .get(b"piece length".as_slice())
        .and_then(|v| v.as_int())
        .context("info 缺少 piece length")? as u32;
    if piece_length == 0 {
        bail!("piece length 不能为 0");
    }

    let pieces = info
        .get(b"pieces".as_slice())
        .and_then(|v| v.as_bytes())
        .context("info 缺少 pieces")?
        .to_vec();
    if pieces.len() % 20 != 0 || pieces.is_empty() {
        bail!("pieces 长度 {} 不是 20 的正整数倍", pieces.len());
    }

    // 单文件 / 多文件
    let (is_multi, total_length, files) = if let Some(list) = info.get(b"files".as_slice()) {
        let list = list.as_list().context("files 必须是列表")?;
        let mut files = Vec::with_capacity(list.len());
        let mut total = 0u64;
        for item in list {
            let d = item.as_dict().context("files 中的元素必须是字典")?;
            let length = d
                .get(b"length".as_slice())
                .and_then(|v| v.as_int())
                .context("文件缺少 length")? as u64;
            let path_items = d
                .get(b"path".as_slice())
                .and_then(|v| v.as_list())
                .context("文件缺少 path")?;
            let path: Vec<String> = path_items
                .iter()
                .map(|p| p.as_str().unwrap_or_default())
                .map(|p| sanitize(&p))
                .collect();
            total += length;
            files.push(TorrentFile { path, length });
        }
        (true, total, files)
    } else {
        let length = info
            .get(b"length".as_slice())
            .and_then(|v| v.as_int())
            .context("info 既没有 files 也没有 length")? as u64;
        (false, length, vec![TorrentFile {
            path: vec![sanitize(&name)],
            length,
        }])
    };

    // 分片数必须与总长度一致
    let expect_pieces = total_length.div_ceil(piece_length as u64) as usize;
    if pieces.len() / 20 != expect_pieces {
        bail!(
            "分片数不匹配：pieces 有 {} 个，内容需要 {} 个",
            pieces.len() / 20,
            expect_pieces
        );
    }

    // Tracker（裸 info 字典没有这些字段）
    let mut announce = Vec::new();
    let mut announce_list = Vec::new();
    let mut web_seeds = Vec::new();
    if let Some(root) = root {
        if let Some(a) = root.get(b"announce".as_slice()).and_then(|v| v.as_str()) {
            if !a.is_empty() {
                announce.push(a);
            }
        }
        if let Some(al) = root.get(b"announce-list".as_slice()).and_then(|v| v.as_list()) {
            for tier in al {
                let urls: Vec<String> = tier
                    .as_list()
                    .map(|l| l.iter().filter_map(|x| x.as_str()).collect())
                    .unwrap_or_default();
                if !urls.is_empty() {
                    announce_list.push(urls);
                }
            }
        }
        // announce 里没的补进 announce_list，反之亦然
        if !announce.is_empty() && announce_list.is_empty() {
            announce_list.push(announce.clone());
        }
        web_seeds = root
            .get(b"url-list".as_slice())
            .map(|v| match v {
                Bencode::Bytes(_) => v.as_str().into_iter().collect(),
                Bencode::List(l) => l.iter().filter_map(|x| x.as_str()).collect(),
                _ => Vec::new(),
            })
            .unwrap_or_default();
    }

    Ok(Metainfo {
        info_hash,
        name,
        piece_length,
        pieces,
        total_length,
        files,
        is_multi,
        announce,
        announce_list,
        comment: root.and_then(|r| r.get(b"comment".as_slice()).and_then(|v| v.as_str())),
        created_by: root.and_then(|r| r.get(b"created by".as_slice()).and_then(|v| v.as_str())),
        creation_date: root.and_then(|r| r.get(b"creation date".as_slice()).and_then(|v| v.as_int())),
        web_seeds,
        is_private: info
            .get(b"private".as_slice())
            .and_then(|v| v.as_int())
            .unwrap_or(0)
            == 1,
        node: root.and_then(|r| r.get(b"node".as_slice()).and_then(|v| v.as_str())),
    })
}

/// 在原始字节中定位 info 字典的 [start, end) 区间。
///
/// info hash 必须基于原始字节计算，重新编码可能改变字节表示。
fn find_info_span(raw: &[u8]) -> Result<(usize, usize)> {
    // 顶层是 d，然后反复解析 "key: value"
    if raw.first() != Some(&b'd') {
        bail!("torrent 顶层不是字典");
    }
    let mut pos = 1usize;
    loop {
        if raw.get(pos) == Some(&b'e') {
            bail!("torrent 中找不到 info 字段");
        }
        // 读 key
        let key_len = read_len_prefix(raw, &mut pos)?;
        let key_end = pos + key_len;
        if key_end > raw.len() {
            bail!("torrent 数据截断");
        }
        let is_info = &raw[pos..key_end] == b"info";
        pos = key_end;
        // 读 value
        let value_start = pos;
        skip_value(raw, &mut pos)?;
        if is_info {
            return Ok((value_start, pos));
        }
    }
}

fn read_len_prefix(raw: &[u8], pos: &mut usize) -> Result<usize> {
    let start = *pos;
    while raw.get(*pos).map(|c| *c != b':').unwrap_or(false) {
        *pos += 1;
    }
    if raw.get(*pos).is_none() {
        bail!("长度前缀未结束");
    }
    let s = std::str::from_utf8(&raw[start..*pos]).context("长度前缀不是 UTF-8")?;
    let n: usize = s.parse().context("非法长度前缀")?;
    *pos += 1; // ':'
    Ok(n)
}

fn skip_value(raw: &[u8], pos: &mut usize) -> Result<()> {
    match raw.get(*pos) {
        Some(b'i') => {
            while raw.get(*pos) != Some(&b'e') {
                *pos += 1;
                if *pos >= raw.len() {
                    bail!("整数未结束");
                }
            }
            *pos += 1;
        }
        Some(b'l') | Some(b'd') => {
            let mut depth = 0usize;
            loop {
                let c = *raw.get(*pos).context("容器未结束")?;
                match c {
                    b'l' | b'd' => depth += 1,
                    b'e' => {
                        depth -= 1;
                        if depth == 0 {
                            *pos += 1;
                            return Ok(());
                        }
                    }
                    b'i' => {
                        // 跳过整数
                        while raw.get(*pos) != Some(&b'e') {
                            *pos += 1;
                        }
                    }
                    b'0'..=b'9' => {
                        let n = read_len_prefix(raw, pos)?;
                        *pos += n;
                        continue;
                    }
                    _ => bail!("非法字节 '{}'", c as char),
                }
                *pos += 1;
                if *pos >= raw.len() {
                    bail!("容器未结束");
                }
            }
        }
        Some(b'0'..=b'9') => {
            let n = read_len_prefix(raw, pos)?;
            *pos += n;
            if *pos > raw.len() {
                bail!("字符串越界");
            }
        }
        _ => bail!("非法的 bencode 标记"),
    }
    Ok(())
}

/// 从 magnet 链接解析出的初始信息。
#[derive(Debug, Clone)]
pub struct MagnetInfo {
    pub info_hash: [u8; 20],
    /// 链接里给出的显示名（可能为空）
    pub name: Option<String>,
    pub trackers: Vec<String>,
    /// magnet 里直接给出的 peer（`x.pe=`，BEP 9），形如 `host:port`，逗号分隔
    pub peers: Vec<String>,
    /// magnet 里直接给出的完整元数据（xt=urn:btmh:）
    pub meta: Option<Bencode>,
}

/// 解析 `magnet:?xt=urn:btih:<hash>&dn=<name>&tr=<url>`。
///
/// info hash 支持两种编码：
/// - 40 位十六进制（十六进制串）
/// - 32 位 base32（RFC4648 大写）
pub fn parse_magnet(link: &str) -> Result<MagnetInfo> {
    if !link.trim_start().to_ascii_lowercase().starts_with("magnet:") {
        bail!("不是 magnet 链接");
    }
    let query = link
        .trim()
        .split_once(':')
        .map(|(_, q)| q)
        .unwrap_or("");
    // 去掉 magnet: 后的前导 '?'，否则第一个参数名会变成 "?xt" 而匹配不到。
    let query = query.strip_prefix('?').unwrap_or(query);

    let mut info_hash: Option<[u8; 20]> = None;
    let mut name: Option<String> = None;
    let mut trackers = Vec::new();
    let mut peers: Vec<String> = Vec::new();
    // v2 的 xt=urn:btmh:1220<sha256>（BEP 52），info hash 取 sha256 前 20 字节
    let mut v2_hash: Option<[u8; 20]> = None;
    // as= 参数内嵌的完整元数据（base32），xt=urn:btmh 时可用
    let mut meta: Option<Bencode> = None;

    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        // magnet 的查询串用 `+` 表示空格（application/x-www-form-urlencoded），
        // 直接调用通用解码会留下 `+`，导致 dn 参数出现 "Hello+World"。
        let v = percent_decode(&v.replace('+', " "));
        match k {
            "xt" => {
                let lower = v.to_ascii_lowercase();
                if let Some(h) = lower.strip_prefix("urn:btih:") {
                    if info_hash.is_none() {
                        info_hash = Some(decode_info_hash(h).context("解析 magnet info hash 失败")?);
                    }
                } else if let Some(h) = lower.strip_prefix("urn:btmh:") {
                    // multihash：1220 前缀 + 64 位十六进制（sha2-256）
                    if v2_hash.is_none() {
                        v2_hash = decode_btmh(h);
                    }
                }
            }
            "as" => {
                if meta.is_none() {
                    meta = decode_b32_bencode(&v);
                }
            }
            "dn" => name = Some(v),
            "tr" => trackers.push(v),
            // x.pe: 直接给出 peer 地址，tracker/DHT 都不可用时也能开工
            "x.pe" => {
                for part in v.split(',') {
                    let part = part.trim();
                    if !part.is_empty() {
                        peers.push(part.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    // 优先用 v1 info hash；仅有 v2 时退化取 sha256 前 20 字节
    let info_hash = match (info_hash, v2_hash) {
        (Some(h), _) => h,
        (None, Some(h)) => h,
        (None, None) => bail!("magnet 链接中缺少有效的 xt=urn:btih: 字段"),
    };
    if trackers.is_empty() {
        trackers.extend(
            super::tracker::DEFAULT_TRACKERS
                .iter()
                .map(|t| t.to_string()),
        );
    }

    Ok(MagnetInfo {
        info_hash,
        name,
        trackers,
        peers,
        meta,
    })
}

fn percent_decode(s: &str) -> String {
    crate::common::util::percent_decode(s)
}

/// 解码 info hash：40 位 hex 或 32 位 base32。
pub fn decode_info_hash(s: &str) -> Result<[u8; 20]> {
    let mut out = [0u8; 20];
    if s.len() == 40 {
        let bytes = hex::decode(s).context("info hash 不是合法十六进制")?;
        out.copy_from_slice(&bytes);
        Ok(out)
    } else if s.len() == 32 {
        // base32 (RFC4648)
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
        let mut bits = 0u32;
        let mut nbits = 0u32;
        let mut idx = 0usize;
        for c in s.bytes() {
            let upper = c.to_ascii_uppercase();
            let v = ALPHABET
                .iter()
                .position(|a| *a == upper)
                .ok_or_else(|| anyhow::anyhow!("非法 base32 字符 '{}'", c as char))?;
            bits = (bits << 5) | v as u32;
            nbits += 5;
            if nbits >= 8 {
                nbits -= 8;
                if idx < 20 {
                    out[idx] = ((bits >> nbits) & 0xFF) as u8;
                    idx += 1;
                }
            }
        }
        Ok(out)
    } else {
        bail!("info hash 长度应为 40（hex）或 32（base32），实际 {}", s.len())
    }
}

/// 解码 `1220<64位hex>` 形式的 sha2-256 multihash，取前 20 字节作为 v2 info hash。
fn decode_btmh(h: &str) -> Option<[u8; 20]> {
    if h.len() == 68 && h.starts_with("1220") {
        let digest = hex::decode(&h[4..]).ok()?;
        if digest.len() == 32 {
            let mut out = [0u8; 20];
            out.copy_from_slice(&digest[..20]);
            return Some(out);
        }
    }
    None
}

/// base32 解码（BEP 52 的 `as=` 参数）。
fn base32_decode(s: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    let mut bits = 0u32;
    let mut nbits = 0u32;
    for c in s.bytes() {
        let upper = c.to_ascii_uppercase();
        if upper == b'=' {
            break;
        }
        let v = ALPHABET.iter().position(|a| *a == upper)? as u32;
        bits = (bits << 5) | v;
        nbits += 5;
        if nbits >= 8 {
            nbits -= 8;
            out.push(((bits >> nbits) & 0xFF) as u8);
            // 只保留尚未取走的低位，否则高位残留会污染后续字节
            bits &= (1u32 << nbits) - 1;
        }
    }
    Some(out)
}

/// 解析 magnet `as=` 参数内嵌的 bencode 元数据。
fn decode_b32_bencode(s: &str) -> Option<Bencode> {
    let raw = base32_decode(s)?;
    let (v, _) = Bencode::decode(&raw).ok()?;
    // 必须是一个含 info 字段的字典才算有效元数据
    if v.as_dict()
        .map(|d| d.get(b"info".as_slice()).is_some())
        .unwrap_or(false)
    {
        Some(v)
    } else {
        None
    }
}

/// 随机生成 peer id（Azureus 风格 `-RD0100-`）。
pub fn random_peer_id() -> [u8; 20] {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut id = [0u8; 20];
    id[0] = b'-';
    id[1] = b'R';
    id[2] = b'D';
    id[3..7].copy_from_slice(b"0100");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // 用时间 + 进程 id 做伪随机，避免额外依赖
    let pid = std::process::id() as u64;
    let mut x = nanos ^ (pid << 32);
    for slot in id[7..].iter_mut() {
        // xorshift64*
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        *slot = (x.wrapping_mul(0x2545F4914F6CDD1D) >> 32) as u8;
    }
    id
}

/// 解析 magnet 中的 tracker 列表（BEP 12 的 announce-list 也走这里）。
pub fn trackers_of(meta: &Metainfo) -> Vec<String> {
    let mut out = Vec::new();
    for t in &meta.announce {
        if !t.is_empty() && !out.contains(t) {
            out.push(t.clone());
        }
    }
    for tier in &meta.announce_list {
        for t in tier {
            if !t.is_empty() && !out.contains(t) {
                out.push(t.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_torrent(piece_len: u32, lengths: &[u64]) -> Vec<u8> {
        let total: u64 = lengths.iter().sum();
        let count = total.div_ceil(piece_len as u64) as usize;
        let mut info = super::super::bencode::dict(vec![
            ("name", super::super::bencode::bstr("testdir")),
            ("piece length", Bencode::Int(piece_len as i64)),
            ("pieces", Bencode::Bytes(vec![0xAB; count * 20])),
        ]);
        if lengths.len() > 1 {
            let files: Vec<Bencode> = lengths
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    super::super::bencode::dict(vec![
                        ("length", Bencode::Int(*l as i64)),
                        (
                            "path",
                            Bencode::List(vec![super::super::bencode::bstr(&format!("f{i}.bin"))]),
                        ),
                    ])
                })
                .collect();
            if let Bencode::Dict(m) = &mut info {
                m.insert(b"files".to_vec(), Bencode::List(files));
            }
        } else {
            if let Bencode::Dict(m) = &mut info {
                m.insert(b"length".to_vec(), Bencode::Int(lengths[0] as i64));
            }
        }
        let root = super::super::bencode::dict(vec![
            ("announce", super::super::bencode::bstr("http://tr.example/announce")),
            ("info", info),
        ]);
        root.encode()
    }

    #[test]
    fn parses_single_file_torrent() {
        let raw = build_torrent(16384, &[40000]);
        let m = parse_torrent(&raw).unwrap();
        assert_eq!(m.name, "testdir");
        assert!(!m.is_multi);
        assert_eq!(m.total_length, 40000);
        assert_eq!(m.piece_count(), 3); // ceil(40000/16384)
        assert_eq!(m.announce[0], "http://tr.example/announce");
    }

    #[test]
    fn parses_multi_file_torrent() {
        let raw = build_torrent(32768, &[40000, 10000]);
        let m = parse_torrent(&raw).unwrap();
        assert!(m.is_multi);
        assert_eq!(m.total_length, 50000);
        assert_eq!(m.files.len(), 2);
        assert_eq!(m.piece_count(), 2);
        // 最后一个分片长度应被截断
        assert_eq!(m.piece_len(0), 32768);
        assert_eq!(m.piece_len(1), 17232);
    }

    #[test]
    fn rejects_inconsistent_pieces() {
        let raw = build_torrent(16384, &[40000]);
        // 人为破坏 pieces 长度
        let mut broken = raw.clone();
        if let Some(p) = broken.windows(6).position(|w| w == b"pieces") {
            // 不好精确定位，改为直接构造一个 pieces 数量错误的 torrent
            let bad = super::super::bencode::dict(vec![
                ("info", {
                    let mut d = super::super::bencode::dict(vec![
                        ("name", super::super::bencode::bstr("x")),
                        ("piece length", Bencode::Int(16384)),
                        ("pieces", Bencode::Bytes(vec![0u8; 20])),
                        ("length", Bencode::Int(100000)),
                    ]);
                    d
                }),
            ]);
            assert!(parse_torrent(&bad.encode()).is_err());
        }
        assert!(!broken.is_empty());
    }

    #[test]
    fn decodes_hex_and_base32_hashes() {
        let hex_h = "0102030405060708090a0b0c0d0e0f1011121314";
        let h = decode_info_hash(hex_h).unwrap();
        assert_eq!(h[0], 1);
        assert_eq!(h[19], 0x14);

        // base32("...") 与 hex 表示同一 info hash
        let b32 = "AEBAGBAFAYDQQCIKBMGA2DQPCAIREEYU";
        let h2 = decode_info_hash(b32).unwrap();
        assert_eq!(h2[0], 1);
        assert_eq!(h2[19], 0x14);
    }

    #[test]
    fn parses_magnet_link() {
        let link = "magnet:?xt=urn:btih:0102030405060708090a0b0c0d0e0f1011121314&dn=Hello+World&tr=http%3A%2F%2Ftr.example%2Fannounce";
        let m = parse_magnet(link).unwrap();
        assert_eq!(m.info_hash[0], 1);
        assert_eq!(m.name.as_deref(), Some("Hello World"));
        assert_eq!(m.trackers[0], "http://tr.example/announce");
    }

#[test]
fn magnet_without_hash_fails() {
assert!(parse_magnet("magnet:?dn=onlyname").is_err());
}

#[test]
fn magnet_x_pe_gives_direct_peers() {
    // x.pe（BEP 9）：发布者直接给出的 peer 地址，tracker/DHT 不可用时也能开工
    let link = "magnet:?xt=urn:btih:0102030405060708090a0b0c0d0e0f1011121314&x.pe=1.2.3.4:6881,5.6.7.8:51413";
    let m = parse_magnet(link).unwrap();
    assert_eq!(m.peers, vec!["1.2.3.4:6881".to_string(), "5.6.7.8:51413".to_string()]);
}

    #[test]
    fn decodes_base32_payload() {
        let raw = b"\x00\x01\x02\x03";
        let enc = super::super::bencode::base32_encode(raw);
        assert_eq!(base32_decode(&enc).unwrap(), raw);
        assert_eq!(base32_decode("AAAQEAY").unwrap(), vec![0, 1, 2, 3]);
        // 遇到 '=' 填充即停止
        assert_eq!(base32_decode("AAAQEAY======").unwrap(), vec![0, 1, 2, 3]);
        assert!(base32_decode("!!!").is_none());
    }

    #[test]
    fn decodes_btmh_v2_hash() {
        let h = format!("1220{}", "ab".repeat(32));
        let v = decode_btmh(&h).unwrap();
        assert_eq!(v, [0xabu8; 20]);
        assert!(decode_btmh("1220abcd").is_none());
    }

    #[test]
    fn magnet_with_embedded_metadata_is_used() {
        // as= 携带 base32 编码的 bencode 元数据，可跳过网络获取
        let info = super::super::bencode::dict(vec![
            ("name", super::super::bencode::bstr("from_as")),
            ("piece length", Bencode::Int(16384)),
            ("pieces", Bencode::Bytes(vec![0u8; 20])),
            ("length", Bencode::Int(16384)),
        ]);
        let root = super::super::bencode::dict(vec![("info", info)]);
        let enc = super::super::bencode::base32_encode(&root.encode());
        let link = format!(
            "magnet:?xt=urn:btih:0102030405060708090a0b0c0d0e0f1011121314&as={enc}"
        );
        let m = parse_magnet(&link).unwrap();
        let meta = m.meta.expect("应解析出内嵌元数据");
        let parsed = parse_torrent(&meta.encode()).unwrap();
        assert_eq!(parsed.name, "from_as");
    }

    #[test]
    fn magnet_space_in_name_is_decoded() {
        let link = "magnet:?xt=urn:btih:0102030405060708090a0b0c0d0e0f1011121314&dn=My+Movie+2024";
        let m = parse_magnet(link).unwrap();
        assert_eq!(m.name.as_deref(), Some("My Movie 2024"));
    }
}