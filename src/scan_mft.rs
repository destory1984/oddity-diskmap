//! Fast scanner: reads the NTFS Master File Table ($MFT) straight from the volume.
//!
//! Instead of opening every folder, it reads one big table that holds a record for every
//! file and folder on the volume, then links records to their parents. Needs administrator
//! rights (opening `\\.\C:` for raw reads) and an NTFS volume.

use crate::tree::{Method, Node, Progress, Tree};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::sync_channel;
use std::time::Instant;

const ROOT_RECORD: usize = 5;
const CHUNK: u64 = 8 << 20;

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

pub struct Geometry {
    pub bytes_per_sector: u64,
    pub cluster: u64,
    pub mft_lcn: u64,
    pub record_size: usize,
}

pub fn parse_boot(b: &[u8]) -> Result<Geometry, String> {
    if b.len() < 512 || &b[3..11] != b"NTFS    " {
        return Err("NTFS 볼륨이 아닙니다".into());
    }
    let bps = u16_at(b, 0x0B) as u64;
    let spc_raw = b[0x0D];
    let spc = if spc_raw > 0x80 { 1u64 << (256 - spc_raw as u32) } else { spc_raw as u64 };
    let cluster = bps * spc;
    let mft_lcn = u64_at(b, 0x30);
    let cpr = b[0x40] as i8;
    let record_size = if cpr > 0 { cpr as u64 * cluster } else { 1u64 << (-(cpr as i32)) } as usize;
    if bps == 0 || cluster == 0 || !(256..=65536).contains(&record_size) {
        return Err("부트 섹터 값이 이상합니다".into());
    }
    Ok(Geometry { bytes_per_sector: bps, cluster, mft_lcn, record_size })
}

/// Undo the NTFS "update sequence" protection: the last two bytes of every 512-byte stride
/// were replaced with a check value on disk; the real bytes live in an array in the header.
pub fn apply_fixup(rec: &mut [u8]) -> bool {
    let usa_ofs = u16_at(rec, 4) as usize;
    let usa_cnt = u16_at(rec, 6) as usize;
    if usa_cnt < 2 || usa_ofs + usa_cnt * 2 > rec.len() {
        return false;
    }
    let stride = rec.len() / (usa_cnt - 1);
    let check = [rec[usa_ofs], rec[usa_ofs + 1]];
    for k in 1..usa_cnt {
        let pos = k * stride - 2;
        if rec[pos..pos + 2] != check {
            return false;
        }
        rec[pos] = rec[usa_ofs + 2 * k];
        rec[pos + 1] = rec[usa_ofs + 2 * k + 1];
    }
    true
}

/// Decode a non-resident attribute's run list into (lcn, cluster count) pairs.
/// `lcn == None` marks a sparse run.
pub fn parse_runs(b: &[u8]) -> Vec<(Option<u64>, u64)> {
    let mut runs = Vec::new();
    let mut i = 0;
    let mut lcn: i64 = 0;
    while i < b.len() && b[i] != 0 {
        let h = b[i];
        let (ln, on) = ((h & 0x0F) as usize, (h >> 4) as usize);
        i += 1;
        if ln == 0 || ln > 8 || on > 8 || i + ln + on > b.len() {
            break;
        }
        let mut len: u64 = 0;
        for k in 0..ln {
            len |= (b[i + k] as u64) << (8 * k);
        }
        i += ln;
        if on == 0 {
            runs.push((None, len));
        } else {
            let mut off: i64 = 0;
            for k in 0..on {
                off |= (b[i + k] as i64) << (8 * k);
            }
            // sign-extend
            let shift = 64 - 8 * on as u32;
            off = (off << shift) >> shift;
            lcn += off;
            runs.push((Some(lcn as u64), len));
        }
        i += on;
    }
    runs
}

/// Per-record facts gathered while parsing.
struct Recs {
    nodes: Vec<Node>,
    in_use: Vec<bool>,
    has_name: Vec<bool>,
}

impl Recs {
    fn ensure(&mut self, n: usize) {
        if n >= self.nodes.len() {
            let new_len = (n + 1).max(self.nodes.len() * 5 / 4);
            self.nodes.resize_with(new_len, || Node::new("".into(), false));
            self.in_use.resize(new_len, false);
            self.has_name.resize(new_len, false);
        }
    }

