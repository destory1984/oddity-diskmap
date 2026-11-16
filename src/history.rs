//! Scan history: after each scan a list of folder sizes is saved, so a later scan
//! can show which folders grew.
//!
//! Layout: %LOCALAPPDATA%\oddity-diskmap\history\<16 hex of root path>\<unix secs>.tsv
//! File format (tab separated, UTF-8):
//!   oddity-diskmap history 1
//!   root    C:\
//!   time    1763280000
//!   method  Mft
//!   total   <alloc bytes>  <size bytes>
//!   <alloc bytes>  <size bytes>  <path relative to root, "" for the root itself>
//!   ...

use crate::tree::{Method, Tree};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const MAGIC: &str = "oddity-diskmap history 1";

/// How much to record. Folders deeper than `depth` or smaller than `min_bytes`
/// (allocated) are left out; they still count in their parent's total.
#[derive(Clone, Copy, Debug)]
pub struct Detail {
    pub depth: u32,
    pub min_bytes: u64,
}

pub fn base_dir() -> Option<PathBuf> {
    let d = match std::env::var_os("DISKMAP_HISTORY_DIR") {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("oddity-diskmap").join("history"),
    };
    Some(d)
}

fn fnv(data: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn dir_for(root: &str) -> Option<PathBuf> {
    Some(base_dir()?.join(format!("{:016x}", fnv(root.to_lowercase().as_bytes()))))
}

/// Folder sizes of a scanned tree, root first, as (relative path, alloc, size).
pub fn collect(tree: &Tree, detail: Detail) -> Vec<(String, u64, u64)> {
    let mut out = Vec::new();
    let mut stack: Vec<(u32, String, u32)> = vec![(tree.root, String::new(), 0)];
    while let Some((i, rel, depth)) = stack.pop() {
        let n = &tree.nodes[i as usize];
        out.push((rel.clone(), n.alloc, n.size));
        if depth >= detail.depth {
            continue;
        }
        for &c in &n.children {
            let cn = &tree.nodes[c as usize];
            if cn.is_dir && cn.alloc >= detail.min_bytes {
                let p = if rel.is_empty() { cn.name.to_string() } else { format!("{rel}\\{}", cn.name) };
                stack.push((c, p, depth + 1));
            }
        }
    }
    out
}

/// Save a snapshot and drop the oldest ones beyond `keep`. Returns the new file.
pub fn save(tree: &Tree, when: i64, entries: &[(String, u64, u64)], keep: usize) -> std::io::Result<PathBuf> {
    use std::fmt::Write;
    let dir = dir_for(&tree.root_path).ok_or_else(|| std::io::Error::other("LOCALAPPDATA 없음"))?;
    std::fs::create_dir_all(&dir)?;
    let root = &tree.nodes[tree.root as usize];
    let mut s = String::with_capacity(entries.len() * 64 + 128);
    let method = match tree.method {
        Method::Mft => "Mft",
        Method::Walk => "Walk",
    };
    let _ = writeln!(s, "{MAGIC}\nroot\t{}\ntime\t{when}\nmethod\t{method}\ntotal\t{}\t{}", tree.root_path, root.alloc, root.size);
    for (rel, alloc, size) in entries {
        let _ = writeln!(s, "{alloc}\t{size}\t{rel}");
    }
    let path = dir.join(format!("{when}.tsv"));
    let tmp = path.with_extension("tsv.tmp");
    std::fs::write(&tmp, s)?;
    std::fs::rename(&tmp, &path)?;
    prune(&dir, keep);
    Ok(path)
}

fn prune(dir: &Path, keep: usize) {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "tsv"))
        .collect();
    files.sort(); // names are unix seconds of equal length: oldest first
    let extra = files.len().saturating_sub(keep.max(1));
    for f in &files[..extra] {
        let _ = std::fs::remove_file(f);
    }
}

/// One saved snapshot (header only).
#[derive(Clone, Debug)]
pub struct Meta {
    pub path: PathBuf,
    pub when: i64,
    pub method: String,
    pub total_alloc: u64,
    pub bytes: u64,
}

/// Snapshots of `root`, newest first.
pub fn list(root: &str) -> Vec<Meta> {
    let Some(dir) = dir_for(root) else { return Vec::new() };
    let mut v: Vec<Meta> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "tsv"))
        .filter_map(|e| read_meta(&e.path()))
        .collect();
    v.sort_by(|a, b| b.when.cmp(&a.when));
    v
}

fn read_meta(path: &Path) -> Option<Meta> {
    use std::io::{BufRead, BufReader};
    let f = std::fs::File::open(path).ok()?;
    let bytes = f.metadata().ok()?.len();
    let mut lines = BufReader::new(f).lines();
    if lines.next()?.ok()? != MAGIC {
        return None;
    }
    let (mut when, mut method, mut total) = (0, String::new(), 0);
    for line in lines.take(4).map_while(Result::ok) {
        let mut it = line.split('\t');
        match it.next() {
            Some("time") => when = it.next()?.parse().ok()?,
            Some("method") => method = it.next().unwrap_or("").to_string(),
            Some("total") => total = it.next()?.parse().ok()?,
            _ => {}
        }
    }
    Some(Meta { path: path.to_path_buf(), when, method, total_alloc: total, bytes })
}

