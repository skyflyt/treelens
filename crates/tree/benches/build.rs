//! Benchmark Tree::build + the hot read queries on a synthetic wide/deep tree.
//!
//! Run with `cargo bench -p tree`. CI only compiles it (`cargo bench --no-run`)
//! to keep the build honest without spending wall-clock on timing in CI.

use criterion::{criterion_group, criterion_main, Criterion};
use scanner::Record;
use std::hint::black_box;
use tree::{list_dir, search, SearchKind, SearchOpts, SizeMode, SortKey, Tree};

/// Build a synthetic record set: `dirs` directories each holding
/// `files_per_dir` files, all under one root. ~dirs*files_per_dir nodes.
fn make_records(dirs: u32, files_per_dir: u32) -> Vec<Record> {
    let mut recs = Vec::with_capacity((dirs * (files_per_dir + 1) + 1) as usize);
    recs.push(Record {
        idx: 0,
        name: "root".into(),
        parent: u32::MAX,
        logical: 0,
        allocated: 0,
        mtime: 1_700_000_000,
        flags: scanner::FLAG_DIR,
    });
    let mut next = 1u32;
    for d in 0..dirs {
        let dir_idx = next;
        next += 1;
        recs.push(Record {
            idx: dir_idx,
            name: format!("dir{d}"),
            parent: 0,
            logical: 0,
            allocated: 0,
            mtime: 1_700_000_000,
            flags: scanner::FLAG_DIR,
        });
        for f in 0..files_per_dir {
            let size = ((d as u64 * 31 + f as u64 * 7) % 100_000) + 1;
            recs.push(Record {
                idx: next,
                name: format!("file{d}_{f}.bin"),
                parent: dir_idx,
                logical: size,
                allocated: (size + 4095) & !4095,
                mtime: 1_700_000_000,
                flags: 0,
            });
            next += 1;
        }
    }
    recs
}

fn bench(c: &mut Criterion) {
    let records = make_records(500, 200); // ~100k nodes

    c.bench_function("tree_build_100k", |b| {
        b.iter(|| {
            let recs = records.clone();
            black_box(Tree::build(black_box(recs)));
        })
    });

    let tree = Tree::build(records.clone());

    c.bench_function("list_dir_root", |b| {
        b.iter(|| {
            black_box(list_dir(
                &tree,
                0,
                SortKey::SizeDesc,
                0,
                100,
                SizeMode::Allocated,
            ))
        })
    });

    c.bench_function("search_substring", |b| {
        let opts = SearchOpts {
            query: "file1_".into(),
            min_size: 0,
            kind: SearchKind::All,
            limit: 500,
        };
        b.iter(|| black_box(search(&tree, 0, &opts, SizeMode::Allocated)))
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
