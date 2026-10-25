//! Simon 与 Speck 分组密码（NSA 公开规范）。
//!
//! 只用到 128 位分组版本：
//! - Simon128/256，72 轮，Z4 序列 —— x-argus 的报文加密
//! - Speck128/256，34 轮 —— x-ladon 的报文加密
//!
//! 均为按公开规范独立实现。

const MASK: u64 = u64::MAX;

/// Simon 轮常量序列 Z4（62 位）。
const Z4: u64 = 0x3dc94c3a046d678b;

const SIMON_ROUNDS: usize = 72;
const SPECK_ROUNDS: usize = 34;

fn ror64(x: u64, n: u32) -> u64 {
    x.rotate_right(n & 63)
}

fn rol64(x: u64, n: u32) -> u64 {
    x.rotate_left(n & 63)
}

fn z4_bit(i: usize) -> u64 {
    (Z4 >> (i % 62)) & 1
}

/// Simon128/256 密钥扩展。key 为 4 个小端 u64。
pub fn simon_expand(key: &[u64; 4]) -> Vec<u64> {
    let mut k = Vec::with_capacity(SIMON_ROUNDS);
    k.extend_from_slice(key);
    for i in 4..SIMON_ROUNDS {
        let mut t = ror64(k[i - 1], 3);
        t ^= k[i - 3];
        t ^= ror64(t, 1);
        k.push((!k[i - 4] & MASK) ^ t ^ z4_bit(i - 4) ^ 3);
    }
    k
}

/// Simon 加密一个 128 位分组。
pub fn simon_encrypt(block: (u64, u64), ks: &[u64]) -> (u64, u64) {
    let (mut x, mut y) = (block.0, block.1);
    for &k in ks.iter().take(SIMON_ROUNDS) {
        let f = (rol64(y, 1) & rol64(y, 8)) ^ rol64(y, 2);
        let next = x ^ f ^ k;
        x = y;
        y = next;
    }
    (x, y)
}

/// Speck128/256 密钥扩展。key 为 4 个小端 u64。
pub fn speck_expand(key: &[u64; 4]) -> Vec<u64> {
    let mut ks = Vec::with_capacity(SPECK_ROUNDS);
    let mut ls = Vec::with_capacity(SPECK_ROUNDS);
    ks.push(key[0]);
    ls.extend_from_slice(&key[1..]);
    for i in 0..SPECK_ROUNDS - 1 {
        let new = ror64(ls[i], 8).wrapping_add(ks[i]) ^ (i as u64);
        ls.push(new);
        ks.push(new ^ rol64(ks[i], 3));
    }
    ks
}

/// Speck 加密一个 128 位分组。
pub fn speck_encrypt(block: (u64, u64), ks: &[u64]) -> (u64, u64) {
    let (mut y, mut x) = (block.0, block.1);
    for &k in ks.iter().take(SPECK_ROUNDS) {
        x = ror64(x, 8).wrapping_add(y) ^ k;
        y = rol64(y, 3) ^ x;
    }
    (y, x)
}

/// 把字节按小端两两拼成 u64，供分组函数使用。
pub fn le_words(bytes: &[u8], count: usize) -> Vec<u64> {
    bytes
        .chunks_exact(8)
        .take(count)
        .map(|c| u64::from_le_bytes(c.try_into().expect("分块长度固定为 8")))
        .collect()
}

pub fn pkcs7_pad(data: &[u8], block: usize) -> Vec<u8> {
    let n = block - data.len() % block;
    let mut out = data.to_vec();
    out.extend(std::iter::repeat_n(n as u8, n));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 参考向量由已验证的 Python 实现（fq_sign.py）生成，
    /// 该实现已用 x-argus 请求验证过可被服务端接受。
    #[test]
    fn simon128_256_matches_reference() {
        let key = [
            0x1f1e1d1c1b1a1918,
            0x131211100f0e0d0c,
            0x0b0a090807060504,
            0x0302010000000000,
        ];
        let ks = simon_expand(&key);
        assert_eq!(ks.len(), SIMON_ROUNDS);
        let pt = (0x657465726e616c20, 0x2e6f6a6563742070);
        let ct = simon_encrypt(pt, &ks);
        assert_eq!(ct.0, 0x8e980e428efa722c);
        assert_eq!(ct.1, 0xacd960a2937b492f);
    }

    #[test]
    fn speck128_256_matches_reference() {
        let mut k = [0u64; 4];
        for (i, w) in k.iter_mut().enumerate() {
            *w = u64::from_le_bytes(
                (0..8)
                    .map(|j| (i * 8 + j) as u8)
                    .collect::<Vec<_>>()
                    .try_into()
                    .unwrap(),
            );
        }
        let ks = speck_expand(&k);
        assert_eq!(ks.len(), SPECK_ROUNDS);
        let ct = speck_encrypt((0x0123456789abcdef, 0xfedcba9876543210), &ks);
        assert_eq!(ct.0, 0x722acab6a916661c);
        assert_eq!(ct.1, 0x8201d6353ca479f6);
    }

    #[test]
    fn pkcs7_pads_to_block() {
        assert_eq!(pkcs7_pad(b"abc", 16).len(), 16);
        assert_eq!(pkcs7_pad(&[0u8; 16], 16).len(), 32);
        assert_eq!(*pkcs7_pad(b"abc", 16).last().unwrap(), 13);
    }
}
