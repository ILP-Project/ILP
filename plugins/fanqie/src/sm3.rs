//! SM3 哈希（GB/T 32905-2016）。
//!
//! 按国标独立实现，用于 x-argus 签名的摘要计算。

const IV: [u32; 8] = [
    0x7380166f, 0x4914b2b9, 0x172442d7, 0xda8a0600, 0xa96f30bc, 0x163138aa, 0xe38dee4d, 0xb0fb0e4e,
];

fn rotl(x: u32, n: u32) -> u32 {
    x.rotate_left(n & 31)
}

fn p0(x: u32) -> u32 {
    x ^ rotl(x, 9) ^ rotl(x, 17)
}

fn p1(x: u32) -> u32 {
    x ^ rotl(x, 15) ^ rotl(x, 23)
}

fn compress(v: &[u32; 8], block: &[u8]) -> [u32; 8] {
    let mut w = [0u32; 68];
    for i in 0..16 {
        w[i] = u32::from_be_bytes([
            block[i * 4],
            block[i * 4 + 1],
            block[i * 4 + 2],
            block[i * 4 + 3],
        ]);
    }
    for j in 16..68 {
        let t = p1(w[j - 16] ^ w[j - 9] ^ rotl(w[j - 3], 15));
        w[j] = t ^ rotl(w[j - 13], 7) ^ w[j - 6];
    }
    let mut wp = [0u32; 64];
    for j in 0..64 {
        wp[j] = w[j] ^ w[j + 4];
    }

    let (mut a, mut b, mut c, mut d) = (v[0], v[1], v[2], v[3]);
    let (mut e, mut f, mut g, mut h) = (v[4], v[5], v[6], v[7]);

    for j in 0..64 {
        let t: u32 = if j < 16 { 0x79cc4519 } else { 0x7a879d8a };
        let ss1 = rotl(
            rotl(a, 12).wrapping_add(e).wrapping_add(rotl(t, j as u32)),
            7,
        );
        let ss2 = ss1 ^ rotl(a, 12);
        let (ff, gg) = if j < 16 {
            (a ^ b ^ c, e ^ f ^ g)
        } else {
            ((a & b) | (a & c) | (b & c), (e & f) | ((!e) & g))
        };
        let tt1 = ff.wrapping_add(d).wrapping_add(ss2).wrapping_add(wp[j]);
        let tt2 = gg.wrapping_add(h).wrapping_add(ss1).wrapping_add(w[j]);
        d = c;
        c = rotl(b, 9);
        b = a;
        a = tt1;
        h = g;
        g = rotl(f, 19);
        f = e;
        e = p0(tt2);
    }

    [
        v[0] ^ a,
        v[1] ^ b,
        v[2] ^ c,
        v[3] ^ d,
        v[4] ^ e,
        v[5] ^ f,
        v[6] ^ g,
        v[7] ^ h,
    ]
}

pub fn sm3(data: &[u8]) -> [u8; 32] {
    let mut v = IV;
    let bit_len = (data.len() as u64).wrapping_mul(8);

    let mut padded = data.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.chunks(64) {
        v = compress(&v, chunk);
    }

    let mut out = [0u8; 32];
    for (i, word) in v.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::sm3;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn matches_gb_standard_vector() {
        assert_eq!(
            hex(&sm3(b"abc")),
            "66c7f0f462eeedd9d1f2d46bdc10e4e24167c4875cf2f7a2297da02b8f4ba8e0"
        );
    }

    #[test]
    fn matches_standard_512_bit_vector() {
        let input = b"abcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcd";
        assert_eq!(
            hex(&sm3(input)),
            "debe9ff92275b8a138604889c18e5a4d6fdb70e5387e5765293dcba39c0c5732"
        );
    }

    #[test]
    fn matches_reference_for_64_bytes() {
        let input: Vec<u8> = (0..64u8).collect();
        assert_eq!(
            hex(&sm3(&input)),
            "93566f236d157aae078d1ddb5cebdbba1520b5142e22a8915564345ba2ae1d63"
        );
    }

    #[test]
    fn empty_input_is_stable() {
        assert_eq!(
            hex(&sm3(b"")),
            "1ab21d8355cfa17f8e61194831e81a8f22bec8c728fefb747ed035eb5082aa2b"
        );
    }
}
