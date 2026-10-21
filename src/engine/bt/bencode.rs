//! Bencode 编解码（BitTorrent 元数据格式）。
//!
//! 参考规范：<https://www.bittorrent.org/beps/bep_0003.html>

use anyhow::{bail, Result};
use std::collections::BTreeMap;

/// Bencode 值。
#[derive(Debug, Clone, PartialEq)]
pub enum Bencode {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Bencode>),
    Dict(BTreeMap<Vec<u8>, Bencode>),
}

/// base32 编码（RFC 4648 大写，无填充），
/// 用于 magnet 链接的 `as=` 参数内嵌元数据（BEP 52）。
pub fn base32_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    let mut bits = 0u32;
    let mut nbits = 0u32;
    for b in data {
        bits = (bits << 8) | *b as u32;
        nbits += 8;
        while nbits >= 5 {
            nbits -= 5;
            out.push(ALPHABET[((bits >> nbits) & 0x1F) as usize] as char);
        }
    }
    if nbits > 0 {
        out.push(ALPHABET[((bits << (5 - nbits)) & 0x1F) as usize] as char);
    }
    out
}

impl Bencode {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Bencode::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Bencode::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<String> {
        self.as_bytes().map(|b| String::from_utf8_lossy(b).into_owned())
    }

    pub fn as_list(&self) -> Option<&[Bencode]> {
        match self {
            Bencode::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&BTreeMap<Vec<u8>, Bencode>> {
        match self {
            Bencode::Dict(d) => Some(d),
            _ => None,
        }
    }

    /// 按 key 取字典字段。
    pub fn get(&self, key: &str) -> Option<&Bencode> {
        self.as_dict()?.get(key.as_bytes())
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Bencode::Int(i) => {
                out.push(b'i');
                out.extend_from_slice(i.to_string().as_bytes());
                out.push(b'e');
            }
            Bencode::Bytes(b) => {
                out.extend_from_slice(b.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(b);
            }
            Bencode::List(l) => {
                out.push(b'l');
                for v in l {
                    v.encode_into(out);
                }
                out.push(b'e');
            }
            Bencode::Dict(d) => {
                out.push(b'd');
                // BTreeMap 保证 key 按字节序排列，符合规范要求
                for (k, v) in d {
                    out.extend_from_slice(k.len().to_string().as_bytes());
                    out.push(b':');
                    out.extend_from_slice(k);
                    v.encode_into(out);
                }
                out.push(b'e');
            }
        }
    }

    /// 解析 bencode 字节流，返回第一个完整的值。
    pub fn decode(input: &[u8]) -> Result<(Bencode, usize)> {
        let mut p = Parser { data: input, pos: 0 };
        let v = p.parse_value()?;
        Ok((v, p.pos))
    }
}

/// 递归下降解析器。
struct Parser<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Result<u8> {
        self.data
            .get(self.pos)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("bencode 数据在偏移 {} 处意外结束", self.pos))
    }

    fn parse_value(&mut self) -> Result<Bencode> {
        match self.peek()? {
            b'i' => self.parse_int(),
            b'l' => self.parse_list(),
            b'd' => self.parse_dict(),
            b'0'..=b'9' => self.parse_bytes(),
            c => bail!("非法的 bencode 类型标记 '{}'（偏移 {}）", c as char, self.pos),
        }
    }

    fn parse_int(&mut self) -> Result<Bencode> {
        self.pos += 1; // 'i'
        let start = self.pos;
        while self.peek()? != b'e' {
            self.pos += 1;
        }
        let s = std::str::from_utf8(&self.data[start..self.pos])
            .map_err(|_| anyhow::anyhow!("整数部分不是合法 UTF-8"))?;
        let n: i64 = s
            .parse()
            .map_err(|_| anyhow::anyhow!("非法整数：{s}"))?;
        self.pos += 1; // 'e'
        Ok(Bencode::Int(n))
    }

    fn parse_bytes(&mut self) -> Result<Bencode> {
        let start = self.pos;
        while self.peek()? != b':' {
            self.pos += 1;
        }
        let len_s = std::str::from_utf8(&self.data[start..self.pos])
            .map_err(|_| anyhow::anyhow!("长度前缀不是合法 UTF-8"))?;
        let len: usize = len_s
            .parse()
            .map_err(|_| anyhow::anyhow!("非法字符串长度：{len_s}"))?;
        self.pos += 1; // ':'
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| anyhow::anyhow!("字符串长度溢出"))?;
        if end > self.data.len() {
            bail!("字符串内容不完整：需要 {len} 字节，实际剩余 {}", self.data.len() - self.pos);
        }
        let bytes = self.data[self.pos..end].to_vec();
        self.pos = end;
        Ok(Bencode::Bytes(bytes))
    }

    fn parse_list(&mut self) -> Result<Bencode> {
        self.pos += 1; // 'l'
        let mut items = Vec::new();
        while self.peek()? != b'e' {
            items.push(self.parse_value()?);
        }
        self.pos += 1; // 'e'
        Ok(Bencode::List(items))
    }

    fn parse_dict(&mut self) -> Result<Bencode> {
        self.pos += 1; // 'd'
        let mut map = BTreeMap::new();
        while self.peek()? != b'e' {
            let key = match self.parse_value()? {
                Bencode::Bytes(b) => b,
                _ => bail!("字典的 key 必须是字符串（偏移 {}）", self.pos),
            };
            let val = self.parse_value()?;
            map.insert(key, val);
        }
        self.pos += 1; // 'e'
        Ok(Bencode::Dict(map))
    }
}

/// 构造字典的便捷函数。
pub fn dict(pairs: Vec<(&str, Bencode)>) -> Bencode {
    let mut m = BTreeMap::new();
    for (k, v) in pairs {
        m.insert(k.as_bytes().to_vec(), v);
    }
    Bencode::Dict(m)
}

/// 便捷构造。
pub fn bstr(s: &str) -> Bencode {
    Bencode::Bytes(s.as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_simple() {
        let v = dict(vec![
            ("announce", bstr("http://tracker.example.com/announce")),
            ("length", Bencode::Int(12345)),
            ("name", bstr("test.bin")),
        ]);
        let bytes = v.encode();
        let (back, used) = Bencode::decode(&bytes).unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!(back, v);
    }

    #[test]
    fn decodes_nested_and_sorts_keys() {
        // 故意乱序 key，编码后应按键排序
        let v = dict(vec![
            ("zebra", Bencode::Int(1)),
            ("alpha", Bencode::List(vec![Bencode::Int(2), bstr("x")])),
        ]);
        let bytes = v.encode();
        assert_eq!(&bytes[..12], b"d5:alphali2e");
    }

    #[test]
    fn rejects_truncated_input() {
        assert!(Bencode::decode(b"d3:foo").is_err());
        assert!(Bencode::decode(b"i12").is_err());
    }

    #[test]
    fn handles_binary_strings_with_colons() {
        let v = Bencode::Bytes(b"a:b:c".to_vec());
        let bytes = v.encode();
        assert_eq!(bytes, b"5:a:b:c");
        let (back, _) = Bencode::decode(&bytes).unwrap();
        assert_eq!(back, v);
    }
}