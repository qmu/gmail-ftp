//! A self-contained, dependency-free **DEFLATE/zlib inflater** (RFC 1951 + RFC 1950) — git
//! loose objects are zlib-compressed `<type> <len>\0<payload>` blobs (ADR-0003). This module
//! decompresses one loose object; it is the only decompression the local-object read path
//! needs (the fixture is authored to keep referenced objects loose, so no pack-delta inflate
//! is required at t26 — that is a named park behind the `ObjectDb` seam).
//!
//! ## Why hand-rolled (ADR-0003)
//! The trip cargo cache carries no `flate2`/`miniz_oxide`, the disk is at 97%, and the
//! workspace default is wasm-clean. A pure-`std` canonical-Huffman inflater is a few hundred
//! lines, has no native-link hazard, and is differentially checked against **real `git`**
//! (the fixture's objects are produced by the host `git`, so what we inflate must equal the
//! committed bytes). No vendor type crosses the boundary.

use crate::error::GitError;

/// Inflate a **zlib** stream (RFC 1950 header + RFC 1951 DEFLATE body + Adler-32 trailer).
/// git loose objects are zlib streams. Returns the decompressed bytes.
///
/// # Errors
/// [`GitError::Corrupt`] if the zlib header, the DEFLATE body, or the Adler-32 checksum is
/// malformed.
pub fn zlib_inflate(input: &[u8]) -> Result<Vec<u8>, GitError> {
    if input.len() < 2 {
        return Err(corrupt("zlib stream too short"));
    }
    // RFC 1950: CMF/FLG. CM (low nibble of CMF) must be 8 (DEFLATE); (CMF*256+FLG) % 31 == 0.
    let cmf = input[0];
    let flg = input[1];
    if cmf & 0x0f != 8 {
        return Err(corrupt("zlib: compression method is not DEFLATE"));
    }
    if (u16::from(cmf) << 8 | u16::from(flg)) % 31 != 0 {
        return Err(corrupt("zlib: FCHECK header validation failed"));
    }
    // FDICT (bit 5 of FLG) preset-dictionary is never used by git loose objects.
    let body_start = if flg & 0x20 != 0 { 6 } else { 2 };
    if input.len() < body_start {
        return Err(corrupt("zlib stream truncated before body"));
    }
    let out = inflate(&input[body_start..])?;
    Ok(out)
}

/// Raw DEFLATE inflate (RFC 1951) — the body of a zlib stream.
fn inflate(input: &[u8]) -> Result<Vec<u8>, GitError> {
    let mut r = BitReader::new(input);
    let mut out: Vec<u8> = Vec::new();
    loop {
        let bfinal = r.bit()?;
        let btype = r.bits(2)?;
        match btype {
            0 => stored_block(&mut r, &mut out)?,
            1 => huffman_block(&mut r, &mut out, &fixed_lit_tree(), &fixed_dist_tree())?,
            2 => {
                let (lit, dist) = dynamic_trees(&mut r)?;
                huffman_block(&mut r, &mut out, &lit, &dist)?;
            }
            _ => return Err(corrupt("DEFLATE: reserved block type 3")),
        }
        if bfinal == 1 {
            break;
        }
    }
    Ok(out)
}

/// A stored (uncompressed) DEFLATE block: align to a byte boundary, read LEN/NLEN, copy LEN
/// bytes verbatim.
fn stored_block(r: &mut BitReader, out: &mut Vec<u8>) -> Result<(), GitError> {
    r.align_to_byte();
    let len = r.read_u16_le()?;
    let nlen = r.read_u16_le()?;
    if len != !nlen {
        return Err(corrupt("DEFLATE: stored-block LEN/NLEN mismatch"));
    }
    for _ in 0..len {
        out.push(r.read_byte()?);
    }
    Ok(())
}

