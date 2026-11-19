//! Pure-Rust reader for Chromium's LevelDB Local Storage (`*.ldb` SSTables and `*.log` WAL).
//!
//! Chromium compacts recent Local Storage writes from `.log` WAL files into `.ldb` SSTables
//! whose data/index blocks are Snappy-compressed, and prefixes item keys/values with encoding
//! markers (`0x00` UTF-16LE or `0x01` Latin1). Parsing both `.ldb` (via Snappy decompression)
//! and `.log` records ensures active tokens are found reliably even after browser compaction
//! or account switching.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const LEVELDB_SSTABLE_MAGIC: [u8; 8] = [0x57, 0xfb, 0x80, 0x8b, 0x24, 0x75, 0x47, 0xdb];
const WAL_BLOCK_SIZE: usize = 32_768;
const WAL_HEADER_SIZE: usize = 7;
const MAX_SNAPPY_UNCOMPRESSED: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct LevelDbRecord {
    pub user_key: Vec<u8>,
    pub value: Vec<u8>,
    pub seq: u64,
    pub is_put: bool,
}

pub struct LevelDbFile {
    pub path: PathBuf,
    pub is_ldb: bool,
    pub modified: Option<SystemTime>,
    pub data: Vec<u8>,
}

fn read_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut result = 0u64;
    let mut shift = 0u32;
    while *pos < buf.len() {
        let b = buf[*pos];
        *pos += 1;
        result |= ((b & 0x7f) as u64).checked_shl(shift)?;
        if (b & 0x80) == 0 {
            return Some(result);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
    None
}

/// Decompresses a raw Snappy block payload (varint uncompressed length followed by opcodes).
pub fn decompress_snappy(src: &[u8]) -> Option<Vec<u8>> {
    let mut pos = 0usize;
    let ulen = read_varint(src, &mut pos)? as usize;
    if ulen > MAX_SNAPPY_UNCOMPRESSED {
        return None;
    }

    let mut out = Vec::with_capacity(ulen.min(1024 * 1024));
    while pos < src.len() {
        let tag = src[pos];
        pos += 1;
        match tag & 0x03 {
            0 => {
                let mut len = ((tag >> 2) as usize) + 1;
                if len > 60 {
                    let extra = len - 60;
                    if extra > 4 || pos.checked_add(extra)? > src.len() {
                        return None;
                    }
                    let mut raw_len = 0usize;
                    for i in 0..extra {
                        raw_len |= (src[pos + i] as usize) << (i * 8);
                    }
                    pos += extra;
                    len = raw_len.checked_add(1)?;
                }
                let end = pos.checked_add(len)?;
                if end > src.len() || out.len().checked_add(len)? > ulen {
                    return None;
                }
                out.extend_from_slice(&src[pos..end]);
                pos = end;
            }
            kind => {
                let (len, offset) = match kind {
                    1 => {
                        if pos >= src.len() {
                            return None;
                        }
                        let len = (((tag >> 2) & 0x07) as usize) + 4;
                        let off = (((tag & 0xe0) as usize) << 3) | (src[pos] as usize);
                        pos += 1;
                        (len, off)
                    }
                    2 => {
                        if pos.checked_add(2)? > src.len() {
                            return None;
                        }
                        let len = ((tag >> 2) as usize) + 1;
                        let off = u16::from_le_bytes([src[pos], src[pos + 1]]) as usize;
                        pos += 2;
                        (len, off)
                    }
                    _ => {
                        if pos.checked_add(4)? > src.len() {
                            return None;
                        }
                        let len = ((tag >> 2) as usize) + 1;
                        let off = u32::from_le_bytes([
                            src[pos],
                            src[pos + 1],
                            src[pos + 2],
                            src[pos + 3],
                        ]) as usize;
                        pos += 4;
                        (len, off)
                    }
                };
                if offset == 0 || offset > out.len() || out.len().checked_add(len)? > ulen {
                    return None;
                }
                for _ in 0..len {
                    let b = out[out.len() - offset];
                    out.push(b);
                }
            }
        }
    }

    if out.len() == ulen {
        Some(out)
    } else {
        None
    }
}

fn read_sstable_block<'a>(data: &[u8], off: usize, size: usize, scratch: &'a mut Vec<u8>) -> Option<&'a [u8]> {
    let end = off.checked_add(size)?;
    // Block trailer = 1 byte compression type + 4 byte masked CRC32C
    if end.checked_add(5)? > data.len() {
        return None;
    }
    let ctype = data[end];
    let payload = &data[off..end];
    match ctype {
        0 => {
            scratch.clear();
            scratch.extend_from_slice(payload);
            Some(scratch.as_slice())
        }
        1 => {
            *scratch = decompress_snappy(payload)?;
            Some(scratch.as_slice())
        }
        _ => None,
    }
}