/// Folder sizes from a snapshot: lowercase relative path -> (display path, alloc).
pub fn load(path: &Path) -> Option<HashMap<String, (String, u64)>> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    if lines.next()? != MAGIC {
        return None;
    }
    let mut map = HashMap::new();
    for line in lines {
        let mut it = line.splitn(3, '\t');
        let (Some(a), Some(_s), Some(rel)) = (it.next(), it.next(), it.next()) else { continue };
        let Ok(alloc) = a.parse::<u64>() else { continue }; // header lines
        map.insert(rel.to_lowercase(), (rel.to_string(), alloc));
    }
    Some(map)
}

/// One folder in the comparison.
#[derive(Clone, Debug)]
pub struct Change {
    pub rel: String,
    pub old: Option<u64>,
    pub new: Option<u64>,
    pub delta: i64,
}

/// Compare a saved snapshot with the current folder sizes (both alloc).
/// `causes_only` hides a folder when one child explains 80% or more of its change,
/// so the list points at the folder that actually grew.
pub fn compare(
    old: &HashMap<String, (String, u64)>,
    current: &[(String, u64, u64)],
    min_delta: u64,
    causes_only: bool,
) -> Vec<Change> {
    let mut all: HashMap<String, Change> = HashMap::new();
    for (rel, alloc, _) in current {
        let key = rel.to_lowercase();
        let o = old.get(&key).map(|v| v.1);
        all.insert(key, Change { rel: rel.clone(), old: o, new: Some(*alloc), delta: *alloc as i64 - o.unwrap_or(0) as i64 });
    }
    for (key, (rel, alloc)) in old {
        all.entry(key.clone()).or_insert(Change { rel: rel.clone(), old: Some(*alloc), new: None, delta: -(*alloc as i64) });
    }
    // a folder missing from one side was below the size limit there, or did not exist:
    // its change is only known if the parent is on both sides; keep it but it may overstate
    let mut v: Vec<Change> = all.values().filter(|c| c.delta.unsigned_abs() >= min_delta).cloned().collect();
    if causes_only {
        let by_key: HashMap<String, i64> = all.iter().map(|(k, c)| (k.clone(), c.delta)).collect();
        let mut child_best: HashMap<String, i64> = HashMap::new();
        for (k, d) in &by_key {
            if k.is_empty() {
                continue;
            }
            let parent = k.rsplit_once('\\').map(|(p, _)| p.to_string()).unwrap_or_default();
            let e = child_best.entry(parent).or_insert(0);
            if d.abs() > e.abs() {
                *e = *d;
            }
        }
        v.retain(|c| {
            let best = child_best.get(&c.rel.to_lowercase()).copied().unwrap_or(0);
            !(best.signum() == c.delta.signum() && best.abs() * 10 >= c.delta.abs() * 8)
        });
    }
    v.sort_by(|a, b| b.delta.cmp(&a.delta).then_with(|| a.rel.cmp(&b.rel)));
    v
}

/// Bytes used by all saved history.
pub fn usage() -> u64 {
    let Some(d) = base_dir() else { return 0 };
    std::fs::read_dir(d)
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|r| std::fs::read_dir(r.path()).into_iter().flatten().flatten())
        .filter_map(|f| f.metadata().ok())
        .map(|m| m.len())
        .sum()
}

pub fn clear() -> std::io::Result<()> {
    if let Some(d) = base_dir() {
        if d.exists() {
            std::fs::remove_dir_all(d)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn old_map(rows: &[(&str, u64)]) -> HashMap<String, (String, u64)> {
        rows.iter().map(|(r, a)| (r.to_lowercase(), (r.to_string(), *a))).collect()
    }

    #[test]
    fn compare_finds_cause() {
        let old = old_map(&[("", 100), ("Users", 60), ("Users\\me", 50), ("Windows", 40)]);
        let cur = vec![
            ("".to_string(), 130, 0),
            ("Users".to_string(), 90, 0),
            ("Users\\me".to_string(), 80, 0),
            ("Windows".to_string(), 40, 0),
        ];
        let all = compare(&old, &cur, 1, false);
        assert_eq!(all.iter().map(|c| c.rel.as_str()).collect::<Vec<_>>(), vec!["", "Users", "Users\\me"]);
        let causes = compare(&old, &cur, 1, true);
        assert_eq!(causes.len(), 1);
        assert_eq!(causes[0].rel, "Users\\me");
        assert_eq!(causes[0].delta, 30);
    }

    #[test]
    fn new_and_gone_folders() {
        let old = old_map(&[("", 100), ("Old", 20)]);
        let cur = vec![("".to_string(), 100, 0), ("New".to_string(), 20, 0)];
        let v = compare(&old, &cur, 1, false);
        let new = v.iter().find(|c| c.rel == "New").unwrap();
        assert_eq!((new.old, new.delta), (None, 20));
        let gone = v.iter().find(|c| c.rel == "Old").unwrap();
        assert_eq!((gone.new, gone.delta), (None, -20));
    }
}
