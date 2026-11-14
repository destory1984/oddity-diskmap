//! Ordinary scanner: lists every folder with the Win32 directory API (works on any file system).

use crate::tree::{Method, Node, Progress, Tree, NO_PARENT};
use rayon::prelude::*;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

struct TmpFile {
    name: Box<str>,
    size: u64,
    mtime: u64,
}

struct TmpDir {
    name: Box<str>,
    mtime: u64,
    files: Vec<TmpFile>,
    dirs: Vec<TmpDir>,
}

fn scan_dir(path: &Path, name: Box<str>, mtime: u64, p: &Progress) -> TmpDir {
    let mut out = TmpDir { name, mtime, files: Vec::new(), dirs: Vec::new() };
    if p.cancelled() {
        return out;
    }
    let rd = match std::fs::read_dir(path) {
        Ok(rd) => rd,
        Err(_) => {
            p.errors.fetch_add(1, Relaxed);
            return out;
        }
    };
    let mut subdirs: Vec<(PathBuf, Box<str>, u64)> = Vec::new();
    for e in rd {
        let Ok(e) = e else {
            p.errors.fetch_add(1, Relaxed);
            continue;
        };
        // On Windows this metadata comes from the directory listing itself: no extra system call.
        let Ok(md) = e.metadata() else {
            p.errors.fetch_add(1, Relaxed);
            continue;
        };
        let name: Box<str> = e.file_name().to_string_lossy().into();
        let attrs = md.file_attributes();
        if md.is_dir() {
            // Junctions and symlinked folders point elsewhere; counting them would double-count.
            if attrs & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                subdirs.push((e.path(), name, md.last_write_time()));
            }
        } else {
            out.files.push(TmpFile { name, size: md.file_size(), mtime: md.last_write_time() });
        }
    }
    p.files.fetch_add(out.files.len() as u64, Relaxed);
    p.dirs.fetch_add(subdirs.len() as u64, Relaxed);
    out.dirs = subdirs
        .into_par_iter()
        .map(|(path, name, mtime)| scan_dir(&path, name, mtime, p))
        .collect();
    out
}

fn flatten(d: TmpDir, parent: u32, cluster: u64, nodes: &mut Vec<Node>) -> u32 {
    let me = nodes.len() as u32;
    let mut n = Node::new(d.name, true);
    n.parent = parent;
    n.mtime = d.mtime;
    nodes.push(n);
    let mut children = Vec::with_capacity(d.files.len() + d.dirs.len());
    for sub in d.dirs {
        children.push(flatten(sub, me, cluster, nodes));
    }
    for f in d.files {
        let idx = nodes.len() as u32;
        let mut n = Node::new(f.name, false);
        n.parent = me;
        n.size = f.size;
        n.alloc = f.size.div_ceil(cluster) * cluster;
        n.mtime = f.mtime;
        nodes.push(n);
        children.push(idx);
    }
    nodes[me as usize].children = children;
    me
}

pub fn scan(root: &str, cluster: u64, p: &Progress) -> Result<Tree, String> {
    let start = Instant::now();
    let path = Path::new(root);
    let md = std::fs::metadata(path).map_err(|e| format!("{root}: {e}"))?;
    if !md.is_dir() {
        return Err(format!("{root} 은(는) 폴더가 아닙니다"));
    }
    let tmp = scan_dir(path, "".into(), md.last_write_time(), p);
    if p.cancelled() {
        return Err("중지됨".into());
    }
    let mut nodes = Vec::with_capacity((p.files.load(Relaxed) + p.dirs.load(Relaxed) + 1) as usize);
    let cluster = cluster.max(1);
    flatten(tmp, NO_PARENT, cluster, &mut nodes);
    let mut t = Tree {
        nodes,
        root: 0,
        root_path: root.to_string(),
        errors: p.errors.load(Relaxed),
        method: Method::Walk,
        elapsed: start.elapsed(),
        cluster_size: cluster,
    };
    t.aggregate();
    t.elapsed = start.elapsed();
    Ok(t)
}