    fn parse(&mut self, rec: &mut [u8], recno: usize, p: &Progress) {
        if rec.len() < 0x30 || &rec[0..4] != b"FILE" || !apply_fixup(rec) {
            return;
        }
        let flags = u16_at(rec, 0x16);
        if flags & 1 == 0 {
            return;
        }
        let base = (u64_at(rec, 0x20) & 0xFFFF_FFFF_FFFF) as usize;
        let owner = if base != 0 { base } else { recno };
        self.ensure(owner);
        if base == 0 {
            self.in_use[owner] = true;
            let is_dir = flags & 2 != 0;
            self.nodes[owner].is_dir = is_dir;
            if is_dir {
                p.dirs.fetch_add(1, Relaxed);
            } else {
                p.files.fetch_add(1, Relaxed);
            }
        }
        let mut off = u16_at(rec, 0x14) as usize;
        while off + 16 <= rec.len() {
            let t = u32_at(rec, off);
            if t == 0xFFFF_FFFF {
                break;
            }
            let alen = u32_at(rec, off + 4) as usize;
            if alen < 16 || off + alen > rec.len() {
                break;
            }
            let a = &rec[off..off + alen];
            let nonres = a[8] != 0;
            let name_len = a[9];
            match t {
                // $STANDARD_INFORMATION: last write time
                0x10 if !nonres && alen >= 0x18 => {
                    let c = u16_at(a, 0x14) as usize;
                    if c + 16 <= alen {
                        self.nodes[owner].mtime = u64_at(a, c + 8);
                    }
                }
                // $FILE_NAME: name and parent folder
                0x30 if !nonres && alen >= 0x18 => {
                    let c = u16_at(a, 0x14) as usize;
                    if c + 0x42 <= alen && !self.has_name[owner] {
                        let ns = a[c + 0x41];
                        let n = a[c + 0x40] as usize;
                        // namespace 2 = DOS 8.3 short name only; skip it
                        if ns != 2 && c + 0x42 + 2 * n <= alen {
                            let units: Vec<u16> =
                                (0..n).map(|k| u16_at(a, c + 0x42 + 2 * k)).collect();
                            let node = &mut self.nodes[owner];
                            node.name = String::from_utf16_lossy(&units).into();
                            node.parent = (u64_at(a, c) & 0xFFFF_FFFF_FFFF) as u32;
                            self.has_name[owner] = true;
                        }
                    }
                }
                // unnamed $DATA: file contents
                0x80 if name_len == 0 => {
                    let node = &mut self.nodes[owner];
                    if !nonres {
                        node.size = u32_at(a, 0x10) as u64;
                    } else if alen >= 0x40 && u64_at(a, 0x10) == 0 {
                        node.alloc = u64_at(a, 0x28);
                        node.size = u64_at(a, 0x30);
                        let aflags = u16_at(a, 0x0C);
                        let runs_ofs = u16_at(a, 0x20) as usize;
                        // compressed or sparse: real on-disk usage is in "total allocated"
                        if aflags & 0x8001 != 0 && runs_ofs >= 0x48 && alen >= 0x48 {
                            node.alloc = u64_at(a, 0x40);
                        }
                    }
                }
                _ => {}
            }
            off += alen;
        }
    }
}

fn volume_of(root: &str) -> Result<char, String> {
    let c = root.chars().next().ok_or("경로가 비었습니다")?;
    if !c.is_ascii_alphabetic() || root.chars().nth(1) != Some(':') {
        return Err("MFT 방식은 드라이브 문자로 시작하는 경로만 됩니다".into());
    }
    Ok(c.to_ascii_uppercase())
}