fn block_entries_limit(block: &[u8]) -> Option<usize> {
    if block.len() < 4 {
        return None;
    }
    let n_restarts = u32::from_le_bytes(block[block.len() - 4..].try_into().ok()?) as usize;
    let footer_bytes = n_restarts.checked_mul(4)?.checked_add(4)?;
    block.len().checked_sub(footer_bytes)
}

fn iterate_block_entries<F>(block: &[u8], mut on_entry: F)
where
    F: FnMut(&[u8], &[u8]),
{
    let Some(limit) = block_entries_limit(block) else {
        return;
    };
    let mut pos = 0usize;
    let mut last_key = Vec::new();
    while pos < limit {
        let Some(shared) = read_varint(block, &mut pos).map(|v| v as usize) else {
            break;
        };
        let Some(non_shared) = read_varint(block, &mut pos).map(|v| v as usize) else {
            break;
        };
        let Some(val_len) = read_varint(block, &mut pos).map(|v| v as usize) else {
            break;
        };
        if shared > last_key.len() {
            break;
        }
        let Some(val_end) = pos.checked_add(non_shared).and_then(|p| p.checked_add(val_len)) else {
            break;
        };
        if val_end > limit {
            break;
        }
        last_key.truncate(shared);
        last_key.extend_from_slice(&block[pos..pos + non_shared]);
        let key_end = pos + non_shared;
        let val = &block[key_end..key_end + val_len];
        pos = val_end;
        on_entry(&last_key, val);
    }
}

/// Parses a Chromium LevelDB `.ldb` SSTable, returning decoded records and concatenated
/// uncompressed block payloads for downstream heuristic fallback matching.
pub fn parse_sstable(data: &[u8]) -> Option<(Vec<LevelDbRecord>, Vec<u8>)> {
    if data.len() < 48 || data[data.len() - 8..] != LEVELDB_SSTABLE_MAGIC {
        return None;
    }
    let footer = &data[data.len() - 48..];
    let mut fpos = 0usize;
    let _meta_off = read_varint(footer, &mut fpos)?;
    let _meta_sz = read_varint(footer, &mut fpos)?;
    let idx_off = read_varint(footer, &mut fpos)? as usize;
    let idx_sz = read_varint(footer, &mut fpos)? as usize;

    let mut idx_buf = Vec::new();
    let idx_block = read_sstable_block(data, idx_off, idx_sz, &mut idx_buf)?;

    let mut handles: Vec<(usize, usize)> = Vec::new();
    iterate_block_entries(idx_block, |_k, v| {
        let mut vp = 0usize;
        if let (Some(boff), Some(bsz)) = (read_varint(v, &mut vp), read_varint(v, &mut vp)) {
            handles.push((boff as usize, bsz as usize));
        }
    });

    let mut records = Vec::new();
    let mut blob = Vec::new();
    let mut blk_buf = Vec::new();
    for (boff, bsz) in handles {
        let Some(blk) = read_sstable_block(data, boff, bsz, &mut blk_buf) else {
            continue;
        };
        blob.extend_from_slice(blk);
        blob.push(b'\n');

        iterate_block_entries(blk, |internal_key, val| {
            if internal_key.len() >= 8 {
                let split = internal_key.len() - 8;
                let tag = u64::from_le_bytes(internal_key[split..].try_into().unwrap_or([0; 8]));
                let seq = tag >> 8;
                let vtype = tag & 0xff;
                if matches!(vtype, 0 | 1) {
                    records.push(LevelDbRecord {
                        user_key: internal_key[..split].to_vec(),
                        value: val.to_vec(),
                        seq,
                        is_put: vtype == 1,
                    });
                }
            }
        });
    }

    Some((records, blob))
}

