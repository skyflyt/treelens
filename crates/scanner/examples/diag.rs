//! Diagnostic CLI: `cargo run -p scanner --example diag --release -- <path>`
//!
//! Scans the given path with the v0.1 scanner + tree crate and dumps the
//! totals, reparse counts, and the top-50 directories and files so we can
//! see what the UI actually had to work with.

use scanner::{spawn, Cancel, ScanEvent, ScanOptions, FLAG_DIR, FLAG_REPARSE};
use std::env;
use std::path::PathBuf;

fn main() {
    let mut args = env::args().skip(1);
    let path: PathBuf = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\"));
    eprintln!("scanning {}", path.display());

    let opts = ScanOptions::new(&path);
    let cancel = Cancel::new();
    let (rec_rx, evt_rx, h) = spawn(opts, cancel, 16_384, 32);

    let mut records: Vec<scanner::Record> = Vec::with_capacity(1 << 20);
    loop {
        crossbeam_channel::select! {
            recv(rec_rx) -> r => match r {
                Ok(rec) => records.push(rec),
                Err(_) => break,
            },
            recv(evt_rx) -> e => if let Ok(ScanEvent::Progress(p)) = e {
                if p.elapsed_ms % 2000 < 100 {
                    eprintln!(
                        "progress: {} files, {} bytes, {} dirs, {} errors, {}ms",
                        p.files_seen, p.bytes_seen, p.dirs_seen, p.errors, p.elapsed_ms
                    );
                }
            }
        }
    }
    let _ = h.join();

    eprintln!("records: {}", records.len());

    let total_logical: u64 = records.iter().map(|r| r.logical).sum();
    let total_allocated: u64 = records.iter().map(|r| r.allocated).sum();
    let dir_count = records.iter().filter(|r| r.flags & FLAG_DIR != 0).count();
    let file_count = records
        .iter()
        .filter(|r| r.flags & FLAG_DIR == 0 && r.flags & FLAG_REPARSE == 0)
        .count();
    let reparse_count = records
        .iter()
        .filter(|r| r.flags & FLAG_REPARSE != 0)
        .count();

    println!("---- totals ----");
    println!("records     : {}", records.len());
    println!("dirs        : {}", dir_count);
    println!("files       : {}", file_count);
    println!("reparse pts : {}", reparse_count);
    println!(
        "total logical:  {} ({:.2} GiB)",
        total_logical,
        total_logical as f64 / 1024.0 / 1024.0 / 1024.0
    );
    println!(
        "total alloc.  : {} ({:.2} GiB)",
        total_allocated,
        total_allocated as f64 / 1024.0 / 1024.0 / 1024.0
    );

    let tree = tree::Tree::build(records);

    println!("\n---- root (rolled-up) ----");
    let root = &tree.nodes[0];
    println!("name        : {}", root.name);
    println!(
        "logical     : {} ({:.2} GiB)",
        root.logical,
        root.logical as f64 / 1024.0 / 1024.0 / 1024.0
    );
    println!(
        "allocated   : {} ({:.2} GiB)",
        root.allocated,
        root.allocated as f64 / 1024.0 / 1024.0 / 1024.0
    );
    println!("file_count  : {}", root.file_count);
    println!("dir_count   : {}", root.dir_count);

    println!("\n---- top 30 dirs (rolled-up, sorted by allocated desc) ----");
    let mut top = tree::top_dirs(&tree, 0, 30, tree::SizeMode::Allocated);
    for r in &mut top {
        let path_str = path_for(&tree, r.idx);
        println!(
            "{:>10}  {:>5}  {:<60}  {}",
            human(r.size),
            r.file_count.to_string(),
            truncate_left(&path_str, 60),
            r.name
        );
    }

    println!("\n---- top 30 files ----");
    let top_files = tree::top_files(&tree, 0, 30, tree::SizeMode::Allocated);
    for r in &top_files {
        let path_str = path_for(&tree, r.idx);
        println!(
            "{:>10}  {:<60}  {}",
            human(r.size),
            truncate_left(&path_str, 60),
            r.name
        );
    }

    println!("\n---- direct children of root (Contents view) ----");
    let kids = tree::list_dir(
        &tree,
        0,
        tree::SortKey::SizeDesc,
        0,
        50,
        tree::SizeMode::Allocated,
    );
    for r in &kids {
        println!(
            "{:>10}  {:>6}  {:>7}  is_dir={}  is_reparse={}  {}",
            human(r.size),
            r.file_count.to_string(),
            format!("{:.1}%", r.pct_root * 100.0),
            r.is_dir,
            r.is_reparse,
            r.name
        );
    }
}

fn path_for(tree: &tree::Tree, idx: u32) -> String {
    tree.path(idx)
        .iter()
        .skip(1)
        .map(|(_, n)| n.as_str())
        .collect::<Vec<_>>()
        .join("\\")
}

fn truncate_left(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let skip = s.chars().count() - n + 1;
    format!("…{}", s.chars().skip(skip).collect::<String>())
}

fn human(b: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{:.1} {}", v, UNITS[i])
}