pub fn scan(root: &str, p: &Progress) -> Result<Tree, String> {
    let start = Instant::now();
    let letter = volume_of(root)?;
    let dev = format!(r"\\.\{letter}:");
    let mut f = File::open(&dev).map_err(|e| format!("볼륨을 열 수 없습니다 ({e}). 관리자 권한이 필요합니다"))?;

    let mut boot = vec![0u8; 4096];
    f.read_exact(&mut boot).map_err(|e| format!("부트 섹터 읽기 실패: {e}"))?;
    let g = parse_boot(&boot)?;
    let rs = g.record_size;

    // Record 0 is $MFT itself; its $DATA run list says where the rest of the table lives.
    let first_len = (rs as u64).div_ceil(g.bytes_per_sector) * g.bytes_per_sector;
    let mut rec0 = vec![0u8; first_len as usize];
    f.seek(SeekFrom::Start(g.mft_lcn * g.cluster)).map_err(|e| e.to_string())?;
    f.read_exact(&mut rec0).map_err(|e| format!("$MFT 읽기 실패: {e}"))?;
    let rec0 = &mut rec0[..rs];
    if &rec0[0..4] != b"FILE" || !apply_fixup(rec0) {
        return Err("$MFT 레코드가 깨졌습니다".into());
    }
    let (mut runs, mut mft_size) = (Vec::new(), 0u64);
    let mut off = u16_at(rec0, 0x14) as usize;
    while off + 16 <= rs {
        let t = u32_at(rec0, off);
        let alen = u32_at(rec0, off + 4) as usize;
        if t == 0xFFFF_FFFF || alen < 16 || off + alen > rs {
            break;
        }
        if t == 0x80 && rec0[off + 8] != 0 && rec0[off + 9] == 0 {
            let a = &rec0[off..off + alen];
            mft_size = u64_at(a, 0x30);
            runs = parse_runs(&a[u16_at(a, 0x20) as usize..]);
            break;
        }
        off += alen;
    }
    if runs.is_empty() || mft_size == 0 {
        return Err("$MFT 위치를 찾지 못했습니다".into());
    }
    p.bytes_total.store(mft_size, Relaxed);

    let total_records = (mft_size / rs as u64) as usize;
    let mut recs = Recs {
        nodes: Vec::with_capacity(total_records),
        in_use: Vec::with_capacity(total_records),
        has_name: Vec::with_capacity(total_records),
    };
    recs.ensure(total_records.max(ROOT_RECORD + 1) - 1);

    let cluster = g.cluster;
    let read_err = std::thread::scope(|s| -> Result<(), String> {
        let (tx, rx) = sync_channel::<Vec<u8>>(4);
        let reader = s.spawn(move || -> Result<(), String> {
            let mut left = mft_size;
            for &(lcn, count) in &runs {
                let mut run_off = 0u64;
                let run_len = count * cluster;
                while run_off < run_len && left > 0 {
                    if p.cancelled() {
                        return Ok(());
                    }
                    let n = CHUNK.min(run_len - run_off);
                    let mut buf = vec![0u8; n as usize];
                    if let Some(lcn) = lcn {
                        f.seek(SeekFrom::Start(lcn * cluster + run_off)).map_err(|e| e.to_string())?;
                        f.read_exact(&mut buf).map_err(|e| format!("MFT 읽기 실패: {e}"))?;
                    }
                    if n > left {
                        buf.truncate(left as usize);
                    }
                    left -= buf.len() as u64;
                    run_off += n;
                    if tx.send(buf).is_err() {
                        return Ok(());
                    }
                }
            }
            Ok(())
        });

        let mut pending: Vec<u8> = Vec::new();
        let mut recno = 0usize;
        for chunk in rx {
            p.bytes_done.fetch_add(chunk.len() as u64, Relaxed);
            let mut data = if pending.is_empty() {
                chunk
            } else {
                let mut v = std::mem::take(&mut pending);
                v.extend_from_slice(&chunk);
                v
            };
            let whole = data.len() / rs * rs;
            for r in data[..whole].chunks_exact_mut(rs) {
                recs.parse(r, recno, p);
                recno += 1;
            }
            pending = data.split_off(whole);
        }
        reader.join().map_err(|_| "읽기 스레드가 죽었습니다".to_string())?
    });
    read_err?;
    if p.cancelled() {
        return Err("중지됨".into());
    }

    // Link every named, in-use record to its parent folder.
    let Recs { mut nodes, in_use, has_name } = recs;
    let n = nodes.len();
    for r in 0..n {
        if r == ROOT_RECORD || !in_use[r] || !has_name[r] {
            continue;
        }
        let parent = nodes[r].parent as usize;
        if parent < n && in_use[parent] && nodes[parent].is_dir && parent != r {
            nodes[parent].children.push(r as u32);
        }
    }
    nodes[ROOT_RECORD].name = "".into();
    nodes[ROOT_RECORD].parent = crate::tree::NO_PARENT;
    nodes[ROOT_RECORD].is_dir = true;

    let mut t = Tree {
        nodes,
        root: ROOT_RECORD as u32,
        root_path: format!("{letter}:\\"),
        errors: 0,
        method: Method::Mft,
        elapsed: start.elapsed(),
        cluster_size: g.cluster,
    };
    // A folder below the drive root: scan the whole volume, then start the view there.
    let rel = root[2..].trim_matches('\\');
    if !rel.is_empty() {
        let sub = t.find_path(rel).ok_or_else(|| format!("{root} 을(를) MFT에서 찾지 못했습니다"))?;
        t.root = sub;
        t.root_path = root.trim_end_matches('\\').to_string();
    }
    t.aggregate();
    t.elapsed = start.elapsed();
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs() {
        // len 0x18 clusters at lcn 0x5634; then len 0x10 at +0x100 (relative); then sparse 4
        let b = [0x21, 0x18, 0x34, 0x56, 0x21, 0x10, 0x00, 0x01, 0x01, 0x04, 0x00];
        assert_eq!(
            parse_runs(&b),
            vec![(Some(0x5634), 0x18), (Some(0x5734), 0x10), (None, 4)]
        );
        // negative offset
        let b = [0x11, 0x02, 0xFE, 0x00];
        assert_eq!(parse_runs(&b), vec![(Some(u64::MAX - 1), 2)]);
    }

    #[test]
    fn fixup() {
        let mut r = vec![0u8; 1024];
        r[0..4].copy_from_slice(b"FILE");
        r[4] = 0x30; // usa offset
        r[6] = 3; // usa count: check + 2 sectors
        r[0x30] = 0xAB;
        r[0x31] = 0xCD;
        r[0x32] = 1;
        r[0x33] = 2;
        r[0x34] = 3;
        r[0x35] = 4;
        r[510] = 0xAB;
        r[511] = 0xCD;
        r[1022] = 0xAB;
        r[1023] = 0xCD;
        assert!(apply_fixup(&mut r));
        assert_eq!(&r[510..512], &[1, 2]);
        assert_eq!(&r[1022..1024], &[3, 4]);
        r[510] = 0;
        r[0x30] = 0x11;
        assert!(!apply_fixup(&mut r));
    }
}
