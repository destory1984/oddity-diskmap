//! Shared tree model produced by both scanners.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

pub const NO_PARENT: u32 = u32::MAX;

pub struct Node {
    pub name: Box<str>,
    pub parent: u32,
    pub children: Vec<u32>,
    /// Logical size (sum of file lengths).
    pub size: u64,
    /// Space allocated on disk.
    pub alloc: u64,
    /// Number of files below (1 for a file itself).
    pub files: u64,
    /// Number of folders below (not counting itself).
    pub dirs: u64,
    /// FILETIME (100 ns since 1601-01-01). For folders: newest in the subtree.
    pub mtime: u64,
    pub is_dir: bool,
}

impl Node {
    pub fn new(name: Box<str>, is_dir: bool) -> Self {
        Node {
            name,
            parent: NO_PARENT,
            children: Vec::new(),
            size: 0,
            alloc: 0,
            files: 0,
            dirs: 0,
            mtime: 0,
            is_dir,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    Mft,
    Walk,
}

pub struct Tree {
    pub nodes: Vec<Node>,
    pub root: u32,
    /// Full path of the root node, e.g. `C:\` or `C:\Users`.
    pub root_path: String,
    pub errors: u64,
    pub method: Method,
    pub elapsed: Duration,
    pub cluster_size: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Name,
    Size,
    Alloc,
    Files,
    Dirs,
    Mtime,
}

impl Tree {
    /// Compute folder totals bottom-up from the root.
    pub fn aggregate(&mut self) {
        // Iterative post-order: collect order first, then process in reverse.
        let mut order = Vec::with_capacity(self.nodes.len());
        // Files stamped in the future (bad clocks, odd installers) must not become a folder's date.
        let now_ft = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            + 11_644_473_600
            + 86_400)
            * 10_000_000;
        let mut stack = vec![self.root];
        while let Some(i) = stack.pop() {
            order.push(i);
            stack.extend_from_slice(&self.nodes[i as usize].children);
        }
        for &i in order.iter().rev() {
            let i = i as usize;
            if !self.nodes[i].is_dir {
                self.nodes[i].files = 1;
                self.nodes[i].dirs = 0;
                continue;
            }
            let (mut size, mut alloc, mut files, mut dirs, mut mtime) =
                (0u64, 0u64, 0u64, 0u64, self.nodes[i].mtime);
            for &c in &self.nodes[i].children {
                let c = &self.nodes[c as usize];
                size += c.size;
                alloc += c.alloc;
                files += c.files;
                dirs += c.dirs + c.is_dir as u64;
                if c.mtime <= now_ft {
                    mtime = mtime.max(c.mtime);
                }
            }
            let n = &mut self.nodes[i];
            n.size = size;
            n.alloc = alloc;
            n.files = files;
            n.dirs = dirs;
            n.mtime = mtime;
        }
    }

    pub fn sort(&mut self, key: SortKey, descending: bool) {
        let mut all: Vec<Vec<u32>> = self
            .nodes
            .iter_mut()
            .map(|n| std::mem::take(&mut n.children))
            .collect();
        let nodes = &self.nodes;
        use rayon::prelude::*;
        all.par_iter_mut().for_each(|ch| {
            if ch.len() < 2 {
                return;
            }
            ch.sort_by(|&a, &b| {
                let (a, b) = (&nodes[a as usize], &nodes[b as usize]);
                let o = match key {
                    SortKey::Name => b
                        .is_dir
                        .cmp(&a.is_dir)
                        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
                    SortKey::Size => a.size.cmp(&b.size),
                    SortKey::Alloc => a.alloc.cmp(&b.alloc),
                    SortKey::Files => a.files.cmp(&b.files),
                    SortKey::Dirs => a.dirs.cmp(&b.dirs),
                    SortKey::Mtime => a.mtime.cmp(&b.mtime),
                };
                if descending { o.reverse() } else { o }
            });
        });
        for (n, ch) in self.nodes.iter_mut().zip(all) {
            n.children = ch;
        }
    }

    pub fn path_of(&self, mut i: u32) -> String {
        let mut parts = Vec::new();
        while i != self.root && i != NO_PARENT {
            parts.push(&*self.nodes[i as usize].name);
            i = self.nodes[i as usize].parent;
        }
        let mut p = self.root_path.clone();
        for part in parts.iter().rev() {
            if !p.ends_with('\\') {
                p.push('\\');
            }
            p.push_str(part);
        }
        p
    }

    /// Ancestors of `i` from the root down to `i` (inclusive).
    pub fn chain(&self, mut i: u32) -> Vec<u32> {
        let mut v = vec![i];
        while i != self.root && self.nodes[i as usize].parent != NO_PARENT {
            i = self.nodes[i as usize].parent;
            v.push(i);
        }
        v.reverse();
        v
    }

    /// Find the node for a path below the root (case-insensitive).
    pub fn find_path(&self, rel: &str) -> Option<u32> {
        let mut cur = self.root;
        for part in rel.split('\\').filter(|s| !s.is_empty()) {
            let lower = part.to_lowercase();
            cur = *self.nodes[cur as usize]
                .children
                .iter()
                .find(|&&c| self.nodes[c as usize].name.to_lowercase() == lower)?;
        }
        Some(cur)
    }

    pub fn value(&self, i: u32, use_alloc: bool) -> u64 {
        let n = &self.nodes[i as usize];
        if use_alloc { n.alloc } else { n.size }
    }
}

/// Live counters shared between a scan thread and the UI.
#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub errors: AtomicU64,
    pub bytes_done: AtomicU64,
    pub bytes_total: AtomicU64,
    pub cancel: AtomicBool,
}

impl Progress {
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

pub fn fmt_size(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if b < 1024 {
        return format!("{b} B");
    }
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < U.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1} {}", U[u])
}

pub fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn fmt_filetime(ft: u64) -> String {
    if ft == 0 {
        return String::new();
    }
    let secs = (ft / 10_000_000) as i64 - 11_644_473_600;
    match chrono::DateTime::from_timestamp(secs, 0) {
        Some(t) => t
            .with_timezone(&chrono::Local)
            .format("%y-%m-%d")
            .to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(fmt_size(500), "500 B");
        assert_eq!(fmt_size(1536), "1.5 KB");
        assert_eq!(fmt_count(1570300), "1,570,300");
        assert_eq!(fmt_count(12), "12");
    }

    #[test]
    fn aggregate_and_path() {
        let mut t = Tree {
            nodes: vec![
                Node::new("".into(), true),
                Node::new("a".into(), true),
                Node::new("f1".into(), false),
                Node::new("f2".into(), false),
            ],
            root: 0,
            root_path: "C:\\".into(),
            errors: 0,
            method: Method::Walk,
            elapsed: Duration::ZERO,
            cluster_size: 4096,
        };
        t.nodes[2].size = 10;
        t.nodes[3].size = 30;
        t.nodes[0].children = vec![1, 3];
        t.nodes[1].children = vec![2];
        t.nodes[1].parent = 0;
        t.nodes[3].parent = 0;
        t.nodes[2].parent = 1;
        t.aggregate();
        assert_eq!(t.nodes[0].size, 40);
        assert_eq!(t.nodes[0].files, 2);
        assert_eq!(t.nodes[0].dirs, 1);
        t.sort(SortKey::Size, true);
        assert_eq!(t.nodes[0].children, vec![3, 1]);
        assert_eq!(t.path_of(2), "C:\\a\\f1");
        assert_eq!(t.find_path("A\\F1"), Some(2));
    }
}