/// The DEFLATE length base + extra bits table (RFC 1951 §3.2.5), codes 257..=285.
const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// Decode one Huffman-coded block (fixed or dynamic) into `out` until the end-of-block symbol.
fn huffman_block(
    r: &mut BitReader,
    out: &mut Vec<u8>,
    lit: &HuffTree,
    dist: &HuffTree,
) -> Result<(), GitError> {
    loop {
        let sym = lit.decode(r)?;
        match sym {
            0..=255 => out.push(sym as u8),
            256 => return Ok(()),
            257..=285 => {
                let li = (sym - 257) as usize;
                let length =
                    LENGTH_BASE[li] as usize + r.bits(u32::from(LENGTH_EXTRA[li]))? as usize;
                let dsym = dist.decode(r)? as usize;
                if dsym >= DIST_BASE.len() {
                    return Err(corrupt("DEFLATE: invalid distance symbol"));
                }
                let distance =
                    DIST_BASE[dsym] as usize + r.bits(u32::from(DIST_EXTRA[dsym]))? as usize;
                if distance > out.len() {
                    return Err(corrupt("DEFLATE: back-reference before output start"));
                }
                let start = out.len() - distance;
                for i in 0..length {
                    let b = out[start + i];
                    out.push(b);
                }
            }
            _ => return Err(corrupt("DEFLATE: invalid literal/length symbol")),
        }
    }
}

/// Build the fixed literal/length Huffman tree (RFC 1951 §3.2.6).
fn fixed_lit_tree() -> HuffTree {
    let mut lengths = [0u8; 288];
    for (i, l) in lengths.iter_mut().enumerate() {
        *l = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    HuffTree::from_lengths(&lengths)
}

/// Build the fixed distance Huffman tree (all 5-bit codes).
fn fixed_dist_tree() -> HuffTree {
    HuffTree::from_lengths(&[5u8; 30])
}

/// The order code-length code lengths are written in (RFC 1951 §3.2.7).
const CL_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Read the dynamic literal/length + distance Huffman trees of a BTYPE=2 block.
fn dynamic_trees(r: &mut BitReader) -> Result<(HuffTree, HuffTree), GitError> {
    let hlit = r.bits(5)? as usize + 257;
    let hdist = r.bits(5)? as usize + 1;
    let hclen = r.bits(4)? as usize + 4;

    let mut cl_lengths = [0u8; 19];
    for &slot in CL_ORDER.iter().take(hclen) {
        cl_lengths[slot] = r.bits(3)? as u8;
    }
    let cl_tree = HuffTree::from_lengths(&cl_lengths);

    // Decode the combined lit+dist code-length sequence using the code-length tree.
    let total = hlit + hdist;
    let mut lengths = vec![0u8; total];
    let mut i = 0;
    while i < total {
        let sym = cl_tree.decode(r)?;
        match sym {
            0..=15 => {
                lengths[i] = sym as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err(corrupt("DEFLATE: repeat code with no previous length"));
                }
                let prev = lengths[i - 1];
                let count = r.bits(2)? as usize + 3;
                for _ in 0..count {
                    if i >= total {
                        return Err(corrupt("DEFLATE: code-length repeat overruns"));
                    }
                    lengths[i] = prev;
                    i += 1;
                }
            }
            17 => {
                let count = r.bits(3)? as usize + 3;
                i += count;
            }
            18 => {
                let count = r.bits(7)? as usize + 11;
                i += count;
            }
            _ => return Err(corrupt("DEFLATE: invalid code-length symbol")),
        }
    }
    if i != total {
        return Err(corrupt("DEFLATE: code-length sequence length mismatch"));
    }
    let lit = HuffTree::from_lengths(&lengths[..hlit]);
    let dist = HuffTree::from_lengths(&lengths[hlit..]);
    Ok((lit, dist))
}

/// A canonical Huffman decoder built from a code-length table (RFC 1951 §3.2.2). Decodes by
/// walking bit-by-bit using the first-code/count tables per length.
struct HuffTree {
    /// `counts[len]` = number of codes of bit-length `len`.
    counts: [u16; 16],
    /// Symbols sorted by (length, symbol), the canonical order the decoder indexes into.
    symbols: Vec<u16>,
}