/// Parses LevelDB `.log` Write-Ahead Log (WAL) physical records into logical Key-Value ops.
pub fn parse_wal_records(data: &[u8]) -> Vec<LevelDbRecord> {
    let mut batches: Vec<Vec<u8>> = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut pos = 0usize;

    while pos + WAL_HEADER_SIZE <= data.len() {
        let blk_rem = WAL_BLOCK_SIZE - (pos % WAL_BLOCK_SIZE);
        if blk_rem < WAL_HEADER_SIZE {
            pos += blk_rem;
            continue;
        }
        let rlen = u16::from_le_bytes([data[pos + 4], data[pos + 5]]) as usize;
        let rtype = data[pos + 6];
        if rlen == 0 && rtype == 0 {
            pos += blk_rem;
            continue;
        }
        if rlen > blk_rem - WAL_HEADER_SIZE || pos + WAL_HEADER_SIZE + rlen > data.len() {
            break;
        }
        let payload = &data[pos + WAL_HEADER_SIZE..pos + WAL_HEADER_SIZE + rlen];
        pos += WAL_HEADER_SIZE + rlen;
        match rtype {
            1 => {
                current.clear();
                batches.push(payload.to_vec());
            }
            2 => {
                current.clear();
                current.extend_from_slice(payload);
            }
            3 => {
                if !current.is_empty() {
                    current.extend_from_slice(payload);
                }
            }
            4 => {
                if !current.is_empty() {
                    current.extend_from_slice(payload);
                    batches.push(std::mem::take(&mut current));
                }
            }
            _ => {
                current.clear();
            }
        }
    }

    let mut out = Vec::new();
    for batch in batches {
        if batch.len() < 12 {
            continue;
        }
        let base_seq = u64::from_le_bytes(batch[..8].try_into().unwrap_or([0; 8]));
        let count = u32::from_le_bytes(batch[8..12].try_into().unwrap_or([0; 4]));
        let mut bp = 12usize;
        for op_idx in 0..count {
            if bp >= batch.len() {
                break;
            }
            let vtype = batch[bp];
            bp += 1;
            let Some(klen) = read_varint(&batch, &mut bp).map(|v| v as usize) else {
                break;
            };
            if bp.checked_add(klen).is_none_or(|e| e > batch.len()) {
                break;
            }
            let k = batch[bp..bp + klen].to_vec();
            bp += klen;

            let v = if vtype == 1 {
                let Some(vlen) = read_varint(&batch, &mut bp).map(|v| v as usize) else {
                    break;
                };
                if bp.checked_add(vlen).is_none_or(|e| e > batch.len()) {
                    break;
                }
                let val = batch[bp..bp + vlen].to_vec();
                bp += vlen;
                val
            } else if vtype == 0 {
                Vec::new()
            } else {
                break;
            };

            out.push(LevelDbRecord {
                user_key: k,
                value: v,
                seq: base_seq.saturating_add(op_idx as u64),
                is_put: vtype == 1,
            });
        }
    }
    out
}

/// Decodes Chromium Local Storage tagged strings (`0x00` UTF-16LE, `0x01` Latin1, or plain UTF-8).
pub fn decode_chrome_ls_string(raw: &[u8]) -> Option<String> {
    if raw.is_empty() {
        return Some(String::new());
    }
    match raw[0] {
        0x00 => {
            let body = &raw[1..];
            let units: Vec<u16> = body
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            Some(String::from_utf16_lossy(&units))
        }
        0x01 => Some(raw[1..].iter().map(|&b| b as char).collect()),
        _ => Some(String::from_utf8_lossy(raw).into_owned()),
    }
}

/// Parses Chromium Local Storage item key `b"_<origin>\\x00<encoding><key>"` -> `(origin, key)`.
pub fn parse_chrome_ls_key(user_key: &[u8]) -> Option<(String, String)> {
    if !user_key.starts_with(b"_") {
        return None;
    }
    let sep = user_key[1..].iter().position(|&b| b == 0)? + 1;
    let origin = String::from_utf8_lossy(&user_key[1..sep]).into_owned();
    let key_name = decode_chrome_ls_string(&user_key[sep + 1..])?;
    Some((origin, key_name))
}

