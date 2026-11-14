#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod scan_mft;
mod scan_walk;
mod tree;
mod treemap;
mod win;

use eframe::egui;
use std::sync::atomic::Ordering::Relaxed;
use tree::{Method, Progress, Tree};

/// Normalise "c:", "C:\foo\" etc. to "C:\" / "C:\foo".
pub fn normalize_path(p: &str) -> String {
    let p = p.trim().trim_matches('"').replace('/', "\\");
    let mut chars: Vec<char> = p.chars().collect();
    if chars.len() >= 2 && chars[1] == ':' {
        chars[0] = chars[0].to_ascii_uppercase();
    }
    let p: String = chars.into_iter().collect();
    if p.len() == 2 && p.ends_with(':') {
        return format!("{p}\\");
    }
    if p.len() > 3 {
        return p.trim_end_matches('\\').to_string();
    }
    p
}

/// Run a scan with the requested method. If MFT fails (no admin rights, not NTFS),
/// fall back to the ordinary method and say why.
pub fn run_scan(path: &str, method: Method, p: &Progress) -> Result<(Tree, Option<String>), String> {
    let root = path.get(..3).unwrap_or(path);
    if method == Method::Mft {
        match scan_mft::scan(path, p) {
            Ok(t) => return Ok((t, None)),
            Err(e) if p.cancelled() => return Err(e),
            Err(e) => {
                for c in [&p.files, &p.dirs, &p.errors, &p.bytes_done, &p.bytes_total] {
                    c.store(0, Relaxed);
                }
                let t = scan_walk::scan(path, win::cluster_size(root), p)?;
                return Ok((t, Some(format!("MFT 실패, 일반 방식으로 훑음: {e}"))));
            }
        }
    }
    scan_walk::scan(path, win::cluster_size(root), p).map(|t| (t, None))
}

fn cli(args: &[String]) {
    win::attach_parent_console();
    let path = normalize_path(args.get(2).map(String::as_str).unwrap_or("C:\\"));
    let method = if args.iter().any(|a| a == "--walk") { Method::Walk } else { Method::Mft };
    let p = Progress::default();
    match run_scan(&path, method, &p) {
        Ok((t, note)) => {
            let r = &t.nodes[t.root as usize];
            println!();
            if let Some(n) = note {
                println!("{n}");
            }
            println!(
                "{path} method={:?} time={:.2}s size={} alloc={} files={} dirs={} errors={}",
                t.method,
                t.elapsed.as_secs_f64(),
                tree::fmt_size(r.size),
                tree::fmt_size(r.alloc),
                tree::fmt_count(r.files),
                tree::fmt_count(r.dirs),
                t.errors
            );
            let mut kids: Vec<u32> = r.children.clone();
            kids.sort_by_key(|&c| std::cmp::Reverse(t.nodes[c as usize].size));
            for c in kids.into_iter().take(12) {
                let n = &t.nodes[c as usize];
                println!("  {:>10}  {:>10}  {}", tree::fmt_size(n.size), tree::fmt_size(n.alloc), n.name);
            }
        }
        Err(e) => println!("\nerror: {e}"),
    }
}

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--cli") {
        cli(&args);
        return Ok(());
    }
    let start_path = args.get(1).map(|s| normalize_path(s));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("oddity-diskmap")
            .with_inner_size([1280.0, 900.0])
            .with_min_inner_size([640.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "oddity-diskmap",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, start_path)))),
    )
}

#[cfg(test)]
mod tests {
    use super::normalize_path;

    #[test]
    fn paths() {
        assert_eq!(normalize_path("c:"), "C:\\");
        assert_eq!(normalize_path("c:\\"), "C:\\");
        assert_eq!(normalize_path("\"D:/Users/\""), "D:\\Users");
    }
}