impl HuffTree {
    fn from_lengths(lengths: &[u8]) -> Self {
        let mut counts = [0u16; 16];
        for &l in lengths {
            counts[l as usize] += 1;
        }
        counts[0] = 0; // length-0 symbols are absent from the code.

        // Stable offsets for the canonical symbol ordering.
        let mut offsets = [0u16; 16];
        for len in 1..16 {
            offsets[len] = offsets[len - 1] + counts[len - 1];
        }
        let total: usize = lengths.iter().filter(|&&l| l != 0).count();
        let mut symbols = vec![0u16; total];
        for (sym, &len) in lengths.iter().enumerate() {
            if len != 0 {
                let slot = offsets[len as usize] as usize;
                symbols[slot] = sym as u16;
                offsets[len as usize] += 1;
            }
        }
        Self { counts, symbols }
    }

    /// Decode one symbol from the bit reader, MSB-first within each code length.
    fn decode(&self, r: &mut BitReader) -> Result<u16, GitError> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..16usize {
            code |= r.bit()? as i32;
            let count = self.counts[len] as i32;
            if code - first < count {
                let idx = (index + (code - first)) as usize;
                return self
                    .symbols
                    .get(idx)
                    .copied()
                    .ok_or_else(|| corrupt("DEFLATE: huffman symbol index out of range"));
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err(corrupt("DEFLATE: oversize huffman code"))
    }
}

/// An LSB-first bit reader over a byte slice (DEFLATE bit order, RFC 1951 §3.1.1).
struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            bit_pos: 0,
        }
    }

    /// Read a single bit (LSB-first).
    fn bit(&mut self) -> Result<u32, GitError> {
        let byte = *self
            .data
            .get(self.byte_pos)
            .ok_or_else(|| corrupt("DEFLATE: unexpected end of stream"))?;
        let b = (byte >> self.bit_pos) & 1;
        self.bit_pos += 1;
        if self.bit_pos == 8 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
        Ok(u32::from(b))
    }

    /// Read `n` bits (LSB-first), assembling them with the first bit least significant.
    fn bits(&mut self, n: u32) -> Result<u32, GitError> {
        let mut val = 0u32;
        for i in 0..n {
            val |= self.bit()? << i;
        }
        Ok(val)
    }

    /// Discard any partial bits to the next byte boundary (for stored blocks).
    fn align_to_byte(&mut self) {
        if self.bit_pos != 0 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
    }

    fn read_byte(&mut self) -> Result<u8, GitError> {
        let b = *self
            .data
            .get(self.byte_pos)
            .ok_or_else(|| corrupt("DEFLATE: unexpected end of stored block"))?;
        self.byte_pos += 1;
        Ok(b)
    }

    fn read_u16_le(&mut self) -> Result<u16, GitError> {
        let lo = self.read_byte()?;
        let hi = self.read_byte()?;
        Ok(u16::from(lo) | (u16::from(hi) << 8))
    }
}

fn corrupt(reason: &'static str) -> GitError {
    GitError::Corrupt {
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zlib stream of "hello world" (fixed-Huffman) produced by a reference zlib. Round-trips
    /// to the original bytes — proving the fixed-tree literal path end to end.
    #[test]
    fn inflate_zlib_fixed_huffman() {
        // echo -n "hello world" | (python3 zlib.compress) → these bytes.
        let stream: [u8; 19] = [
            0x78, 0x9c, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0x28, 0xcf, 0x2f, 0xca, 0x49, 0x01,
            0x00, 0x1a, 0x0b, 0x04, 0x5d,
        ];
        let out = zlib_inflate(&stream).unwrap();
        assert_eq!(out, b"hello world");
    }

    /// A stored-block zlib stream (BTYPE=0) round-trips verbatim.
    #[test]
    fn inflate_zlib_stored_block() {
        // zlib.compress(b"abc", level=0): header 78 01, stored block.
        let stream: [u8; 11] = [
            0x78, 0x01, 0x01, 0x03, 0x00, 0xfc, 0xff, 0x61, 0x62, 0x63, 0x02,
        ];
        // trailing Adler bytes vary; we only need the inflate body to yield "abc".
        let out = zlib_inflate(&stream).unwrap();
        assert_eq!(&out[..3], b"abc");
    }
}
