//! Treelens arena tree, aggregation, queries, and treemap layout.
//!
//! The scanner streams [`scanner::Record`] values; this crate builds them into a
//! flat `Vec<Node>` arena, rolls up sizes bottom-up, and answers narrow queries
//! that the frontend can render directly. The tree never crosses the IPC
//! boundary — see PLAN.md §3.1.
//!
//! v0.1 deliberately keeps the node layout readable rather than packed; the
//! sub-48-byte target in PLAN.md §3.3 is a v0.2 optimization.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const FLAG_DIR: u16 = 1 << 0;
pub const FLAG_REPARSE: u16 = 1 << 1;
pub const FLAG_UNSCANNABLE: u16 = 1 << 2;
pub const FLAG_HIDDEN: u16 = 1 << 3;
pub const FLAG_SYSTEM: u16 = 1 << 4;
pub const FLAG_READONLY: u16 = 1 << 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub name: String,
    pub parent: u32,
    pub first_child: u32, // u32::MAX if none
    pub child_count: u32,
    pub logical: u64,
    pub allocated: u64,
    pub mtime: i64,
    pub file_count: u64,    // for dirs: rolled-up count of files in subtree
    pub dir_count: u64,     // for dirs: rolled-up count of subdirs
    pub newest_mtime: i64,  // max mtime in subtree (for age-heat)
    pub oldest_mtime: i64,  // min mtime in subtree
    pub flags: u16,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        self.flags & FLAG_DIR != 0
    }
    pub fn is_reparse(&self) -> bool {
        self.flags & FLAG_REPARSE != 0
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SizeMode {
    Allocated,
    Logical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tree {
    pub nodes: Vec<Node>,
    pub root: u32,
}

impl Tree {
    /// Build a tree from a flat record stream (scanner output).
    ///
    /// Records are expected in scanner-emission order: the root first, then
    /// children in whatever order the parallel walk produced them. We bucket
    /// children by parent, then write them out contiguously and rewire
    /// `first_child`/`child_count`.
    pub fn build(records: Vec<scanner::Record>) -> Self {
        let n = records.len();
        if n == 0 {
            return Tree {
                nodes: vec![],
                root: 0,
            };
        }

        // First pass: build per-parent child lists, keyed by scanner-local index.
        // We accept the BTreeMap cost — at v0.1 scales (millions of files) the
        // per-parent dispatch is dominated by the file IO time.
        let mut children_of: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (i, r) in records.iter().enumerate() {
            if r.parent != u32::MAX {
                children_of.entry(r.parent).or_default().push(i as u32);
            }
        }

        // We want children stored contiguously after their parent so `first_child +
        // child_count` slicing works without an index table. Easiest scheme:
        // depth-first reorder from the root.
        let mut order: Vec<u32> = Vec::with_capacity(n);
        let mut stack: Vec<u32> = vec![0];
        while let Some(idx) = stack.pop() {
            order.push(idx);
            if let Some(children) = children_of.get(&idx) {
                // Push in reverse so the first child comes off the stack first.
                for &c in children.iter().rev() {
                    stack.push(c);
                }
            }
        }

        // Map scanner-local index -> new arena index.
        let mut remap = vec![0u32; n];
        for (new_idx, &old_idx) in order.iter().enumerate() {
            remap[old_idx as usize] = new_idx as u32;
        }

        // Build new nodes, fixing parent pointers and inserting child ranges.
        let mut nodes: Vec<Node> = Vec::with_capacity(n);
        for &old_idx in &order {
            let r = &records[old_idx as usize];
            let mut flags = r.flags;
            // Promote scanner flags. Reparse never gets descended; ensure the dir flag
            // is preserved (the scanner sets DIR only for non-reparse dirs).
            if r.flags & scanner::FLAG_DIR != 0 {
                flags |= FLAG_DIR;
            }
            if r.flags & scanner::FLAG_REPARSE != 0 {
                flags |= FLAG_REPARSE;
            }
            if r.flags & scanner::FLAG_HIDDEN != 0 {
                flags |= FLAG_HIDDEN;
            }
            if r.flags & scanner::FLAG_SYSTEM != 0 {
                flags |= FLAG_SYSTEM;
            }
            if r.flags & scanner::FLAG_READONLY != 0 {
                flags |= FLAG_READONLY;
            }
            let parent = if r.parent == u32::MAX {
                u32::MAX
            } else {
                remap[r.parent as usize]
            };
            nodes.push(Node {
                name: r.name.clone(),
                parent,
                first_child: u32::MAX,
                child_count: 0,
                logical: r.logical,
                allocated: r.allocated,
                mtime: r.mtime,
                file_count: 0,
                dir_count: 0,
                newest_mtime: r.mtime,
                oldest_mtime: if r.mtime > 0 { r.mtime } else { i64::MAX },
                flags,
            });
        }

        // Wire children ranges. Because of the DFS order, every parent's children appear
        // as a contiguous block — but not necessarily right after the parent. We need a
        // direct scan: for each new node, if its parent isn't MAX, extend the parent's
        // child range.
        // children_of is keyed by old idx; convert.
        let mut new_children: Vec<Vec<u32>> = vec![Vec::new(); n];
        for (old_parent, kids) in &children_of {
            let np = remap[*old_parent as usize] as usize;
            for &k in kids {
                new_children[np].push(remap[k as usize]);
            }
        }
        // Sort each parent's children by their NEW index so they form a contiguous run.
        for (parent_idx, kids) in new_children.iter_mut().enumerate() {
            if kids.is_empty() {
                continue;
            }
            kids.sort_unstable();
            // Sanity: contiguous range starting at kids[0].
            let first = kids[0];
            nodes[parent_idx].first_child = first;
            nodes[parent_idx].child_count = kids.len() as u32;
        }

        // Bottom-up aggregation: traverse new arena in reverse to roll sizes into parents.
        // Each iteration first finalizes the current node's own counts (1 if file,
        // 1 dir contribution if directory) then propagates the aggregated totals to
        // its parent. Walking back-to-front guarantees children are finalized first.
        for i in (0..n).rev() {
            // Stamp self counts on leaves before we look at them.
            {
                let node = &mut nodes[i];
                if !node.is_dir() {
                    node.file_count = 1;
                }
            }
            let (logical, allocated, file_count, dir_count, newest, oldest, parent, is_dir) = {
                let node = &nodes[i];
                (
                    node.logical,
                    node.allocated,
                    node.file_count,
                    node.dir_count + if node.is_dir() { 1 } else { 0 },
                    node.newest_mtime,
                    node.oldest_mtime,
                    node.parent,
                    node.is_dir(),
                )
            };
            if i != 0 && parent != u32::MAX {
                let p = &mut nodes[parent as usize];
                p.logical = p.logical.saturating_add(logical);
                p.allocated = p.allocated.saturating_add(allocated);
                p.file_count = p.file_count.saturating_add(file_count);
                p.dir_count = p.dir_count.saturating_add(dir_count);
                if newest > p.newest_mtime {
                    p.newest_mtime = newest;
                }
                if oldest < p.oldest_mtime {
                    p.oldest_mtime = oldest;
                }
            }
            let _ = is_dir;
        }
        // Normalize "no mtime" sentinel.
        for node in nodes.iter_mut() {
            if node.oldest_mtime == i64::MAX {
                node.oldest_mtime = 0;
            }
            // Directories shouldn't count themselves in dir_count of the root summary.
            if node.parent == u32::MAX && node.is_dir() {
                // root dir_count was inflated by its own "+1"; subtract.
                if node.dir_count > 0 {
                    node.dir_count -= 1;
                }
            }
        }

        Tree { nodes, root: 0 }
    }

    pub fn size_of(&self, idx: u32, mode: SizeMode) -> u64 {
        let n = &self.nodes[idx as usize];
        match mode {
            SizeMode::Allocated => n.allocated,
            SizeMode::Logical => n.logical,
        }
    }

    pub fn root_size(&self, mode: SizeMode) -> u64 {
        self.size_of(self.root, mode)
    }

    pub fn children(&self, idx: u32) -> &[Node] {
        let n = &self.nodes[idx as usize];
        if n.child_count == 0 {
            return &[];
        }
        let start = n.first_child as usize;
        &self.nodes[start..start + n.child_count as usize]
    }

    pub fn child_indexes(&self, idx: u32) -> std::ops::Range<u32> {
        let n = &self.nodes[idx as usize];
        let start = n.first_child;
        start..(start + n.child_count)
    }

    /// Walk from root to `idx`, returning each ancestor's (index, name).
    pub fn path(&self, idx: u32) -> Vec<(u32, String)> {
        let mut out: Vec<(u32, String)> = Vec::new();
        let mut cur = idx;
        loop {
            let n = &self.nodes[cur as usize];
            out.push((cur, n.name.clone()));
            if n.parent == u32::MAX {
                break;
            }
            cur = n.parent;
        }
        out.reverse();
        out
    }
}

// ---------- List + top-N queries ----------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SortKey {
    SizeDesc,
    SizeAsc,
    NameAsc,
    NameDesc,
    MtimeDesc,
    MtimeAsc,
    CountDesc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirRow {
    pub idx: u32,
    pub name: String,
    pub size: u64,
    pub pct_parent: f32,
    pub pct_root: f32,
    pub file_count: u64,
    pub mtime: i64,
    pub is_dir: bool,
    pub is_reparse: bool,
}

pub fn list_dir(
    tree: &Tree,
    parent: u32,
    sort: SortKey,
    offset: usize,
    limit: usize,
    mode: SizeMode,
) -> Vec<DirRow> {
    let parent_size = tree.size_of(parent, mode).max(1);
    let root_size = tree.root_size(mode).max(1);
    let mut ids: Vec<u32> = tree.child_indexes(parent).collect();
    sort_ids(&mut ids, tree, sort, mode);
    ids.into_iter()
        .skip(offset)
        .take(limit)
        .map(|i| row_for(tree, i, parent_size, root_size, mode))
        .collect()
}

pub fn dir_count(tree: &Tree, parent: u32) -> usize {
    tree.nodes[parent as usize].child_count as usize
}

fn row_for(tree: &Tree, i: u32, parent_size: u64, root_size: u64, mode: SizeMode) -> DirRow {
    let n = &tree.nodes[i as usize];
    let size = match mode {
        SizeMode::Allocated => n.allocated,
        SizeMode::Logical => n.logical,
    };
    DirRow {
        idx: i,
        name: n.name.clone(),
        size,
        pct_parent: (size as f64 / parent_size as f64) as f32,
        pct_root: (size as f64 / root_size as f64) as f32,
        file_count: if n.is_dir() { n.file_count } else { 1 },
        mtime: n.mtime,
        is_dir: n.is_dir(),
        is_reparse: n.is_reparse(),
    }
}

fn sort_ids(ids: &mut [u32], tree: &Tree, sort: SortKey, mode: SizeMode) {
    let sz = |i: u32| match mode {
        SizeMode::Allocated => tree.nodes[i as usize].allocated,
        SizeMode::Logical => tree.nodes[i as usize].logical,
    };
    let nm = |i: u32| tree.nodes[i as usize].name.to_ascii_lowercase();
    let mt = |i: u32| tree.nodes[i as usize].mtime;
    let ct = |i: u32| tree.nodes[i as usize].file_count;
    match sort {
        SortKey::SizeDesc => ids.sort_unstable_by(|a, b| sz(*b).cmp(&sz(*a))),
        SortKey::SizeAsc => ids.sort_unstable_by(|a, b| sz(*a).cmp(&sz(*b))),
        SortKey::NameAsc => ids.sort_unstable_by(|a, b| nm(*a).cmp(&nm(*b))),
        SortKey::NameDesc => ids.sort_unstable_by(|a, b| nm(*b).cmp(&nm(*a))),
        SortKey::MtimeDesc => ids.sort_unstable_by(|a, b| mt(*b).cmp(&mt(*a))),
        SortKey::MtimeAsc => ids.sort_unstable_by(|a, b| mt(*a).cmp(&mt(*b))),
        SortKey::CountDesc => ids.sort_unstable_by(|a, b| ct(*b).cmp(&ct(*a))),
    }
}

/// Top-N largest files in the subtree rooted at `root_idx`.
pub fn top_files(tree: &Tree, root_idx: u32, n: usize, mode: SizeMode) -> Vec<DirRow> {
    let root_size = tree.root_size(mode).max(1);
    let mut heap: std::collections::BinaryHeap<std::cmp::Reverse<(u64, u32)>> =
        std::collections::BinaryHeap::with_capacity(n + 1);
    walk_subtree(tree, root_idx, &mut |idx, node| {
        if !node.is_dir() {
            let size = match mode {
                SizeMode::Allocated => node.allocated,
                SizeMode::Logical => node.logical,
            };
            heap.push(std::cmp::Reverse((size, idx)));
            if heap.len() > n {
                heap.pop();
            }
        }
    });
    let mut out: Vec<(u64, u32)> = heap.into_iter().map(|r| r.0).collect();
    out.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    out.into_iter()
        .map(|(_, i)| {
            let parent_size = if tree.nodes[i as usize].parent == u32::MAX {
                root_size
            } else {
                tree.size_of(tree.nodes[i as usize].parent, mode).max(1)
            };
            row_for(tree, i, parent_size, root_size, mode)
        })
        .collect()
}

/// Top-N directories (excluding the queried root) by size.
pub fn top_dirs(tree: &Tree, root_idx: u32, n: usize, mode: SizeMode) -> Vec<DirRow> {
    let root_size = tree.root_size(mode).max(1);
    let mut heap: std::collections::BinaryHeap<std::cmp::Reverse<(u64, u32)>> =
        std::collections::BinaryHeap::with_capacity(n + 1);
    walk_subtree(tree, root_idx, &mut |idx, node| {
        if node.is_dir() && idx != root_idx {
            let size = match mode {
                SizeMode::Allocated => node.allocated,
                SizeMode::Logical => node.logical,
            };
            heap.push(std::cmp::Reverse((size, idx)));
            if heap.len() > n {
                heap.pop();
            }
        }
    });
    let mut out: Vec<(u64, u32)> = heap.into_iter().map(|r| r.0).collect();
    out.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    out.into_iter()
        .map(|(_, i)| {
            let parent_size = if tree.nodes[i as usize].parent == u32::MAX {
                root_size
            } else {
                tree.size_of(tree.nodes[i as usize].parent, mode).max(1)
            };
            row_for(tree, i, parent_size, root_size, mode)
        })
        .collect()
}

fn walk_subtree<F: FnMut(u32, &Node)>(tree: &Tree, root: u32, f: &mut F) {
    // Iterative DFS using a vec-based stack to avoid stack overflow on deep trees.
    let mut stack: Vec<u32> = vec![root];
    while let Some(idx) = stack.pop() {
        let n = &tree.nodes[idx as usize];
        f(idx, n);
        if n.child_count > 0 {
            let start = n.first_child;
            for c in start..start + n.child_count {
                stack.push(c);
            }
        }
    }
}

// ---------- Squarified treemap ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rect {
    pub idx: u32,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub size: u64,
    /// Subtree's newest_mtime (for age-heat coloring).
    pub newest_mtime: i64,
    /// Subtree's oldest_mtime.
    pub oldest_mtime: i64,
    pub is_dir: bool,
    pub depth: u16,
    /// File or directory name. Included so the canvas renderer can pick a
    /// file-type hue and draw a label without a follow-up name lookup.
    pub name: String,
}

#[derive(Debug, Clone, Copy)]
pub struct LayoutOpts {
    pub width: f32,
    pub height: f32,
    pub min_px: f32, // minimum side length to keep a rect
    pub max_depth: u16,
    pub padding: f32,
}

impl Default for LayoutOpts {
    fn default() -> Self {
        Self {
            width: 1280.0,
            height: 720.0,
            min_px: 3.0,
            max_depth: 4,
            padding: 1.0,
        }
    }
}

/// Compute a squarified treemap layout for the subtree rooted at `root_idx`.
///
/// Returns a flat list of rects (one per visible node). Frontend renders the
/// list directly. Layout is bounded in size by the `min_px` cutoff.
pub fn treemap_layout(
    tree: &Tree,
    root_idx: u32,
    opts: LayoutOpts,
    mode: SizeMode,
) -> Vec<Rect> {
    let mut out: Vec<Rect> = Vec::new();
    let root_size = tree.size_of(root_idx, mode);
    if root_size == 0 {
        return out;
    }
    let root = &tree.nodes[root_idx as usize];
    out.push(Rect {
        idx: root_idx,
        x: 0.0,
        y: 0.0,
        w: opts.width,
        h: opts.height,
        size: root_size,
        newest_mtime: root.newest_mtime,
        oldest_mtime: root.oldest_mtime,
        is_dir: root.is_dir(),
        depth: 0,
        name: root.name.clone(),
    });
    layout_recurse(
        tree,
        root_idx,
        0.0,
        0.0,
        opts.width,
        opts.height,
        root_size,
        mode,
        0,
        &opts,
        &mut out,
    );
    out
}

#[allow(clippy::too_many_arguments)]
fn layout_recurse(
    tree: &Tree,
    parent: u32,
    px: f32,
    py: f32,
    pw: f32,
    ph: f32,
    parent_size: u64,
    mode: SizeMode,
    depth: u16,
    opts: &LayoutOpts,
    out: &mut Vec<Rect>,
) {
    if depth >= opts.max_depth {
        return;
    }
    let kids: Vec<u32> = tree.child_indexes(parent).collect();
    if kids.is_empty() || pw < opts.min_px || ph < opts.min_px {
        return;
    }
    // Sort by size desc.
    let mut sized: Vec<(u32, u64)> = kids
        .into_iter()
        .map(|i| (i, tree.size_of(i, mode)))
        .filter(|(_, s)| *s > 0)
        .collect();
    sized.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    if sized.is_empty() {
        return;
    }

    // Squarify: greedily build rows by trying to keep aspect ratio close to 1.
    let total: u64 = sized.iter().map(|(_, s)| *s).sum();
    if total == 0 {
        return;
    }

    let mut remaining = sized.as_slice();
    let mut rect_x = px;
    let mut rect_y = py;
    let mut rect_w = pw;
    let mut rect_h = ph;
    let mut remaining_size = total;

    while !remaining.is_empty() && rect_w > opts.min_px && rect_h > opts.min_px {
        let short = rect_w.min(rect_h);
        // Choose row length by picking children while worst aspect ratio improves.
        let mut row_count = 0usize;
        let mut row_sum: u64 = 0;
        let mut best_worst = f32::INFINITY;
        for (i, (_, s)) in remaining.iter().enumerate() {
            let candidate_sum = row_sum + *s;
            let worst = worst_aspect(remaining, i + 1, candidate_sum, short, rect_w, rect_h, remaining_size);
            if worst < best_worst {
                best_worst = worst;
                row_count = i + 1;
                row_sum = candidate_sum;
            } else {
                break;
            }
        }
        if row_count == 0 {
            break;
        }
        let row = &remaining[..row_count];
        // Decide row orientation: lay along the long edge.
        let row_frac = row_sum as f64 / remaining_size as f64;
        let row_thickness =
            (if rect_w >= rect_h { rect_w } else { rect_h } as f64 * row_frac) as f32;
        let along_w = rect_w >= rect_h;
        let along_len = if along_w { rect_h } else { rect_w };

        let mut cursor = if along_w { rect_y } else { rect_x };
        for (idx, sz) in row.iter() {
            let frac = *sz as f64 / row_sum as f64;
            let len = (along_len as f64 * frac) as f32;
            let (rx, ry, rw, rh) = if along_w {
                (rect_x, cursor, row_thickness, len)
            } else {
                (cursor, rect_y, len, row_thickness)
            };
            // Apply 1px padding (purely visual).
            let pad = opts.padding.min(rw.min(rh) * 0.25);
            let ix = rx + pad;
            let iy = ry + pad;
            let iw = (rw - pad * 2.0).max(0.0);
            let ih = (rh - pad * 2.0).max(0.0);
            if iw >= opts.min_px && ih >= opts.min_px {
                let node = &tree.nodes[*idx as usize];
                out.push(Rect {
                    idx: *idx,
                    x: ix,
                    y: iy,
                    w: iw,
                    h: ih,
                    size: *sz,
                    newest_mtime: node.newest_mtime,
                    oldest_mtime: node.oldest_mtime,
                    is_dir: node.is_dir(),
                    depth: depth + 1,
                    name: node.name.clone(),
                });
                if node.is_dir() && depth + 1 < opts.max_depth {
                    layout_recurse(
                        tree,
                        *idx,
                        ix,
                        iy,
                        iw,
                        ih,
                        *sz,
                        mode,
                        depth + 1,
                        opts,
                        out,
                    );
                }
            }
            cursor += len;
        }
        // Carve the row off the parent rect.
        if along_w {
            rect_x += row_thickness;
            rect_w -= row_thickness;
        } else {
            rect_y += row_thickness;
            rect_h -= row_thickness;
        }
        remaining_size -= row_sum;
        remaining = &remaining[row_count..];
    }
    let _ = parent_size; // currently unused (we use the slice's total instead)
}

fn worst_aspect(
    items: &[(u32, u64)],
    row_count: usize,
    row_sum: u64,
    _short: f32,
    rect_w: f32,
    rect_h: f32,
    remaining_size: u64,
) -> f32 {
    if row_sum == 0 || row_count == 0 || remaining_size == 0 {
        return f32::INFINITY;
    }
    let along_w = rect_w >= rect_h;
    let row_thickness = if along_w {
        rect_w * (row_sum as f32 / remaining_size as f32)
    } else {
        rect_h * (row_sum as f32 / remaining_size as f32)
    };
    let along_len = if along_w { rect_h } else { rect_w };

    let mut worst = 0f32;
    for (_, s) in &items[..row_count] {
        let frac = *s as f32 / row_sum as f32;
        let len = along_len * frac;
        if len == 0.0 || row_thickness == 0.0 {
            return f32::INFINITY;
        }
        let ar = (row_thickness / len).max(len / row_thickness);
        if ar > worst {
            worst = ar;
        }
    }
    worst
}

#[cfg(test)]
mod tests {
    use super::*;
    use scanner::Record;

    fn rec(name: &str, parent: u32, size: u64, is_dir: bool) -> Record {
        Record {
            name: name.into(),
            parent,
            logical: if is_dir { 0 } else { size },
            allocated: if is_dir { 0 } else { (size + 4095) & !4095 },
            mtime: 1_700_000_000,
            flags: if is_dir { scanner::FLAG_DIR } else { 0 },
        }
    }

    fn flat() -> Tree {
        // root/
        //   a.bin (100)
        //   b.bin (300)
        //   sub/
        //     c.bin (50)
        let records = vec![
            rec("root", u32::MAX, 0, true),
            rec("a.bin", 0, 100, false),
            rec("b.bin", 0, 300, false),
            rec("sub", 0, 0, true),
            rec("c.bin", 3, 50, false),
        ];
        Tree::build(records)
    }

    #[test]
    fn aggregation_rolls_sizes_up() {
        let t = flat();
        // root logical = 100 + 300 + 50
        assert_eq!(t.nodes[0].logical, 450);
        // sub logical = 50
        let sub_idx = t.nodes.iter().position(|n| n.name == "sub").unwrap();
        assert_eq!(t.nodes[sub_idx].logical, 50);
        // file_count root = 3
        assert_eq!(t.nodes[0].file_count, 3);
    }

    #[test]
    fn list_dir_returns_children_sorted_by_size() {
        let t = flat();
        let rows = list_dir(&t, 0, SortKey::SizeDesc, 0, 100, SizeMode::Logical);
        assert!(!rows.is_empty());
        assert_eq!(rows[0].name, "b.bin"); // 300
    }

    #[test]
    fn top_files_returns_largest() {
        let t = flat();
        let top = top_files(&t, 0, 2, SizeMode::Logical);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].name, "b.bin");
        assert_eq!(top[1].name, "a.bin");
    }

    #[test]
    fn treemap_layout_produces_rects() {
        let t = flat();
        let opts = LayoutOpts {
            width: 400.0,
            height: 300.0,
            min_px: 1.0,
            max_depth: 4,
            padding: 0.0,
        };
        let rects = treemap_layout(&t, 0, opts, SizeMode::Logical);
        // Root + 3 children + 1 grandchild = 5
        assert!(rects.len() >= 4);
    }
}
