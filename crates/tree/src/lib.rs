// Sort-by-closure is the natural form for these comparators (some need
// case-insensitive name compare; some are sized for the key fn cost). The
// "use sort_unstable_by_key" suggestion would either allocate per comparison
// or obscure intent — allowing per-crate.
#![allow(clippy::unnecessary_sort_by)]

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
    pub file_count: u64,   // for dirs: rolled-up count of files in subtree
    pub dir_count: u64,    // for dirs: rolled-up count of subdirs
    pub newest_mtime: i64, // max mtime in subtree (for age-heat)
    pub oldest_mtime: i64, // min mtime in subtree
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
    pub fn build(mut records: Vec<scanner::Record>) -> Self {
        let n = records.len();
        if n == 0 {
            return Tree {
                nodes: vec![],
                root: 0,
            };
        }

        // The scanner emits records concurrently from many workers, so emission
        // order ≠ scanner-idx order. Sort so Vec-position == idx; this restores
        // the invariant that every record's `parent` field is an index into THIS
        // Vec. (Without this step, parent pointers cross-attach unrelated
        // subtrees.) Records were originally allocated dense via fetch_add, so
        // after the sort idxs are 0..n.
        records.sort_unstable_by_key(|r| r.idx);

        // The build is keyed by Vec POSITION, not by the scanner idx value, so it
        // stays correct even if the scanner stream has gaps or duplicate idxs
        // (which a cancelled mid-directory scan can produce). We map each
        // scanner idx -> its Vec position once; any record whose parent idx
        // isn't present is treated as an orphan and dropped from the tree rather
        // than corrupting it or panicking. Earlier this relied on a dense-idx
        // `debug_assert` that was compiled out of release builds.
        let mut idx_to_pos: std::collections::HashMap<u32, u32> =
            std::collections::HashMap::with_capacity(n);
        for (pos, r) in records.iter().enumerate() {
            idx_to_pos.insert(r.idx, pos as u32);
        }

        // children_by_pos[parent_pos] = child Vec positions. Orphans (parent idx
        // not found, or self-referential) are simply never added.
        let mut children_by_pos: Vec<Vec<u32>> = vec![Vec::new(); n];
        let mut root_pos: Option<u32> = None;
        for (pos, r) in records.iter().enumerate() {
            if r.parent == u32::MAX {
                if root_pos.is_none() {
                    root_pos = Some(pos as u32);
                }
                continue;
            }
            if let Some(&ppos) = idx_to_pos.get(&r.parent) {
                if ppos as usize != pos {
                    children_by_pos[ppos as usize].push(pos as u32);
                }
            }
        }
        let root_pos = root_pos.unwrap_or(0);

        // BFS (level-order) from the root so each parent's children land in one
        // contiguous run — required for `first_child + child_count` slicing.
        // A `visited` set makes this cycle-safe (a malformed self/loop parent
        // can't spin forever or list a node twice).
        let mut order: Vec<u32> = Vec::with_capacity(n);
        let mut visited = vec![false; n];
        let mut queue: std::collections::VecDeque<u32> = std::collections::VecDeque::new();
        queue.push_back(root_pos);
        visited[root_pos as usize] = true;
        while let Some(pos) = queue.pop_front() {
            order.push(pos);
            for &c in &children_by_pos[pos as usize] {
                if !visited[c as usize] {
                    visited[c as usize] = true;
                    queue.push_back(c);
                }
            }
        }

        // Map old Vec position -> new arena index.
        let mut remap = vec![u32::MAX; n];
        for (new_idx, &old_pos) in order.iter().enumerate() {
            remap[old_pos as usize] = new_idx as u32;
        }

        // Build new nodes, fixing parent pointers and inserting child ranges.
        let mut nodes: Vec<Node> = Vec::with_capacity(order.len());
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
            // Resolve the parent to a NEW arena index via position map. The root
            // (and any node whose parent didn't make it into `order`) gets MAX.
            let parent = if r.parent == u32::MAX {
                u32::MAX
            } else {
                idx_to_pos
                    .get(&r.parent)
                    .map(|&ppos| remap[ppos as usize])
                    .unwrap_or(u32::MAX)
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
        let node_count = nodes.len();

        // Wire children ranges from the BFS order: a parent's children occupy a
        // contiguous run of new indices, so first_child = min, count = len.
        let mut new_children: Vec<Vec<u32>> = vec![Vec::new(); node_count];
        for (new_idx, node) in nodes.iter().enumerate() {
            if node.parent != u32::MAX {
                new_children[node.parent as usize].push(new_idx as u32);
            }
        }
        for (parent_idx, kids) in new_children.iter_mut().enumerate() {
            if kids.is_empty() {
                continue;
            }
            kids.sort_unstable();
            nodes[parent_idx].first_child = kids[0];
            nodes[parent_idx].child_count = kids.len() as u32;
        }

        // Bottom-up aggregation: traverse new arena in reverse to roll sizes into parents.
        // Each iteration first finalizes the current node's own counts (1 if file,
        // 1 dir contribution if directory) then propagates the aggregated totals to
        // its parent. Walking back-to-front guarantees children are finalized first.
        for i in (0..node_count).rev() {
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
    /// Bounds- and cycle-guarded: a malformed `parent` (out of range, or a loop)
    /// can't index-panic or spin forever — it just stops the walk.
    pub fn path(&self, idx: u32) -> Vec<(u32, String)> {
        let mut out: Vec<(u32, String)> = Vec::new();
        let mut cur = idx;
        // Depth cap: no real filesystem path is anywhere near this deep, so this
        // only ever trips on corrupt input (e.g. a self-referential parent).
        let cap = self.nodes.len() + 1;
        for _ in 0..cap {
            let Some(n) = self.nodes.get(cur as usize) else {
                break;
            };
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
/// Top-N directories (excluding the queried root) by size.
///
/// Passthrough ancestors — directories whose size is ≥95% accounted for by a
/// single child — are suppressed in favor of that child. This stops the panel
/// from filling with long ancestor chains like
/// `AppData / Local / Microsoft / OneDrive / logs / ListSync / Local`
/// where every level reports essentially the same number.
pub fn top_dirs(tree: &Tree, root_idx: u32, n: usize, mode: SizeMode) -> Vec<DirRow> {
    let root_size = tree.root_size(mode).max(1);
    let mut heap: std::collections::BinaryHeap<std::cmp::Reverse<(u64, u32)>> =
        std::collections::BinaryHeap::with_capacity(n + 1);
    walk_subtree(tree, root_idx, &mut |idx, node| {
        if node.is_dir() && idx != root_idx && !is_passthrough(tree, idx, mode) {
            let size = match mode {
                SizeMode::Allocated => node.allocated,
                SizeMode::Logical => node.logical,
            };
            if size > 0 {
                heap.push(std::cmp::Reverse((size, idx)));
                if heap.len() > n {
                    heap.pop();
                }
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

/// A directory is a "passthrough" if a single child accounts for ≥95% of its
/// size — meaning the directory itself adds essentially nothing over its child.
/// We hide passthroughs from top_dirs in favor of the child that's doing the
/// actual work.
fn is_passthrough(tree: &Tree, idx: u32, mode: SizeMode) -> bool {
    let my_size = tree.size_of(idx, mode);
    if my_size == 0 {
        return true;
    }
    let mut max_child = 0u64;
    for c in tree.child_indexes(idx) {
        let cs = tree.size_of(c, mode);
        if cs > max_child {
            max_child = cs;
        }
    }
    // 95% is deliberately strict: `AppData/Local/Microsoft` (~91% from OneDrive)
    // STAYS visible because there's real other content alongside; `.silabs/slt/
    // installs` (≈100% from one child) collapses out.
    (max_child as f64) >= (my_size as f64) * 0.95
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

/// Hard upper bound on rects returned from `treemap_layout`. The canvas
/// renderer is O(N) per draw, the JSON serialization is O(N), and the WebView
/// has to allocate one JS object per rect — so an unbounded result will freeze
/// the UI on a pathological subtree even when `min_px` should have filtered it
/// down. 8192 is plenty for any reasonable viewport (4K + min_px=3 caps around
/// ~5K) and stays well under 2 MB of JSON.
pub const MAX_RECTS: usize = 8192;

/// Compute a squarified treemap layout for the subtree rooted at `root_idx`.
///
/// Returns a flat list of rects (one per visible node). Frontend renders the
/// list directly. Layout is bounded in size by the `min_px` cutoff AND by a
/// hard `MAX_RECTS` cap as a defensive ceiling.
pub fn treemap_layout(tree: &Tree, root_idx: u32, opts: LayoutOpts, mode: SizeMode) -> Vec<Rect> {
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
    if depth >= opts.max_depth || out.len() >= MAX_RECTS {
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
            let worst = worst_aspect(
                remaining,
                i + 1,
                candidate_sum,
                short,
                rect_w,
                rect_h,
                remaining_size,
            );
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
                if out.len() >= MAX_RECTS {
                    return;
                }
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
                    layout_recurse(tree, *idx, ix, iy, iw, ih, *sz, mode, depth + 1, opts, out);
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

    fn rec(idx: u32, name: &str, parent: u32, size: u64, is_dir: bool) -> Record {
        Record {
            idx,
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
            rec(0, "root", u32::MAX, 0, true),
            rec(1, "a.bin", 0, 100, false),
            rec(2, "b.bin", 0, 300, false),
            rec(3, "sub", 0, 0, true),
            rec(4, "c.bin", 3, 50, false),
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
    fn build_is_robust_to_emission_order() {
        // Mimic concurrent workers emitting records out of idx order. Records'
        // `parent` fields still reference scanner-local idxs; Tree::build must
        // sort by idx so Vec-position becomes canonical. Without that sort,
        // child records cross-attach to whichever record happens to occupy the
        // Vec position they reference, garbling the tree.
        let mut records = vec![
            rec(0, "root", u32::MAX, 0, true),
            rec(1, "A", 0, 0, true),
            rec(2, "B", 0, 0, true),
            rec(3, "A1", 1, 100, false),
            rec(4, "B1", 2, 200, false),
        ];
        // Shuffle: simulate worker-B-faster emission.
        records.swap(2, 3); // B and A1 swap
        records.swap(1, 4); // A and B1 swap

        let t = Tree::build(records);

        // After sort + build, the tree's structural invariants must hold:
        // root has exactly two dir children (A, B), each holding one file.
        let kids = list_dir(&t, 0, SortKey::NameAsc, 0, 100, SizeMode::Logical);
        let names: Vec<&str> = kids.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, &["A", "B"]);
        assert_eq!(t.nodes[0].logical, 300, "root rolls up to 100+200");
        assert_eq!(t.nodes[0].file_count, 2);
    }

    #[test]
    fn build_tolerates_gaps_and_orphans_without_panic() {
        // Simulate a cancelled mid-directory scan: idx 2 was allocated but never
        // emitted (gap), and idx 4 references a parent (3) that was never
        // emitted (orphan). Release builds must NOT panic or corrupt — the
        // orphan is dropped, the rest of the tree is intact.
        let records = vec![
            rec(0, "root", u32::MAX, 0, true),
            rec(1, "A", 0, 0, true),
            // idx 2 missing (gap)
            rec(3, "A1", 1, 100, false),
            rec(4, "ghost", 99, 500, false), // parent 99 never existed → orphan
        ];
        let t = Tree::build(records);
        // root → A → A1, ghost dropped. root rolls up only A1's 100.
        assert_eq!(t.nodes[0].name, "root");
        assert_eq!(t.nodes[0].logical, 100, "orphan's 500 must not be counted");
        let kids = list_dir(&t, 0, SortKey::NameAsc, 0, 100, SizeMode::Logical);
        assert_eq!(
            kids.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            &["A"]
        );
        // No node named "ghost" anywhere.
        assert!(t.nodes.iter().all(|n| n.name != "ghost"));
    }

    #[test]
    fn path_is_cycle_and_bounds_safe() {
        // A well-formed tree: path() returns root→leaf.
        let t = flat();
        let leaf = t.nodes.iter().position(|n| n.name == "c.bin").unwrap() as u32;
        let p = t.path(leaf);
        assert_eq!(p.first().unwrap().1, "root");
        assert_eq!(p.last().unwrap().1, "c.bin");
        // An out-of-range idx must not panic — just yields an empty/partial path.
        let _ = t.path(99999);
    }

    #[test]
    fn empty_and_single_node_trees() {
        // No records → empty tree, no panic.
        let empty = Tree::build(vec![]);
        assert_eq!(empty.nodes.len(), 0);
        // Root-only tree (e.g. scanning an empty folder).
        let single = Tree::build(vec![rec(0, "root", u32::MAX, 0, true)]);
        assert_eq!(single.nodes.len(), 1);
        assert_eq!(
            list_dir(&single, 0, SortKey::SizeDesc, 0, 100, SizeMode::Allocated).len(),
            0
        );
        let _ = treemap_layout(&single, 0, LayoutOpts::default(), SizeMode::Allocated);
    }

    #[test]
    fn deeper_tree_no_duplicate_idxs_in_queries() {
        // Build a 3-level tree that previously triggered the DFS-preorder layout bug:
        // sibling subtrees of unequal depth made first_child + child_count slice across
        // non-siblings, so walk_subtree and list_dir visited deeper nodes multiple times.
        //
        // root/
        //   A/ (subtree: A, A1, A2, A1a)
        //     A1/  (subtree: A1, A1a)
        //       A1a (file 100)
        //     A2 (file 200)
        //   B/ (subtree: B, B1)
        //     B1 (file 300)
        //   C/ (subtree: C, C1, C2)
        //     C1 (file 400)
        //     C2 (file 500)
        let mut records = vec![rec(0, "root", u32::MAX, 0, true)];
        let a = records.len() as u32;
        records.push(rec(a, "A", 0, 0, true));
        let a1 = records.len() as u32;
        records.push(rec(a1, "A1", a, 0, true));
        let a1a = records.len() as u32;
        records.push(rec(a1a, "A1a", a1, 100, false));
        let a2 = records.len() as u32;
        records.push(rec(a2, "A2", a, 200, false));
        let b = records.len() as u32;
        records.push(rec(b, "B", 0, 0, true));
        let b1 = records.len() as u32;
        records.push(rec(b1, "B1", b, 300, false));
        let c = records.len() as u32;
        records.push(rec(c, "C", 0, 0, true));
        let c1 = records.len() as u32;
        records.push(rec(c1, "C1", c, 400, false));
        let c2 = records.len() as u32;
        records.push(rec(c2, "C2", c, 500, false));

        let t = Tree::build(records);

        // Children of root should be exactly {A, B, C} — three distinct dirs, in any order.
        let kids = list_dir(&t, 0, SortKey::NameAsc, 0, 100, SizeMode::Logical);
        let names: Vec<&str> = kids.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, &["A", "B", "C"], "root must have exactly A, B, C");

        // top_files across the whole tree should return each file exactly once.
        let top = top_files(&t, 0, 50, SizeMode::Logical);
        let top_idxs: std::collections::HashSet<u32> = top.iter().map(|r| r.idx).collect();
        assert_eq!(
            top_idxs.len(),
            top.len(),
            "top_files contained duplicate idxs"
        );
        assert_eq!(top.len(), 5, "expected 5 files (A1a, A2, B1, C1, C2)");

        // top_dirs walks every dir in the subtree, but the passthrough filter hides
        // dirs whose single child holds ≥95% of their size. So A1 (only A1a) and
        // B (only B1) collapse out; we're left with {A, C} where the dir has a real
        // mix of children.
        let dirs = top_dirs(&t, 0, 50, SizeMode::Logical);
        let dir_idxs: std::collections::HashSet<u32> = dirs.iter().map(|r| r.idx).collect();
        assert_eq!(
            dir_idxs.len(),
            dirs.len(),
            "top_dirs contained duplicate idxs"
        );
        let names: Vec<&str> = dirs.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"A"));
        assert!(names.contains(&"C"));
        assert!(
            !names.contains(&"B"),
            "B has a single child B1 — should be filtered as passthrough"
        );
        assert!(
            !names.contains(&"A1"),
            "A1 has a single child A1a — should be filtered as passthrough"
        );

        // Aggregation: root logical = 100 + 200 + 300 + 400 + 500 = 1500.
        assert_eq!(t.nodes[0].logical, 1500);
        assert_eq!(t.nodes[0].file_count, 5);
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