/// Reads all `.log` / `.ldb` files under `<profile_dir>/Local Storage/leveldb` sorted newest first,
/// sharing read/write/delete locks so live Chromium browsers never trigger sharing violations.
pub fn read_leveldb_files<F>(profile_dir: &Path, mut shared_read: F) -> Vec<LevelDbFile>
where
    F: FnMut(&Path) -> Option<Vec<u8>>,
{
    let leveldb_dir = profile_dir.join("Local Storage").join("leveldb");
    if !leveldb_dir.exists() {
        return Vec::new();
    }
    let entries = match fs::read_dir(&leveldb_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut paths: Vec<(PathBuf, bool, Option<SystemTime>)> = Vec::new();
    for entry in entries.flatten() {
        let p = entry.path();
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext == "ldb" || ext == "log" {
            let is_ldb = ext == "ldb";
            let modified = p.metadata().ok().and_then(|m| m.modified().ok());
            paths.push((p, is_ldb, modified));
        }
    }
    paths.sort_by(|a, b| b.2.cmp(&a.2));

    let mut out = Vec::with_capacity(paths.len());
    for (path, is_ldb, modified) in paths {
        if let Some(data) = shared_read(&path) {
            if !data.is_empty() {
                out.push(LevelDbFile {
                    path,
                    is_ldb,
                    modified,
                    data,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_varint(mut v: u64, dst: &mut Vec<u8>) {
        while v >= 0x80 {
            dst.push((v as u8 & 0x7f) | 0x80);
            v >>= 7;
        }
        dst.push(v as u8);
    }

    fn compress_snappy_literals_and_copy_test_block(raw: &[u8]) -> Vec<u8> {
        // Compress `raw` by emitting literal first, or helper supporting >60 literals cleanly.
        let mut out = Vec::new();
        encode_varint(raw.len() as u64, &mut out);
        if raw.len() <= 60 {
            out.push((raw.len() as u8 - 1) << 2);
        } else {
            let n = (raw.len() - 1) as u16;
            out.push(61 << 2);
            out.extend_from_slice(&n.to_le_bytes());
        }
        out.extend_from_slice(raw);
        out
    }

    fn build_sstable_block(entries: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut blk = Vec::new();
        let mut prev: &[u8] = &[];
        for (k, v) in entries {
            let shared = prev.iter().zip(k.iter()).take_while(|(a, b)| a == b).count();
            encode_varint(shared as u64, &mut blk);
            encode_varint((k.len() - shared) as u64, &mut blk);
            encode_varint(v.len() as u64, &mut blk);
            blk.extend_from_slice(&k[shared..]);
            blk.extend_from_slice(v);
            prev = k;
        }
        blk.extend_from_slice(&0u32.to_le_bytes()); // 1 restart at 0
        blk.extend_from_slice(&1u32.to_le_bytes()); // n_restarts = 1
        blk
    }

    fn build_snappy_sstable(entries: &[(&[u8], &[u8])]) -> Vec<u8> {
        let data_blk = build_sstable_block(entries);
        let compressed = compress_snappy_literals_and_copy_test_block(&data_blk);

        let mut sstable = Vec::new();
        let data_off = 0u64;
        let data_sz = compressed.len() as u64;
        sstable.extend_from_slice(&compressed);
        sstable.push(1); // Snappy compression
        sstable.extend_from_slice(&[0u8; 4]); // CRC placeholder (not validated by reader)

        let mut data_handle = Vec::new();
        encode_varint(data_off, &mut data_handle);
        encode_varint(data_sz, &mut data_handle);
        let last_k = entries.last().map(|(k, _)| *k).unwrap_or(&[]);
        let idx_blk = build_sstable_block(&[(last_k, &data_handle)]);

        let idx_off = sstable.len() as u64;
        let idx_sz = idx_blk.len() as u64;
        sstable.extend_from_slice(&idx_blk);
        sstable.push(0); // Uncompressed index
        sstable.extend_from_slice(&[0u8; 4]);

        let mut footer = Vec::new();
        encode_varint(0, &mut footer);
        encode_varint(0, &mut footer);
        encode_varint(idx_off, &mut footer);
        encode_varint(idx_sz, &mut footer);
        while footer.len() < 40 {
            footer.push(0);
        }
        footer.extend_from_slice(&LEVELDB_SSTABLE_MAGIC);
        sstable.extend_from_slice(&footer);
        sstable
    }

    #[test]
    fn snappy_decompresses_literals_and_rle_backrefs() {
        // Encodes "abcd" literal (len 4) + 1-byte copy len=8 offset=4 => "abcdabcdabcd"
        let mut src = Vec::new();
        encode_varint(12, &mut src);
        src.push(3 << 2); // literal len 4
        src.extend_from_slice(b"abcd");
        src.push((((8 - 4) as u8) << 2) | 1); // copy-1 len=8
        src.push(4); // offset=4
        assert_eq!(decompress_snappy(&src).as_deref(), Some(&b"abcdabcdabcd"[..]));
    }

    #[test]
    fn parses_snappy_sstable_and_chrome_local_storage_records() {
        let mut k_new = b"_https://chat.deepseek.com\x00\x01userToken".to_vec();
        k_new.extend_from_slice(&((45245u64 << 8) | 1u64).to_le_bytes());

        let mut k_old = b"_https://chat.deepseek.com\x00\x01userToken".to_vec();
        k_old.extend_from_slice(&((45171u64 << 8) | 1u64).to_le_bytes());

        let v_new = b"\x01{\"value\":\"ACTIVE_6bQn_TOKEN\",\"__version\":\"0\"}";
        let v_old = b"\x01{\"value\":null,\"__version\":\"0\"}";

        let sstable = build_snappy_sstable(&[(&k_new, v_new), (&k_old, v_old)]);
        let (recs, blob) = parse_sstable(&sstable).expect("valid sstable");
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].seq, 45245);
        assert!(recs[0].is_put);
        let (origin, key) = parse_chrome_ls_key(&recs[0].user_key).expect("chrome ls key");
        assert_eq!(origin, "https://chat.deepseek.com");
        assert_eq!(key, "userToken");
        let val = decode_chrome_ls_string(&recs[0].value).unwrap();
        assert!(val.contains("ACTIVE_6bQn_TOKEN"));
        assert!(String::from_utf8_lossy(&blob).contains("ACTIVE_6bQn_TOKEN"));
    }

    #[test]
    fn parses_wal_write_batch_put_and_delete() {
        let mut batch = Vec::new();
        batch.extend_from_slice(&9001u64.to_le_bytes());
        batch.extend_from_slice(&2u32.to_le_bytes());
        // op 0: Put UTF-16LE key & UTF-16LE value
        let mut u16_key = b"_https://chat.deepseek.com\x00\x00".to_vec();
        for ch in "userToken".encode_utf16() {
            u16_key.extend_from_slice(&ch.to_le_bytes());
        }
        let mut u16_val = vec![0x00u8];
        for ch in "{\"value\":\"WAL_TOK\"}".encode_utf16() {
            u16_val.extend_from_slice(&ch.to_le_bytes());
        }
        batch.push(1);
        encode_varint(u16_key.len() as u64, &mut batch);
        batch.extend_from_slice(&u16_key);
        encode_varint(u16_val.len() as u64, &mut batch);
        batch.extend_from_slice(&u16_val);
        // op 1: Delete
        batch.push(0);
        encode_varint(3, &mut batch);
        batch.extend_from_slice(b"old");

        let mut wal = Vec::new();
        wal.extend_from_slice(&0u32.to_le_bytes()); // crc
        wal.extend_from_slice(&(batch.len() as u16).to_le_bytes());
        wal.push(1); // Full record
        wal.extend_from_slice(&batch);

        let recs = parse_wal_records(&wal);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].seq, 9001);
        assert!(recs[0].is_put);
        let (orig, key) = parse_chrome_ls_key(&recs[0].user_key).unwrap();
        assert_eq!(orig, "https://chat.deepseek.com");
        assert_eq!(key, "userToken");
        assert_eq!(decode_chrome_ls_string(&recs[0].value).as_deref(), Some("{\"value\":\"WAL_TOK\"}"));
        assert_eq!(recs[1].seq, 9002);
        assert!(!recs[1].is_put);
    }
}
