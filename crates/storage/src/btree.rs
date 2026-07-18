use crate::types::{RowId, TypeId, Value};
use crc32fast::Hasher as Crc32Hasher;
use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;

const ORDER: usize = 256;

// ─── On-disk btree file format ────────────────────────────────────────────
//
// Mission 3: persist b-tree indexes to disk so `CREATE INDEX` survives a
// restart. Format is a small hand-rolled little-endian blob — we explicitly
// avoid pulling in a new serializer dep (no `serde`/`bincode`/`postcard` in
// the workspace, and the tree's node shape is simple enough to encode by
// hand).
//
// Layout:
//   magic     [4]  = "BIDX"
//   version   u16
//   root      u32
//   n_nodes   u32
//   for each node:
//     tag     u8   (0 = Internal, 1 = Leaf)
//     n_keys  u32
//     for each key: encoded Value (type tag + payload — see write_value/read_value)
//     if Internal:
//       n_children u32
//       children:  u32 * n_children
//     if Leaf:
//       for each value slot: page_id u32 + slot_index u16
//       next_leaf_present u8 (0/1)
//       if present: next_leaf u32
//
// Value encoding:
//   type_id u8 + payload. For Int/Float/DateTime: 8 bytes LE. For Bool: 1
//   byte. For Str: u32 len + UTF-8 bytes. For Uuid: 16 bytes. For Bytes:
//   u32 len + raw bytes. For Empty: no payload.
const BTREE_MAGIC: &[u8; 4] = b"BIDX";
pub const LEGACY_BTREE_VERSION: u16 = 1;
/// v2 added the `empty_rids` side list. Expression indexes are written at this
/// version; their keys are raw (never composite Str), so the v3 NUL-escaping
/// does not apply to them and they keep loading unchanged.
pub const EXPRESSION_BTREE_VERSION: u16 = 2;
/// v3 NUL-escapes Str values inside non-unique composite keys (0x00 content
/// byte -> 0x00 0xFF, terminated by 0x00 0x00). This makes each value's key
/// range disjoint even for embedded-NUL strings. A non-unique column index in
/// an older version is never served: it is rebuilt from the heap on open (see
/// `Table::open`).
pub const BTREE_VERSION: u16 = 3;
const NODE_TAG_INTERNAL: u8 = 0;
const NODE_TAG_LEAF: u8 = 1;

#[derive(Debug, Clone)]
enum Node {
    Internal {
        keys: Vec<Value>,
        children: Vec<usize>, // indices into nodes vec
    },
    Leaf {
        keys: Vec<Value>,
        values: Vec<RowId>,
        next_leaf: Option<usize>,
    },
}

/// Coarse per-index statistics used by the conjunction index chooser to rank
/// candidates by estimated rows per key. Read O(1) from in-memory counters via
/// [`BTree::stats`]; never persisted (recomputed on every load).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexStats {
    /// Non-empty keys physically stored in the tree (excludes the v2
    /// `empty_rids` side list).
    pub total_entries: u64,
    /// Distinct logical keys. For unique and expression trees this is the
    /// distinct raw-key count; for non-unique composite trees it is the
    /// distinct value-prefix count (composite key bytes minus the trailing
    /// 8-byte rid).
    pub distinct_keys: u64,
    /// Length of the v2 `empty_rids` side list (missing / JSON-null results).
    pub empty_count: u64,
}

/// How the tree's physical keys map to logical keys for distinct counting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyKind {
    /// The physical key is the logical key: unique column indexes (one entry
    /// per key) and expression indexes (duplicate raw keys repeat physically).
    /// Distinct keys are counted by raw-key transitions along the leaf chain.
    Raw,
    /// The physical key is a composite `(col_val, rid)` blob (non-unique
    /// column indexes). Distinct keys are counted by value-prefix transitions
    /// (composite bytes minus the trailing 8-byte rid).
    Composite,
}

/// Value prefix of a composite key: its bytes minus the trailing 8-byte rid.
/// Non-Bytes or too-short keys yield an empty slice (they never appear in a
/// well-formed composite tree, but the fallback keeps counting total-safe).
fn composite_prefix_bytes(key: &Value) -> &[u8] {
    match key {
        Value::Bytes(b) if b.len() >= 8 => &b[..b.len() - 8],
        _ => &[],
    }
}

/// Append a string's bytes to a composite-key buffer with order-preserving NUL
/// escaping (v3): every 0x00 content byte becomes the pair 0x00 0xFF, and the
/// string is terminated by 0x00 0x00. An escaped NUL (0x00 0xFF) can never be
/// confused with the terminator (0x00 0x00), and the terminator can never occur
/// inside the content, so the encoding is self-delimiting and prefix-free: no
/// value's encoding is a byte prefix of another's. That is exactly what keeps
/// each value's `[value|rid_min, value|rid_max]` composite-key range disjoint
/// once the raw 8-byte rid is appended, and it preserves lexicographic value
/// order (0x00 0x00 terminator < 0x00 0xFF continuation, so `"A" < "A\0"`).
/// Worst-case overhead is 2x for an all-NUL value.
fn encode_str_escaped(buf: &mut Vec<u8>, s: &[u8]) {
    for &byte in s {
        buf.push(byte);
        if byte == 0 {
            buf.push(0xFF);
        }
    }
    buf.push(0);
    buf.push(0);
}

/// In-memory B+ tree index. Keys are Values, values are RowIds.
///
/// Order 256: each node holds up to 256 keys. For 500K rows with integer keys,
/// this gives height 3 (3 node visits per lookup). For 1 billion rows: height 4.
pub struct BTree {
    nodes: Vec<Node>,
    root: usize,
    /// Backing file for on-disk persistence. Set at `create`/`load` time;
    /// `save` writes to this path atomically (write-then-rename).
    path: std::path::PathBuf,
    /// Blocker B3: has this tree been mutated in memory since the last
    /// `save_if_dirty` / `save` call? Every mutating method flips this to
    /// `true`; `save_if_dirty` skips the serialize+rename when the flag
    /// is clear, so a checkpoint that touches an untouched tree is free.
    /// Set to `false` on `create` / `load` / any successful `save_to`.
    dirty: bool,
    format_version: u16,
    /// Missing/JSON-null expression results. Kept outside the ordered key
    /// space so equality/range scans never match them; callers append this
    /// stable RID order for NULLS LAST.
    empty_rids: Vec<RowId>,
    /// Physical (non-empty) key count. Maintained exactly by every mutating
    /// method and recomputed from the leaf chain on load and at each `save`, so
    /// any drift heals at the next checkpoint or reload.
    total_entries: u64,
    /// Distinct logical key count, interpreted per `key_kind`. Maintained
    /// incrementally by every mutating method and recomputed from the leaf chain
    /// on load and at each `save`. Exact for every key shape, including
    /// embedded-NUL string values in a composite (non-unique) index: the v3
    /// escaped encoding gives each value a distinct key prefix (see
    /// `encode_str_escaped`).
    distinct_keys: u64,
    /// Distinct-counting mode. Defaults to `Raw`; the non-unique wrappers flip
    /// it to `Composite`, and `mark_composite` sets it after a load of a
    /// non-unique column index so the recount uses the value prefix.
    key_kind: KeyKind,
}

impl BTree {
    pub fn create(path: &Path) -> std::io::Result<Self> {
        let root_node = Node::Leaf {
            keys: Vec::new(),
            values: Vec::new(),
            next_leaf: None,
        };
        Ok(BTree {
            nodes: vec![root_node],
            root: 0,
            path: path.to_path_buf(),
            // Fresh trees have no on-disk content yet; the caller is
            // expected to `save` once after bulk-loading before the
            // dirty flag is meaningful.
            dirty: false,
            format_version: LEGACY_BTREE_VERSION,
            empty_rids: Vec::new(),
            total_entries: 0,
            distinct_keys: 0,
            key_kind: KeyKind::Raw,
        })
    }

    /// Per-index statistics. O(1) read of in-memory counters; safe to call per
    /// query execution inside runtime plan lowering.
    pub fn stats(&self) -> IndexStats {
        IndexStats {
            total_entries: self.total_entries,
            distinct_keys: self.distinct_keys,
            empty_count: self.empty_rids.len() as u64,
        }
    }

    /// Mark this tree as a non-unique composite index and recompute distinct
    /// keys by value prefix. Called after `load` for a non-unique column index
    /// (the on-disk format does not record the key kind, so the caller, which
    /// knows the index is non-unique, supplies it).
    pub fn mark_composite(&mut self) {
        self.key_kind = KeyKind::Composite;
        self.recount();
    }

    /// Recompute `total_entries` and `distinct_keys` from the leaf chain in the
    /// current `key_kind`. Used on load and by the `insert_duplicate` rebuild
    /// fallback, both of which already touch every key.
    fn recount(&mut self) {
        let mut total: u64 = 0;
        let mut distinct: u64 = 0;
        let mut prev_raw: Option<Value> = None;
        let mut prev_prefix: Option<Vec<u8>> = None;
        let mut node_id = self.root;
        while let Node::Internal { children, .. } = &self.nodes[node_id] {
            node_id = children[0];
        }
        let mut current = Some(node_id);
        while let Some(id) = current {
            let Node::Leaf {
                keys, next_leaf, ..
            } = &self.nodes[id]
            else {
                break;
            };
            for key in keys {
                total += 1;
                match self.key_kind {
                    KeyKind::Raw => {
                        if prev_raw.as_ref() != Some(key) {
                            distinct += 1;
                            prev_raw = Some(key.clone());
                        }
                    }
                    KeyKind::Composite => {
                        let prefix = composite_prefix_bytes(key);
                        if prev_prefix.as_deref() != Some(prefix) {
                            distinct += 1;
                            prev_prefix = Some(prefix.to_vec());
                        }
                    }
                }
            }
            current = *next_leaf;
        }
        self.total_entries = total;
        self.distinct_keys = distinct;
    }

    /// Whether any physical key shares the value prefix of `composite`
    /// (composite bytes minus the trailing 8-byte rid). O(tree height): drives
    /// the Composite-mode distinct maintenance (a new prefix on insert, the
    /// last one gone on delete). Keys sharing a prefix are contiguous in sort
    /// order, so inspecting the first key at or after the prefix suffices.
    fn composite_prefix_present(&self, composite: &Value) -> bool {
        let Value::Bytes(bytes) = composite else {
            return false;
        };
        if bytes.len() < 8 {
            return false;
        }
        let target_prefix = &bytes[..bytes.len() - 8];
        let mut start_bytes = Vec::with_capacity(bytes.len());
        start_bytes.extend_from_slice(target_prefix);
        start_bytes.extend_from_slice(&0u64.to_be_bytes());
        let start = Value::Bytes(start_bytes);

        let mut node_id = self.root;
        while let Node::Internal { keys, children } = &self.nodes[node_id] {
            node_id = children[keys.partition_point(|separator| separator < &start)];
        }
        let mut current = Some(node_id);
        while let Some(id) = current {
            let Node::Leaf {
                keys, next_leaf, ..
            } = &self.nodes[id]
            else {
                return false;
            };
            let pos = keys.partition_point(|k| k < &start);
            if pos < keys.len() {
                return composite_prefix_bytes(&keys[pos]) == target_prefix;
            }
            current = *next_leaf;
        }
        false
    }

    /// Whether any physical entry stores the raw `key`. Unlike [`BTree::lookup`]
    /// (rightmost descent to a single candidate leaf), this is duplicate-aware:
    /// equal raw keys may span several leaves above the leaf order, so it does a
    /// leftmost descent and walks the leaf chain forward to the first key at or
    /// after `key`, exactly as [`BTree::composite_prefix_present`] does. Cost is
    /// O(tree height) plus the bounded walk across the boundary leaves that hold
    /// `key`. Drives Raw-mode distinct maintenance in `insert_duplicate` and
    /// `delete_pair`.
    fn raw_key_present(&self, key: &Value) -> bool {
        let mut node_id = self.root;
        while let Node::Internal { keys, children } = &self.nodes[node_id] {
            node_id = children[keys.partition_point(|separator| separator < key)];
        }
        let mut current = Some(node_id);
        while let Some(id) = current {
            let Node::Leaf {
                keys, next_leaf, ..
            } = &self.nodes[id]
            else {
                return false;
            };
            let pos = keys.partition_point(|stored| stored < key);
            if pos < keys.len() {
                return &keys[pos] == key;
            }
            current = *next_leaf;
        }
        false
    }

    /// Create the v2 expression-index shape. Unique column indexes continue
    /// using [`BTree::create`] and remain v1; non-unique column indexes use
    /// [`BTree::create_non_unique`] (v3).
    pub fn create_v2(path: &Path) -> std::io::Result<Self> {
        let mut tree = Self::create(path)?;
        tree.format_version = EXPRESSION_BTREE_VERSION;
        Ok(tree)
    }

    /// Create a v3 non-unique column index: composite keys with NUL-escaped Str
    /// values (see [`BTREE_VERSION`]). Sets `key_kind` to `Composite` up front
    /// so a tree that never receives an insert still persists at v3 and is not
    /// re-migrated on the next open.
    pub fn create_non_unique(path: &Path) -> std::io::Result<Self> {
        let mut tree = Self::create(path)?;
        tree.format_version = BTREE_VERSION;
        tree.key_kind = KeyKind::Composite;
        Ok(tree)
    }

    pub fn format_version(&self) -> u16 {
        self.format_version
    }

    pub fn insert_empty(&mut self, rid: RowId) {
        self.dirty = true;
        let pair = (rid.page_id, rid.slot_index);
        let position = self
            .empty_rids
            .partition_point(|existing| (existing.page_id, existing.slot_index) < pair);
        if self.empty_rids.get(position) != Some(&rid) {
            self.empty_rids.insert(position, rid);
        }
    }

    pub fn delete_empty(&mut self, rid: RowId) -> bool {
        self.dirty = true;
        let pair = (rid.page_id, rid.slot_index);
        let position = self
            .empty_rids
            .partition_point(|existing| (existing.page_id, existing.slot_index) < pair);
        if self.empty_rids.get(position) == Some(&rid) {
            self.empty_rids.remove(position);
            true
        } else {
            false
        }
    }

    pub fn empty_rids(&self) -> &[RowId] {
        &self.empty_rids
    }

    pub fn insert(&mut self, key: Value, rid: RowId) {
        // Blocker B3: any mutation flips the dirty flag. Callers
        // (Table::insert, Table::update_hinted, ...) defer the actual
        // disk write to the next `save_if_dirty` — typically at
        // `Catalog::checkpoint` or on `Drop`.
        self.dirty = true;
        // Statistics maintenance. In Composite mode the value prefix must be
        // probed before the recursion mutates the tree; new-vs-replace comes
        // from the recursion itself (the replace branch reports it), which
        // saves a second descent on the insert hot path.
        let prefix_present = match self.key_kind {
            KeyKind::Raw => false,
            KeyKind::Composite => self.composite_prefix_present(&key),
        };
        let mut replaced = false;
        let root = self.root;
        if let Some((mid_key, new_node_id)) =
            self.insert_recursive(root, key, rid, true, &mut replaced)
        {
            // Root was split — create new root
            let new_root = Node::Internal {
                keys: vec![mid_key],
                children: vec![self.root, new_node_id],
            };
            let new_root_id = self.nodes.len();
            self.nodes.push(new_root);
            self.root = new_root_id;
        }
        if !replaced {
            self.total_entries += 1;
        }
        // Raw mode: a non-replacing insert is a new distinct key. Composite
        // mode: a replace implies the (value, rid) pair existed, so the prefix
        // was present; distinct only advances when the prefix was absent.
        let distinct_added = match self.key_kind {
            KeyKind::Raw => !replaced,
            KeyKind::Composite => !prefix_present,
        };
        if distinct_added {
            self.distinct_keys += 1;
        }
    }

    /// Insert a raw key/value pair without replacing an existing equal key.
    /// Equal keys may span multiple leaves; lookup and pair deletion use a
    /// left-biased descent and leaf-chain walk so split boundaries are
    /// transparent.
    pub fn insert_duplicate(&mut self, key: Value, rid: RowId) {
        if key.is_empty() {
            self.insert_empty(rid);
            return;
        }
        let rid_key = (rid.page_id, rid.slot_index);
        let last_rid = self.last_rid_for_raw_key(&key);
        if last_rid == Some(rid) {
            return;
        }
        if last_rid.is_some_and(|last| (last.page_id, last.slot_index) > rid_key) {
            let existing = self.lookup_all(&key);
            if existing.contains(&rid) {
                return;
            }
            let mut pairs = self.ordered_pairs();
            pairs.push((key, rid));
            pairs.sort_by(|(left_key, left_rid), (right_key, right_rid)| {
                left_key.cmp(right_key).then_with(|| {
                    (left_rid.page_id, left_rid.slot_index)
                        .cmp(&(right_rid.page_id, right_rid.slot_index))
                })
            });
            self.nodes = vec![Node::Leaf {
                keys: Vec::new(),
                values: Vec::new(),
                next_leaf: None,
            }];
            self.root = 0;
            for (stored_key, stored_rid) in pairs {
                let root = self.root;
                let mut rebuilt_replaced = false;
                if let Some((mid_key, new_node_id)) = self.insert_recursive(
                    root,
                    stored_key,
                    stored_rid,
                    false,
                    &mut rebuilt_replaced,
                ) {
                    let new_root_id = self.nodes.len();
                    self.nodes.push(Node::Internal {
                        keys: vec![mid_key],
                        children: vec![self.root, new_node_id],
                    });
                    self.root = new_root_id;
                }
            }
            self.dirty = true;
            // Whole-tree rebuild fallback: the incremental new-vs-replace
            // signal is not available here, so recompute both counters from
            // the freshly built leaf chain rather than guess.
            self.recount();
            return;
        }
        self.dirty = true;
        // Normal duplicate insert always adds a physical entry. The raw key is a
        // new distinct key only when no copy existed before. Duplicates can span
        // leaves, so use the duplicate-aware probe: `last_rid` reflects only the
        // rightmost candidate leaf and would miss copies held in a lower leaf.
        let key_was_present = self.raw_key_present(&key);
        let mut duplicate_replaced = false;
        let root = self.root;
        if let Some((mid_key, new_node_id)) =
            self.insert_recursive(root, key, rid, false, &mut duplicate_replaced)
        {
            let new_root = Node::Internal {
                keys: vec![mid_key],
                children: vec![self.root, new_node_id],
            };
            let new_root_id = self.nodes.len();
            self.nodes.push(new_root);
            self.root = new_root_id;
        }
        self.total_entries += 1;
        if !key_was_present {
            self.distinct_keys += 1;
        }
    }

    fn last_rid_for_raw_key(&self, key: &Value) -> Option<RowId> {
        let mut node_id = self.root;
        while let Node::Internal { keys, children } = &self.nodes[node_id] {
            node_id = children[keys.partition_point(|separator| separator <= key)];
        }
        let Node::Leaf { keys, values, .. } = &self.nodes[node_id] else {
            unreachable!("B+tree descent ended at an internal node");
        };
        let position = keys.partition_point(|stored| stored <= key);
        position
            .checked_sub(1)
            .filter(|position| keys[*position] == *key)
            .map(|position| values[position])
    }

    /// Mission C Phase 15: specialised int-keyed insert.
    ///
    /// Same rationale as `lookup_int` / `delete_int`: every comparison in
    /// the generic path runs through `<Value as Ord>::cmp`, which matches
    /// on both sides' discriminants before forwarding to `i64::cmp`. On
    /// `insert_batch_1k` (1000 rows, one index on `id`, ~8 comparisons
    /// per descent) that's enough dispatch traffic to show up in the
    /// bench. This path takes the key as a raw `i64`, does single-sided
    /// discriminant matching in the binary-search loop, and stores the
    /// key as `Value::Int(i64)` so the on-disk representation stays
    /// compatible with the generic `insert` / `lookup` paths.
    #[inline]
    pub fn insert_int(&mut self, key: i64, rid: RowId) {
        // Blocker B3: mark dirty; checkpoint flushes later.
        self.dirty = true;
        // Int fast path is only used by unique int column indexes (Raw mode,
        // one entry per key), so a new key advances both counters by one.
        // New-vs-replace is reported by the recursion's replace branch; no
        // separate probe descent.
        let mut replaced = false;
        let root = self.root;
        if let Some((mid_key, new_node_id)) =
            self.insert_recursive_int(root, key, rid, &mut replaced)
        {
            let new_root = Node::Internal {
                keys: vec![Value::Int(mid_key)],
                children: vec![self.root, new_node_id],
            };
            let new_root_id = self.nodes.len();
            self.nodes.push(new_root);
            self.root = new_root_id;
        }
        if !replaced {
            self.total_entries += 1;
            self.distinct_keys += 1;
        }
    }

    fn insert_recursive_int(
        &mut self,
        node_id: usize,
        key: i64,
        rid: RowId,
        replaced: &mut bool,
    ) -> Option<(i64, usize)> {
        match &mut self.nodes[node_id] {
            Node::Leaf { keys, values, .. } => {
                // Single-sided i64 comparison: since the leaf is all
                // Value::Int (invariant of an int index), LLVM collapses
                // this to straight `i64 < key` after inlining.
                let pos = keys.partition_point(|k| match k {
                    Value::Int(i) => *i < key,
                    _ => false,
                });

                // Duplicate key — overwrite rid in place.
                if pos < keys.len() {
                    if let Value::Int(existing) = &keys[pos] {
                        if *existing == key {
                            values[pos] = rid;
                            *replaced = true;
                            return None;
                        }
                    }
                }

                keys.insert(pos, Value::Int(key));
                values.insert(pos, rid);

                if keys.len() <= ORDER {
                    return None;
                }

                // Overflow — split (same shape as `insert_recursive`).
                let mid = keys.len() / 2;
                let right_keys = keys.split_off(mid);
                let right_values = values.split_off(mid);
                let mid_key = match &right_keys[0] {
                    Value::Int(i) => *i,
                    _ => unreachable!("int-keyed btree held non-int key"),
                };
                let captured_next_leaf = match &self.nodes[node_id] {
                    Node::Leaf { next_leaf, .. } => *next_leaf,
                    _ => unreachable!(),
                };

                let right_id = self.nodes.len();
                self.nodes.push(Node::Leaf {
                    keys: right_keys,
                    values: right_values,
                    next_leaf: captured_next_leaf,
                });
                if let Node::Leaf { next_leaf, .. } = &mut self.nodes[node_id] {
                    *next_leaf = Some(right_id);
                }

                Some((mid_key, right_id))
            }
            Node::Internal { keys, children } => {
                // First child whose separator key is strictly greater than
                // `key`. Same single-sided i64 match as `lookup_int`.
                let pos = keys.partition_point(|k| match k {
                    Value::Int(i) => *i <= key,
                    _ => false,
                });
                let child_id = children[pos];
                // Drop the borrow on self.nodes[node_id] before recursing.

                let (mid_key, new_child_id) =
                    self.insert_recursive_int(child_id, key, rid, replaced)?;

                // Re-borrow to insert the promoted key; possibly split.
                let split_payload = match &mut self.nodes[node_id] {
                    Node::Internal { keys, children } => {
                        keys.insert(pos, Value::Int(mid_key));
                        children.insert(pos + 1, new_child_id);
                        if keys.len() <= ORDER {
                            None
                        } else {
                            let mid = keys.len() / 2;
                            let promote_key = match &keys[mid] {
                                Value::Int(i) => *i,
                                _ => unreachable!("int-keyed internal held non-int key"),
                            };
                            let right_keys: Vec<Value> = keys.drain(mid + 1..).collect();
                            keys.truncate(mid);
                            let right_children: Vec<usize> = children.drain(mid + 1..).collect();
                            Some((promote_key, right_keys, right_children))
                        }
                    }
                    _ => unreachable!(),
                };

                if let Some((promote_key, right_keys, right_children)) = split_payload {
                    let right_id = self.nodes.len();
                    self.nodes.push(Node::Internal {
                        keys: right_keys,
                        children: right_children,
                    });
                    Some((promote_key, right_id))
                } else {
                    None
                }
            }
        }
    }

    fn insert_recursive(
        &mut self,
        node_id: usize,
        key: Value,
        rid: RowId,
        replace_duplicate: bool,
        replaced: &mut bool,
    ) -> Option<(Value, usize)> {
        // Mission C Phase 6: in-place insert.
        //
        // The previous implementation did `let node = self.nodes[node_id].clone();`
        // at the top of every recursive call. For the common int-keyed leaf
        // that's a Vec<Value> of up to 256 entries + a Vec<RowId> of the same
        // length — roughly 4-6 KB of memcpy per insert recursion. With a
        // height-3 tree that's 12-18 KB of allocator + memcpy traffic on
        // every insert, which on a 100K-row bench loop dominates the whole
        // operation.
        //
        // The rewrite below does three things:
        //   1. **Hot path (leaf, no split):** a single `&mut self.nodes[node_id]`
        //      match, binary search, `Vec::insert` — zero clones.
        //   2. **Leaf split:** still in place; the only allocation is the
        //      new right-leaf Node we push onto `self.nodes`.
        //   3. **Internal descend:** reads `pos` and `child_id` under a
        //      short borrow, drops the borrow, recurses, then re-borrows to
        //      insert the promoted key. No node-level clone anywhere.
        match &mut self.nodes[node_id] {
            Node::Leaf { keys, values, .. } => {
                let mut pos = if replace_duplicate {
                    keys.partition_point(|k| k < &key)
                } else {
                    keys.partition_point(|k| k < &key)
                };

                if !replace_duplicate {
                    while pos < keys.len()
                        && keys[pos] == key
                        && (values[pos].page_id, values[pos].slot_index)
                            < (rid.page_id, rid.slot_index)
                    {
                        pos += 1;
                    }
                }

                // Duplicate key — update in place.
                if replace_duplicate && pos < keys.len() && keys[pos] == key {
                    values[pos] = rid;
                    *replaced = true;
                    return None;
                }

                keys.insert(pos, key);
                values.insert(pos, rid);

                if keys.len() <= ORDER {
                    return None;
                }

                // Overflow — split. Do the split work while we still hold
                // the borrow on the current leaf, capture the right-half
                // buffers + mid_key, drop the borrow, then push the new
                // leaf onto `self.nodes` and fix up the left leaf's
                // `next_leaf` pointer.
                let mid = keys.len() / 2;
                let right_keys = keys.split_off(mid);
                let right_values = values.split_off(mid);
                let mid_key = right_keys[0].clone();
                // The borrow on self.nodes[node_id] ends here.
                let captured_next_leaf = match &self.nodes[node_id] {
                    Node::Leaf { next_leaf, .. } => *next_leaf,
                    _ => unreachable!(),
                };

                let right_id = self.nodes.len();
                self.nodes.push(Node::Leaf {
                    keys: right_keys,
                    values: right_values,
                    next_leaf: captured_next_leaf,
                });
                if let Node::Leaf { next_leaf, .. } = &mut self.nodes[node_id] {
                    *next_leaf = Some(right_id);
                }

                Some((mid_key, right_id))
            }
            Node::Internal { keys, children } => {
                // Pick the child whose separator is strictly greater than
                // `key`. We only need `pos` and `child_id` — drop the borrow
                // before recursing.
                let pos = keys.partition_point(|k| k <= &key);
                let child_id = children[pos];
                // Borrow on self.nodes[node_id] ends here.

                let (mid_key, new_child_id) =
                    self.insert_recursive(child_id, key, rid, replace_duplicate, replaced)?;

                // Re-borrow to insert the promoted key; possibly split this
                // internal node. All work that needs the borrow happens
                // inside the match arm; `self.nodes.push` for the split
                // right-half runs after the borrow drops.
                let split_payload = match &mut self.nodes[node_id] {
                    Node::Internal { keys, children } => {
                        keys.insert(pos, mid_key);
                        children.insert(pos + 1, new_child_id);
                        if keys.len() <= ORDER {
                            None
                        } else {
                            let mid = keys.len() / 2;
                            let promote_key = keys[mid].clone();
                            let right_keys: Vec<Value> = keys.drain(mid + 1..).collect();
                            keys.truncate(mid);
                            let right_children: Vec<usize> = children.drain(mid + 1..).collect();
                            Some((promote_key, right_keys, right_children))
                        }
                    }
                    _ => unreachable!(),
                };

                if let Some((promote_key, right_keys, right_children)) = split_payload {
                    let right_id = self.nodes.len();
                    self.nodes.push(Node::Internal {
                        keys: right_keys,
                        children: right_children,
                    });
                    Some((promote_key, right_id))
                } else {
                    None
                }
            }
        }
    }

    /// Point lookup: find the RowId for a given key.
    ///
    /// Mission D1: binary search instead of linear scan. With ORDER=256 nodes,
    /// linear scan was ~128 comparisons average; binary search is ~8. The
    /// `Value` Ord impl is total, so `binary_search` is sound. Mission F's
    /// `#[inline]` is preserved so LTO can still fold this into the index-
    /// lookup fast path.
    #[inline]
    pub fn lookup(&self, key: &Value) -> Option<RowId> {
        let mut node_id = self.root;
        loop {
            match &self.nodes[node_id] {
                Node::Leaf { keys, values, .. } => {
                    return match keys.binary_search(key) {
                        Ok(i) => Some(values[i]),
                        Err(_) => None,
                    };
                }
                Node::Internal { keys, children } => {
                    // First child whose separator is strictly greater than `key`.
                    // `partition_point(p)` returns the first index where `p` is
                    // false, so `|k| k <= key` finds the first `k > key`.
                    let pos = keys.partition_point(|k| k <= key);
                    node_id = children[pos];
                }
            }
        }
    }

    /// Return every RowId stored under an equal raw key. Unlike the legacy
    /// non-unique composite-key helper, the key remains its original `Value`
    /// (including `Empty`).
    pub fn lookup_all(&self, key: &Value) -> Vec<RowId> {
        if key.is_empty() {
            return Vec::new();
        }
        let mut node_id = self.root;
        while let Node::Internal { keys, children } = &self.nodes[node_id] {
            node_id = children[keys.partition_point(|separator| separator < key)];
        }

        let mut matches = Vec::new();
        let mut current = Some(node_id);
        while let Some(id) = current {
            let Node::Leaf {
                keys,
                values,
                next_leaf,
            } = &self.nodes[id]
            else {
                unreachable!("leaf chain pointed at an internal node");
            };
            for (stored_key, rid) in keys.iter().zip(values) {
                match stored_key.cmp(key) {
                    std::cmp::Ordering::Less => {}
                    std::cmp::Ordering::Equal => matches.push(*rid),
                    std::cmp::Ordering::Greater => return matches,
                }
            }
            current = *next_leaf;
        }
        matches
    }

    /// Delete exactly one raw `(key, RowId)` pair while preserving other
    /// duplicate entries.
    pub fn delete_pair(&mut self, key: &Value, rid: RowId) -> bool {
        if key.is_empty() {
            return self.delete_empty(rid);
        }
        self.dirty = true;
        let mut node_id = self.root;
        while let Node::Internal { keys, children } = &self.nodes[node_id] {
            node_id = children[keys.partition_point(|separator| separator < key)];
        }

        let mut current = Some(node_id);
        while let Some(id) = current {
            let (found, next, passed_key) = match &self.nodes[id] {
                Node::Leaf {
                    keys,
                    values,
                    next_leaf,
                } => (
                    keys.iter()
                        .zip(values)
                        .position(|(stored_key, stored_rid)| {
                            stored_key == key && *stored_rid == rid
                        }),
                    *next_leaf,
                    keys.last().is_some_and(|stored_key| stored_key > key),
                ),
                Node::Internal { .. } => unreachable!("leaf chain pointed at an internal node"),
            };
            if let Some(position) = found {
                if let Node::Leaf { keys, values, .. } = &mut self.nodes[id] {
                    keys.remove(position);
                    values.remove(position);
                }
                self.total_entries = self.total_entries.saturating_sub(1);
                // Raw duplicate tree (expression index): the raw key stays a
                // distinct key while any other rid under it remains. The
                // remaining copies can live in a different leaf, so probe with
                // the duplicate-aware `raw_key_present` rather than the
                // single-leaf `lookup`, which would misreport the key as gone
                // once the boundary leaf's copies are the ones removed.
                if !self.raw_key_present(key) {
                    self.distinct_keys = self.distinct_keys.saturating_sub(1);
                }
                return true;
            }
            if passed_key {
                return false;
            }
            current = next;
        }
        false
    }

    /// Snapshot every raw `(Value, RowId)` pair in B+tree key order.
    pub fn ordered_pairs(&self) -> Vec<(Value, RowId)> {
        let mut node_id = self.root;
        while let Node::Internal { children, .. } = &self.nodes[node_id] {
            node_id = children[0];
        }
        let mut pairs = Vec::with_capacity(self.len());
        let mut current = Some(node_id);
        while let Some(id) = current {
            let Node::Leaf {
                keys,
                values,
                next_leaf,
            } = &self.nodes[id]
            else {
                unreachable!("leaf chain pointed at an internal node");
            };
            pairs.extend(keys.iter().cloned().zip(values.iter().copied()));
            current = *next_leaf;
        }
        pairs.sort_by(|(left_key, left_rid), (right_key, right_rid)| {
            left_key.cmp(right_key).then_with(|| {
                (left_rid.page_id, left_rid.slot_index)
                    .cmp(&(right_rid.page_id, right_rid.slot_index))
            })
        });
        pairs
    }

    /// Ordered raw pairs followed by missing/JSON-null RIDs. This is the
    /// expression-index ORDER BY contract: ordinary typed values use the
    /// B+tree order while the side set is appended as NULLS LAST.
    pub fn ordered_pairs_nulls_last(&self) -> Vec<(Value, RowId)> {
        let mut pairs = self.ordered_pairs();
        pairs.extend(
            self.empty_rids
                .iter()
                .copied()
                .map(|rid| (Value::Empty, rid)),
        );
        pairs
    }

    /// Inclusive range over raw typed keys. Empty/null side-set entries are
    /// deliberately excluded from range semantics.
    pub fn raw_range_rids(&self, start: Option<&Value>, end: Option<&Value>) -> Vec<RowId> {
        self.ordered_pairs()
            .into_iter()
            .filter(|(key, _)| match start {
                Some(bound) => key >= bound,
                None => true,
            })
            .filter(|(key, _)| match end {
                Some(bound) => key <= bound,
                None => true,
            })
            .map(|(_, rid)| rid)
            .collect()
    }

    pub fn ordered_rids_nulls_last(&self) -> Vec<RowId> {
        self.ordered_pairs_nulls_last()
            .into_iter()
            .map(|(_, rid)| rid)
            .collect()
    }

    pub fn bounded_ordered_rids_nulls_last(
        &self,
        descending: bool,
        offset: usize,
        limit: usize,
    ) -> Vec<RowId> {
        self.bounded_ordered_rids_with_stats(descending, offset, limit)
            .0
    }

    fn bounded_ordered_rids_with_stats(
        &self,
        descending: bool,
        offset: usize,
        limit: usize,
    ) -> (Vec<RowId>, usize, usize) {
        if limit == 0 {
            return (Vec::new(), 0, 0);
        }
        let mut remaining_offset = offset;
        // LIMIT is user-controlled. Cap eager allocation while allowing the
        // result to grow naturally for a genuinely large successful query.
        let mut result = Vec::with_capacity(limit.min(4_096));
        let mut visited_scalars = 0;
        let mut peak_reverse_group = 0;
        if descending {
            let mut reverse_group = ReverseKeyGroup::default();
            if self.visit_reverse_bounded(
                self.root,
                &mut remaining_offset,
                limit,
                &mut result,
                &mut visited_scalars,
                &mut reverse_group,
            ) {
                return (result, visited_scalars, reverse_group.peak);
            }
            if flush_reverse_key_group(
                &mut reverse_group.rids,
                &mut remaining_offset,
                limit,
                &mut result,
            ) {
                return (result, visited_scalars, reverse_group.peak);
            }
            peak_reverse_group = reverse_group.peak;
        } else {
            let mut node_id = self.root;
            while let Node::Internal { children, .. } = &self.nodes[node_id] {
                node_id = children[0];
            }
            let mut current = Some(node_id);
            while let Some(id) = current {
                let Node::Leaf {
                    values, next_leaf, ..
                } = &self.nodes[id]
                else {
                    unreachable!("leaf chain pointed at an internal node");
                };
                for rid in values {
                    visited_scalars += 1;
                    if remaining_offset > 0 {
                        remaining_offset -= 1;
                    } else {
                        result.push(*rid);
                        if result.len() == limit {
                            return (result, visited_scalars, peak_reverse_group);
                        }
                    }
                }
                current = *next_leaf;
            }
        }

        for rid in &self.empty_rids {
            if remaining_offset > 0 {
                remaining_offset -= 1;
            } else {
                result.push(*rid);
                if result.len() == limit {
                    break;
                }
            }
        }
        (result, visited_scalars, peak_reverse_group)
    }

    fn visit_reverse_bounded(
        &self,
        node_id: usize,
        remaining_offset: &mut usize,
        limit: usize,
        result: &mut Vec<RowId>,
        visited_scalars: &mut usize,
        reverse_group: &mut ReverseKeyGroup,
    ) -> bool {
        match &self.nodes[node_id] {
            Node::Internal { children, .. } => {
                for child in children.iter().rev() {
                    if self.visit_reverse_bounded(
                        *child,
                        remaining_offset,
                        limit,
                        result,
                        visited_scalars,
                        reverse_group,
                    ) {
                        return true;
                    }
                }
                false
            }
            Node::Leaf { keys, values, .. } => {
                for (key, rid) in keys.iter().zip(values).rev() {
                    *visited_scalars += 1;
                    if reverse_group
                        .current_key
                        .as_ref()
                        .is_some_and(|current| current != key)
                    {
                        if flush_reverse_key_group(
                            &mut reverse_group.rids,
                            remaining_offset,
                            limit,
                            result,
                        ) {
                            return true;
                        }
                        reverse_group.current_key = Some(key.clone());
                    } else if reverse_group.current_key.is_none() {
                        reverse_group.current_key = Some(key.clone());
                    }
                    // Reverse traversal sees the largest RID first. Only the
                    // smallest `offset + remaining limit` RIDs can contribute
                    // to this query window, so discard older/larger entries
                    // instead of buffering an unbounded duplicate-key group.
                    let group_window =
                        (*remaining_offset).saturating_add(limit.saturating_sub(result.len()));
                    if reverse_group.rids.len() == group_window {
                        let _ = reverse_group.rids.pop_front();
                    }
                    reverse_group.rids.push_back(*rid);
                    reverse_group.peak = reverse_group.peak.max(reverse_group.rids.len());
                    debug_assert!(reverse_group.rids.len() <= group_window);
                }
                false
            }
        }
    }

    /// Mission D7: specialized int-keyed point lookup.
    ///
    /// For an int-keyed index (the overwhelming common case — primary keys,
    /// foreign keys, `created_at` timestamps), every comparison inside
    /// `lookup` goes through `<Value as Ord>::cmp`, which matches on the
    /// discriminant of **both** sides before forwarding to `i64::cmp`. Even
    /// with `#[inline]` that's 5-10ns of pure dispatch per comparison. With
    /// binary search on an order-256 B+tree of ~100K rows we do ~24
    /// comparisons per lookup — that's 120-240ns of overhead on top of the
    /// actual work. On the 124ns `point_lookup_indexed` measurement that's
    /// essentially all the cost.
    ///
    /// This fast path:
    ///   1. Takes the key as a raw `i64` (no `Value::Int` allocation).
    ///   2. At every comparison, extracts the stored `i64` directly via a
    ///      single-sided match, cutting out half of the dispatch.
    ///   3. Uses `debug_unreachable!`-style fallback for non-int keys — the
    ///      caller is expected to only call this on an int-keyed index.
    ///
    /// Callers that are unsure of the index type should use `lookup` instead;
    /// the old path remains correct for every type.
    #[inline]
    pub fn lookup_int(&self, key: i64) -> Option<RowId> {
        let mut node_id = self.root;
        loop {
            match &self.nodes[node_id] {
                Node::Leaf { keys, values, .. } => {
                    // Binary search with single-sided discriminant match.
                    // On a well-typed int index this compiles down to a
                    // straight `i64::cmp` loop because LLVM speculates the
                    // match arm.
                    let result = keys.binary_search_by(|k| match k {
                        Value::Int(i) => i.cmp(&key),
                        _ => std::cmp::Ordering::Less,
                    });
                    return match result {
                        Ok(i) => Some(values[i]),
                        Err(_) => None,
                    };
                }
                Node::Internal { keys, children } => {
                    // First child whose separator is strictly greater than `key`.
                    let pos = keys.partition_point(|k| match k {
                        Value::Int(i) => *i <= key,
                        _ => false,
                    });
                    node_id = children[pos];
                }
            }
        }
    }

    /// Mission C Phase 11: specialised int-keyed delete.
    ///
    /// Same rationale as `lookup_int`: the generic `delete` path runs every
    /// key comparison through `<Value as Ord>::cmp`, which matches on the
    /// discriminant of **both** sides before forwarding to `i64::cmp`. On
    /// `delete_by_filter` (~3300 rows per iteration × 3 iterations × ~12
    /// comparisons per descent = ~120K dispatch-heavy comparisons) that's a
    /// measurable fraction of the total. This fast path takes the key as a
    /// raw `i64` and uses single-sided discriminant matching so LLVM can
    /// compile the binary-search loop down to a straight `i64::cmp`.
    ///
    /// Returns `true` if the key was found and removed.
    #[inline]
    pub fn delete_int(&mut self, key: i64) -> bool {
        // Blocker B3: we mark dirty optimistically even if the key
        // turns out to be missing — the cost of re-checking is higher
        // than the cost of one no-op save.
        self.dirty = true;
        let mut node_id = self.root;
        loop {
            // Walk internal nodes via single-sided comparison.
            match &self.nodes[node_id] {
                Node::Internal { keys, children } => {
                    let pos = keys.partition_point(|k| match k {
                        Value::Int(i) => *i <= key,
                        _ => false,
                    });
                    node_id = children[pos];
                    continue;
                }
                Node::Leaf { .. } => {}
            }
            // Leaf: binary search then remove.
            if let Node::Leaf { keys, values, .. } = &mut self.nodes[node_id] {
                let result = keys.binary_search_by(|k| match k {
                    Value::Int(i) => i.cmp(&key),
                    _ => std::cmp::Ordering::Less,
                });
                if let Ok(pos) = result {
                    keys.remove(pos);
                    values.remove(pos);
                    // Unique int column index (Raw, one entry per key).
                    self.total_entries = self.total_entries.saturating_sub(1);
                    self.distinct_keys = self.distinct_keys.saturating_sub(1);
                    return true;
                }
                return false;
            }
            unreachable!();
        }
    }

    /// Mission C Phase 12: batch-delete many int keys in a single tree walk.
    ///
    /// Given a **sorted ascending** list of int keys to remove, walks the
    /// leaf chain in order and compacts each affected leaf in a single pass.
    ///
    /// For a bulk delete of ~20% of rows, this replaces ~20K individual
    /// `Vec::remove` operations (each O(n) memmove of up to 4KB of `Value`
    /// entries) with a single compact per affected leaf (one pass of
    /// swap-and-truncate). On a 100K-row `delete_by_filter` bench that
    /// collapses ~80MB of pure memmove work down to ~3MB — the difference
    /// between losing to SQLite and winning.
    ///
    /// Keys not present in the tree are silently skipped. Returns the
    /// number of keys actually removed.
    ///
    /// Caller contract: `sorted_keys` must be sorted ascending. Duplicates
    /// are tolerated (the first removes, subsequent see nothing to remove).
    pub fn delete_many_int(&mut self, sorted_keys: &[i64]) -> usize {
        if sorted_keys.is_empty() {
            return 0;
        }
        // Blocker B3: mark dirty; non-empty input means we will try
        // at least one leaf compaction, which may or may not remove
        // anything — still cheaper than a counted "only on match" flip.
        self.dirty = true;

        // Walk to the leftmost leaf. From there we can follow `next_leaf`
        // to visit every leaf in order — matching the sorted-key cursor.
        let mut node_id = self.root;
        while let Node::Internal { children, .. } = &self.nodes[node_id] {
            node_id = children[0];
        }

        let mut total_removed = 0usize;
        let mut key_cursor = 0usize;
        let mut current = Some(node_id);

        while let Some(nid) = current {
            // Early exit: no more keys to delete.
            if key_cursor >= sorted_keys.len() {
                break;
            }

            let next_leaf = if let Node::Leaf {
                keys,
                values,
                next_leaf,
            } = &mut self.nodes[nid]
            {
                let mut write = 0usize;
                for read in 0..keys.len() {
                    // Pull the int key out of the Value wrapper. Non-int
                    // keys shouldn't appear on an int index, but keep them
                    // defensively — they're impossible to match against an
                    // `i64` cursor anyway.
                    let k_opt = match &keys[read] {
                        Value::Int(i) => Some(*i),
                        _ => None,
                    };

                    // Advance cursor past any delete-keys smaller than the
                    // current leaf key. Those were either in a previous
                    // leaf or not present in the tree at all.
                    if let Some(k) = k_opt {
                        while key_cursor < sorted_keys.len() && sorted_keys[key_cursor] < k {
                            key_cursor += 1;
                        }
                        if key_cursor < sorted_keys.len() && sorted_keys[key_cursor] == k {
                            // Match — skip this entry from the output.
                            // Duplicates in sorted_keys still only drop one
                            // btree entry; advance cursor once, then let
                            // any further duplicates drop through to the
                            // "< k" advance on the next iteration.
                            key_cursor += 1;
                            total_removed += 1;
                            continue;
                        }
                    }

                    // Keep this entry. Move it down to the write index if
                    // we've already dropped anything.
                    if read != write {
                        keys.swap(read, write);
                        values.swap(read, write);
                    }
                    write += 1;
                }
                keys.truncate(write);
                values.truncate(write);
                *next_leaf
            } else {
                break;
            };

            current = next_leaf;
        }

        // Batch int delete is only used by unique int column indexes (Raw,
        // one entry per key), so each removal drops one distinct key too.
        self.total_entries = self.total_entries.saturating_sub(total_removed as u64);
        self.distinct_keys = self.distinct_keys.saturating_sub(total_removed as u64);
        total_removed
    }

    /// Delete a key from the tree. Returns true if the key was found and removed.
    pub fn delete(&mut self, key: &Value) -> bool {
        // Mark dirty; see `delete_int` for the optimistic marking rationale.
        self.dirty = true;
        // Deletion does not rebalance: leaves may become underfull but the tree
        // stays valid; lookups and range scans are unaffected.
        let mut node_id = self.root;
        let removed = loop {
            let is_leaf = matches!(self.nodes[node_id], Node::Leaf { .. });
            if is_leaf {
                let mut hit = false;
                if let Node::Leaf { keys, values, .. } = &mut self.nodes[node_id] {
                    // Mission D1: binary search the leaf for an exact match.
                    if let Ok(pos) = keys.binary_search(key) {
                        keys.remove(pos);
                        values.remove(pos);
                        hit = true;
                    }
                }
                break hit;
            }
            match &self.nodes[node_id] {
                Node::Internal { keys, children } => {
                    // Mission D1: binary search for child descent.
                    let pos = keys.partition_point(|k| k <= key);
                    node_id = children[pos];
                }
                _ => unreachable!(),
            }
        };
        if removed {
            self.total_entries = self.total_entries.saturating_sub(1);
            // Raw: unique column index (one entry per key) drops a distinct
            // key. Composite: only when no other rid shares the value prefix.
            let distinct_gone = match self.key_kind {
                KeyKind::Raw => true,
                KeyKind::Composite => !self.composite_prefix_present(key),
            };
            if distinct_gone {
                self.distinct_keys = self.distinct_keys.saturating_sub(1);
            }
        }
        removed
    }

    /// Range scan: returns all (key, rid) pairs where start <= key <= end.
    pub fn range<'a>(
        &'a self,
        start: &Value,
        end: &Value,
    ) -> impl Iterator<Item = (Value, RowId)> + 'a {
        // Find the leaf containing `start`
        let mut node_id = self.root;
        while let Node::Internal { keys, children } = &self.nodes[node_id] {
            // Mission D1: binary search for child descent.
            let pos = keys.partition_point(|k| k <= start);
            node_id = children[pos];
        }

        // Walk leaf chain collecting results
        let end = end.clone();
        let start = start.clone();
        let mut results = Vec::new();
        let mut current = Some(node_id);
        while let Some(nid) = current {
            match &self.nodes[nid] {
                Node::Leaf {
                    keys,
                    values,
                    next_leaf,
                } => {
                    let mut done = false;
                    for (i, k) in keys.iter().enumerate() {
                        if k > &end {
                            done = true;
                            break;
                        }
                        if k >= &start {
                            results.push((k.clone(), values[i]));
                        }
                    }
                    if done {
                        break;
                    }
                    current = *next_leaf;
                }
                _ => break,
            }
        }
        results.into_iter()
    }

    /// Range scan from `start` to the end of the tree (start <= key).
    pub fn range_from(&self, start: &Value) -> Vec<(Value, RowId)> {
        let mut node_id = self.root;
        while let Node::Internal { keys, children } = &self.nodes[node_id] {
            let pos = keys.partition_point(|k| k <= start);
            node_id = children[pos];
        }
        let start = start.clone();
        let mut results = Vec::new();
        let mut current = Some(node_id);
        while let Some(nid) = current {
            match &self.nodes[nid] {
                Node::Leaf {
                    keys,
                    values,
                    next_leaf,
                } => {
                    for (i, k) in keys.iter().enumerate() {
                        if k >= &start {
                            results.push((k.clone(), values[i]));
                        }
                    }
                    current = *next_leaf;
                }
                _ => break,
            }
        }
        results
    }

    /// Range scan from the beginning of the tree to `end` (key <= end).
    pub fn range_to(&self, end: &Value) -> Vec<(Value, RowId)> {
        // Find the leftmost leaf.
        let mut node_id = self.root;
        while let Node::Internal { children, .. } = &self.nodes[node_id] {
            node_id = children[0];
        }
        let end = end.clone();
        let mut results = Vec::new();
        let mut current = Some(node_id);
        while let Some(nid) = current {
            match &self.nodes[nid] {
                Node::Leaf {
                    keys,
                    values,
                    next_leaf,
                } => {
                    let mut done = false;
                    for (i, k) in keys.iter().enumerate() {
                        if k > &end {
                            done = true;
                            break;
                        }
                        results.push((k.clone(), values[i]));
                    }
                    if done {
                        break;
                    }
                    current = *next_leaf;
                }
                _ => break,
            }
        }
        results
    }

    // ─── Non-unique index support ─────────────────────────────────────────
    //
    // Secondary indexes on non-unique columns (e.g. `dept`) use the
    // "composite key" pattern: the stored key is `(column_value, row_id)`
    // so every entry is unique even when many rows share the same column
    // value. This is how SQLite, InnoDB, and MongoDB handle non-unique
    // indexes.
    //
    // The RowId is encoded as `Value::Int((page_id as i64) << 16 | slot_index as i64)`.
    // This gives a total order that is compatible with Value::Ord and
    // survives round-tripping through the on-disk format unchanged.

    /// Build a composite key: `[column_value, rid_as_int]`. The B+ tree
    /// treats this 2-element sequence as a single `Value::Bytes` blob
    /// whose `Ord` implementation naturally sorts by column value first,
    /// then by RowId.
    ///
    /// Encoding: type_tag(1) + column_value_bytes + rid_i64(8).
    /// We use a `Value::Bytes` wrapper so the existing `Value::Ord` gives
    /// us lexicographic byte comparison, which matches the intended
    /// (column_value, rid) sort order for same-type keys.
    ///
    /// Encode a column value into `buf` for a composite non-unique key, up to
    /// but not including the trailing 8-byte rid. Shared by
    /// [`BTree::make_composite_key`], [`BTree::make_prefix_start`], and
    /// [`BTree::make_prefix_end`] so the three encodings can never drift apart.
    ///
    /// NUL safety (v3): a `Str` value is written with order-preserving escaping
    /// (see [`encode_str_escaped`]) so that no value's encoding is a byte
    /// prefix of another's. That prefix-freeness is what makes each value's
    /// composite-key range `[value|rid_min, value|rid_max]` disjoint, even when
    /// a value contains an embedded NUL (`"A\0"`); before v3 the bare-0x00
    /// terminator let the raw rid suffix bridge `"A"` and `"A\0"`, returning
    /// wrong rows.
    ///
    /// Bytes/Json are length-prefixed (`u32` big-endian length + raw bytes),
    /// which is already self-delimiting and therefore prefix-free, so they never
    /// had the embedded-NUL ambiguity and are unchanged. (Length-prefixing is
    /// not order-preserving across differing lengths, so Bytes/Json range scans
    /// across distinct values are not ordered; equality lookup, which only needs
    /// disjoint ranges, is correct. Str keeps the escape encoding precisely
    /// because it must preserve value order for range scans.)
    ///
    /// No decoder unescapes these bytes back into a value: the only readers of a
    /// composite key are [`BTree::rid_from_composite`] (last 8 bytes, never
    /// escaped) and [`composite_prefix_bytes`] (everything but the last 8 bytes,
    /// compared byte-for-byte), both of which are escaping-agnostic.
    fn encode_composite_value(buf: &mut Vec<u8>, col_val: &Value) {
        // Type tag so equal-typed keys compare within one lane.
        buf.push(col_val.type_id() as u8);
        match col_val {
            Value::Int(i) => {
                // Big-endian with sign-flip so byte comparison matches i64 order.
                let flipped = (*i as u64) ^ (1u64 << 63);
                buf.extend_from_slice(&flipped.to_be_bytes());
            }
            Value::Float(f) => {
                let bits = f.to_bits();
                let ordered = if bits >> 63 == 1 {
                    !bits
                } else {
                    bits ^ (1u64 << 63)
                };
                buf.extend_from_slice(&ordered.to_be_bytes());
            }
            Value::Bool(b) => buf.push(if *b { 1 } else { 0 }),
            Value::Str(s) => encode_str_escaped(buf, s.as_bytes()),
            Value::DateTime(t) => {
                let flipped = (*t as u64) ^ (1u64 << 63);
                buf.extend_from_slice(&flipped.to_be_bytes());
            }
            Value::Uuid(u) => buf.extend_from_slice(u),
            Value::Bytes(b) => {
                buf.extend_from_slice(&(b.len() as u32).to_be_bytes());
                buf.extend_from_slice(b);
            }
            // Json keys are not produced by the column-index mechanism (path
            // indexes extract scalars), but encode reversibly like Bytes so no
            // key path can panic on a Json value.
            Value::Json(b) => {
                buf.extend_from_slice(&(b.len() as u32).to_be_bytes());
                buf.extend_from_slice(b);
            }
            Value::Empty => {}
        }
    }

    fn make_composite_key(col_val: &Value, rid: RowId) -> Value {
        let mut buf = Vec::with_capacity(32);
        Self::encode_composite_value(&mut buf, col_val);
        // Append RowId as big-endian u64 so it sorts correctly.
        let rid_bits = ((rid.page_id as u64) << 16) | rid.slot_index as u64;
        buf.extend_from_slice(&rid_bits.to_be_bytes());
        Value::Bytes(buf)
    }

    /// Build a prefix key for scanning: everything before the RowId suffix.
    fn make_prefix_start(col_val: &Value) -> Value {
        let mut buf = Vec::with_capacity(32);
        Self::encode_composite_value(&mut buf, col_val);
        // Append the smallest possible RowId (all zeros).
        buf.extend_from_slice(&0u64.to_be_bytes());
        Value::Bytes(buf)
    }

    /// Build the upper bound prefix for scanning: same column value but
    /// with the maximum possible RowId appended.
    fn make_prefix_end(col_val: &Value) -> Value {
        let mut buf = Vec::with_capacity(32);
        Self::encode_composite_value(&mut buf, col_val);
        // Append the largest possible RowId.
        buf.extend_from_slice(&u64::MAX.to_be_bytes());
        Value::Bytes(buf)
    }

    /// Extract the RowId from a composite key (last 8 bytes of the
    /// `Value::Bytes` blob).
    fn rid_from_composite(key: &Value) -> Option<RowId> {
        if let Value::Bytes(b) = key {
            if b.len() >= 8 {
                let rid_bytes = &b[b.len() - 8..];
                let rid_bits = u64::from_be_bytes(rid_bytes.try_into().ok()?);
                return Some(RowId {
                    page_id: (rid_bits >> 16) as u32,
                    slot_index: (rid_bits & 0xFFFF) as u16,
                });
            }
        }
        None
    }

    /// Insert into a non-unique secondary index. The key stored in the
    /// tree is `make_composite_key(col_val, rid)`, which is unique even
    /// when many rows share the same `col_val`.
    pub fn insert_non_unique(&mut self, col_val: Value, rid: RowId) {
        // A composite tree counts distinct keys by value prefix; declare the mode
        // so `insert` maintains the counters correctly.
        self.key_kind = KeyKind::Composite;
        // Composite Str keys use the v3 NUL-escaped encoding; a tree reaching
        // this path is a non-unique column index and must persist as v3 so it is
        // not treated as an old-format index (and rebuilt) on the next open.
        if self.format_version < BTREE_VERSION {
            self.format_version = BTREE_VERSION;
        }
        let composite = Self::make_composite_key(&col_val, rid);
        // The composite key is always unique, so the normal insert path
        // will never hit the "duplicate key — update in place" branch.
        self.insert(composite, rid);
    }

    /// Int-specialized variant of `insert_non_unique`. Still builds a
    /// composite key (as Bytes), so this routes through the generic
    /// `insert` — the int fast path only applies to keys that are
    /// literally `Value::Int`, not `Value::Bytes`.
    pub fn insert_non_unique_int(&mut self, col_val: i64, rid: RowId) {
        self.insert_non_unique(Value::Int(col_val), rid);
    }

    /// Look up all RowIds matching a column value in a non-unique
    /// secondary index. Does a range scan from
    /// `make_prefix_start(col_val)` to `make_prefix_end(col_val)` and
    /// extracts the RowId from each composite key.
    pub fn lookup_prefix(&self, col_val: &Value) -> Vec<RowId> {
        let start = Self::make_prefix_start(col_val);
        let end = Self::make_prefix_end(col_val);
        self.range(&start, &end)
            .filter_map(|(key, _rid_in_value)| Self::rid_from_composite(&key))
            .collect()
    }

    /// Int-specialized variant of `lookup_prefix`.
    pub fn lookup_prefix_int(&self, col_val: i64) -> Vec<RowId> {
        self.lookup_prefix(&Value::Int(col_val))
    }

    /// Range scan over a NON-unique index: return RowIds for all entries
    /// whose column value lies in [start, end] (inclusive; pass None for
    /// an unbounded side). Composite-key bounds reuse the prefix encoding:
    /// (start, RowId::MIN) .. (end, RowId::MAX). The caller is expected to
    /// recheck exclusive bounds against the decoded row.
    pub fn range_rids(&self, start: Option<&Value>, end: Option<&Value>) -> Vec<RowId> {
        let collect = |pairs: Vec<(Value, RowId)>| {
            pairs
                .into_iter()
                .filter_map(|(k, _)| Self::rid_from_composite(&k))
                .collect()
        };
        match (start, end) {
            (Some(s), Some(e)) => {
                let lo = Self::make_prefix_start(s);
                let hi = Self::make_prefix_end(e);
                self.range(&lo, &hi)
                    .filter_map(|(k, _)| Self::rid_from_composite(&k))
                    .collect()
            }
            (Some(s), None) => collect(self.range_from(&Self::make_prefix_start(s))),
            (None, Some(e)) => collect(self.range_to(&Self::make_prefix_end(e))),
            (None, None) => collect(self.range_from(&Self::make_prefix_start(&Value::Empty))),
        }
    }

    /// Delete a specific (col_val, rid) entry from a non-unique
    /// secondary index.
    pub fn delete_non_unique(&mut self, col_val: &Value, rid: RowId) -> bool {
        // Ensure `delete` counts distinct keys by value prefix.
        self.key_kind = KeyKind::Composite;
        let composite = Self::make_composite_key(col_val, rid);
        self.delete(&composite)
    }

    /// Int-specialized variant of `delete_non_unique`.
    pub fn delete_non_unique_int(&mut self, col_val: i64, rid: RowId) -> bool {
        self.delete_non_unique(&Value::Int(col_val), rid)
    }

    /// Number of entries in the tree.
    pub fn len(&self) -> usize {
        let mut count = 0;
        let mut node_id = self.root;
        // Find leftmost leaf
        while let Node::Internal { children, .. } = &self.nodes[node_id] {
            node_id = children[0];
        }
        // Walk leaf chain
        let mut current = Some(node_id);
        while let Some(nid) = current {
            match &self.nodes[nid] {
                Node::Leaf {
                    keys, next_leaf, ..
                } => {
                    count += keys.len();
                    current = *next_leaf;
                }
                _ => break,
            }
        }
        count
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Mission 3: the on-disk file this tree is backed by. Callers (Table
    /// lifecycle) use this to know where to write when they call
    /// `save`/`save_to_path`.
    pub fn file_path(&self) -> &Path {
        &self.path
    }

    /// Persist the tree to its backing file atomically. Writes to a
    /// sibling `.tmp` path then renames over the target, matching the
    /// catalog's persist strategy.
    pub fn save(&mut self) -> io::Result<()> {
        // Checkpoint-time heal: recompute both counters from the leaf chain in
        // the current `key_kind`. Incremental maintenance keeps them exact, so
        // this is a belt-and-braces invariant that caps any future drift at one
        // checkpoint interval for a long-lived process. `save_to` walks every
        // key anyway, so the extra pass is free.
        self.recount();
        let path = self.path.clone();
        self.save_to(&path)?;
        self.dirty = false;
        Ok(())
    }

    /// Blocker B3: persist the tree only if it has been mutated since
    /// the last successful `save` / `save_if_dirty`. Wired into
    /// [`crate::catalog::Catalog::checkpoint`] and its `Drop` impl so
    /// the write cost is paid at most once per checkpoint instead of
    /// once per insert/update/delete on the hot path.
    pub fn save_if_dirty(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.save()
    }

    /// Is there unflushed work in this tree? Exposed for tests + the
    /// `Table::rebuild_dirty_indexes_from_heap` recovery path.
    #[inline]
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Force the dirty flag. Used by crash-recovery paths that know
    /// the in-memory tree may lag the heap (e.g. after WAL replay).
    #[inline]
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Clear the dirty flag *without* persisting, so the next
    /// `save_if_dirty` becomes a no-op. Used by ROLLBACK: the catalog
    /// about to be dropped holds uncommitted, in-memory index mutations
    /// that must NOT reach disk via the drop-time checkpoint. The live
    /// tree is re-loaded from the (clean) on-disk `.idx` by the freshly
    /// opened replacement catalog, so discarding the flag here is exactly
    /// what reverts the rolled-back index writes.
    #[inline]
    pub fn discard_dirty(&mut self) {
        self.dirty = false;
    }

    /// Persist the tree to an arbitrary path. Primarily used by tests
    /// and by the create-index rebuild path, which wants to write the
    /// file at a location supplied by the caller. Does NOT update
    /// `self.path` or the dirty flag.
    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        let mut buf: Vec<u8> = Vec::with_capacity(64 + 32 * self.nodes.len());
        buf.extend_from_slice(BTREE_MAGIC);
        buf.extend_from_slice(&self.format_version.to_le_bytes());
        buf.extend_from_slice(&(self.root as u32).to_le_bytes());
        buf.extend_from_slice(&(self.nodes.len() as u32).to_le_bytes());
        for node in &self.nodes {
            match node {
                Node::Internal { keys, children } => {
                    buf.push(NODE_TAG_INTERNAL);
                    buf.extend_from_slice(&(keys.len() as u32).to_le_bytes());
                    for k in keys {
                        write_value(&mut buf, k);
                    }
                    buf.extend_from_slice(&(children.len() as u32).to_le_bytes());
                    for &c in children {
                        buf.extend_from_slice(&(c as u32).to_le_bytes());
                    }
                }
                Node::Leaf {
                    keys,
                    values,
                    next_leaf,
                } => {
                    buf.push(NODE_TAG_LEAF);
                    buf.extend_from_slice(&(keys.len() as u32).to_le_bytes());
                    for k in keys {
                        write_value(&mut buf, k);
                    }
                    // values align 1:1 with keys, so we don't repeat the
                    // count — caller reads `n_keys` rids.
                    for rid in values {
                        buf.extend_from_slice(&rid.page_id.to_le_bytes());
                        buf.extend_from_slice(&rid.slot_index.to_le_bytes());
                    }
                    match next_leaf {
                        Some(nid) => {
                            buf.push(1);
                            buf.extend_from_slice(&(*nid as u32).to_le_bytes());
                        }
                        None => {
                            buf.push(0);
                        }
                    }
                }
            }
        }
        if self.format_version >= 2 {
            buf.extend_from_slice(&(self.empty_rids.len() as u32).to_le_bytes());
            for rid in &self.empty_rids {
                buf.extend_from_slice(&rid.page_id.to_le_bytes());
                buf.extend_from_slice(&rid.slot_index.to_le_bytes());
            }
        }

        // Append CRC32 checksum as the last 4 bytes of the serialized data.
        let mut hasher = Crc32Hasher::new();
        hasher.update(&buf);
        let checksum = hasher.finalize();
        buf.extend_from_slice(&checksum.to_le_bytes());

        // Atomic write: sibling .tmp then rename. Mirrors the catalog's
        // persist strategy so a crash between write and rename leaves
        // the old file intact.
        let mut tmp = path.to_path_buf();
        let tmp_name = match path.file_name() {
            Some(n) => {
                let mut s = n.to_os_string();
                s.push(".tmp");
                s
            }
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "btree path has no file name",
                ))
            }
        };
        tmp.set_file_name(tmp_name);

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }

        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(&buf)?;
        f.sync_data()?;
        drop(f);
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Reload a tree from disk. The returned tree's `path` is set to the
    /// supplied path so subsequent `save()` calls hit the same file.
    ///
    /// Guards against corrupted or maliciously large files: rejects files
    /// larger than 512MB and node counts above 10 million.
    pub fn load(path: &Path) -> io::Result<Self> {
        let mut f = fs::File::open(path)?;
        let file_size = f.metadata()?.len();
        if file_size > 512 * 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("btree file too large: {} bytes (limit 512MB)", file_size),
            ));
        }
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;

        // Verify CRC32 checksum (last 4 bytes of file).
        if buf.len() < 18 {
            // Minimum: 4 magic + 2 version + 4 root + 4 n_nodes + 4 crc = 18
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "btree file too short to contain header + checksum",
            ));
        }
        let data_len = buf.len() - 4;
        let stored_crc = u32::from_le_bytes([
            buf[data_len],
            buf[data_len + 1],
            buf[data_len + 2],
            buf[data_len + 3],
        ]);
        let mut hasher = Crc32Hasher::new();
        hasher.update(&buf[..data_len]);
        let computed_crc = hasher.finalize();
        if stored_crc != computed_crc {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "btree CRC32 checksum mismatch: stored {stored_crc:#010x}, computed {computed_crc:#010x}"
                ),
            ));
        }
        // Trim the checksum bytes so the rest of deserialization works unchanged.
        buf.truncate(data_len);

        let mut pos = 0usize;
        if buf.len() < 14 || &buf[0..4] != BTREE_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad btree magic",
            ));
        }
        pos += 4;
        let version = read_u16(&buf, &mut pos)?;
        if version == 0 || version > BTREE_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported btree version: {version}"),
            ));
        }
        let root = read_u32(&buf, &mut pos)? as usize;
        let n_nodes = read_u32(&buf, &mut pos)? as usize;

        if n_nodes > 10_000_000 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("btree file corrupt: unreasonable node count {n_nodes} (limit 10M)"),
            ));
        }

        let mut nodes: Vec<Node> = Vec::with_capacity(n_nodes);
        for _ in 0..n_nodes {
            let tag = read_u8(&buf, &mut pos)?;
            match tag {
                NODE_TAG_INTERNAL => {
                    let n_keys = read_u32(&buf, &mut pos)? as usize;
                    let mut keys = Vec::with_capacity(n_keys);
                    for _ in 0..n_keys {
                        keys.push(read_value(&buf, &mut pos)?);
                    }
                    let n_children = read_u32(&buf, &mut pos)? as usize;
                    let mut children = Vec::with_capacity(n_children);
                    for _ in 0..n_children {
                        children.push(read_u32(&buf, &mut pos)? as usize);
                    }
                    nodes.push(Node::Internal { keys, children });
                }
                NODE_TAG_LEAF => {
                    let n_keys = read_u32(&buf, &mut pos)? as usize;
                    let mut keys = Vec::with_capacity(n_keys);
                    for _ in 0..n_keys {
                        keys.push(read_value(&buf, &mut pos)?);
                    }
                    let mut values = Vec::with_capacity(n_keys);
                    for _ in 0..n_keys {
                        let page_id = read_u32(&buf, &mut pos)?;
                        let slot_index = read_u16(&buf, &mut pos)?;
                        values.push(RowId {
                            page_id,
                            slot_index,
                        });
                    }
                    let has_next = read_u8(&buf, &mut pos)? != 0;
                    let next_leaf = if has_next {
                        Some(read_u32(&buf, &mut pos)? as usize)
                    } else {
                        None
                    };
                    nodes.push(Node::Leaf {
                        keys,
                        values,
                        next_leaf,
                    });
                }
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unknown btree node tag: {other}"),
                    ));
                }
            }
        }

        let mut empty_rids = Vec::new();
        if version >= 2 {
            let count = read_u32(&buf, &mut pos)? as usize;
            if count > (buf.len().saturating_sub(pos)) / 6 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "btree empty RID count exceeds remaining file size",
                ));
            }
            empty_rids.reserve(count);
            for _ in 0..count {
                empty_rids.push(RowId {
                    page_id: read_u32(&buf, &mut pos)?,
                    slot_index: read_u16(&buf, &mut pos)?,
                });
            }
            if empty_rids.windows(2).any(|pair| {
                (pair[0].page_id, pair[0].slot_index) >= (pair[1].page_id, pair[1].slot_index)
            }) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "btree empty RID side set is not strictly ordered",
                ));
            }
        }
        if pos != buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "btree file has trailing bytes",
            ));
        }

        if root >= nodes.len() && !nodes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "btree root index out of range",
            ));
        }

        let mut tree = BTree {
            nodes,
            root,
            path: path.to_path_buf(),
            // Loaded tree matches its backing file exactly.
            dirty: false,
            format_version: version,
            empty_rids,
            total_entries: 0,
            distinct_keys: 0,
            // Load cannot tell a unique/expression (Raw) tree from a
            // non-unique composite one from the bytes alone. Count in Raw
            // mode; a non-unique column index caller invokes `mark_composite`
            // to switch and recount by value prefix.
            key_kind: KeyKind::Raw,
        };
        tree.recount();
        Ok(tree)
    }
}

#[derive(Default)]
struct ReverseKeyGroup {
    current_key: Option<Value>,
    rids: VecDeque<RowId>,
    peak: usize,
}

/// Reverse tree traversal visits equal-key entries in reverse RID order. Flush
/// each complete key group backwards so descending ORDER BY reverses keys but
/// preserves the same stable scan/RID order as the generic sort within ties.
fn flush_reverse_key_group(
    reverse_group: &mut VecDeque<RowId>,
    remaining_offset: &mut usize,
    limit: usize,
    result: &mut Vec<RowId>,
) -> bool {
    while let Some(rid) = reverse_group.pop_back() {
        if *remaining_offset > 0 {
            *remaining_offset -= 1;
        } else {
            result.push(rid);
            if result.len() == limit {
                return true;
            }
        }
    }
    false
}

// ─── on-disk value encoding ────────────────────────────────────────────────

fn write_value(buf: &mut Vec<u8>, v: &Value) {
    buf.push(v.type_id() as u8);
    match v {
        Value::Int(i) => buf.extend_from_slice(&i.to_le_bytes()),
        Value::Float(f) => buf.extend_from_slice(&f.to_le_bytes()),
        Value::Bool(b) => buf.push(if *b { 1 } else { 0 }),
        Value::Str(s) => {
            let bytes = s.as_bytes();
            buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(bytes);
        }
        Value::DateTime(t) => buf.extend_from_slice(&t.to_le_bytes()),
        Value::Uuid(u) => buf.extend_from_slice(u),
        Value::Bytes(b) => {
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
        }
        Value::Json(b) => {
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
        }
        Value::Empty => {}
    }
}

fn read_value(buf: &[u8], pos: &mut usize) -> io::Result<Value> {
    let tag = read_u8(buf, pos)?;
    let type_id = type_id_from_u8(tag)?;
    match type_id {
        TypeId::Int => {
            let v = read_i64(buf, pos)?;
            Ok(Value::Int(v))
        }
        TypeId::Float => {
            let raw = read_u64(buf, pos)?;
            Ok(Value::Float(f64::from_bits(raw)))
        }
        TypeId::Bool => {
            let b = read_u8(buf, pos)?;
            Ok(Value::Bool(b != 0))
        }
        TypeId::Str => {
            let n = read_u32(buf, pos)? as usize;
            if *pos + n > buf.len() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated btree str",
                ));
            }
            let s = std::str::from_utf8(&buf[*pos..*pos + n])
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 in btree str"))?
                .to_string();
            *pos += n;
            Ok(Value::Str(s))
        }
        TypeId::DateTime => {
            let v = read_i64(buf, pos)?;
            Ok(Value::DateTime(v))
        }
        TypeId::Uuid => {
            if *pos + 16 > buf.len() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated btree uuid",
                ));
            }
            let mut u = [0u8; 16];
            u.copy_from_slice(&buf[*pos..*pos + 16]);
            *pos += 16;
            Ok(Value::Uuid(u))
        }
        TypeId::Bytes => {
            let n = read_u32(buf, pos)? as usize;
            if *pos + n > buf.len() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated btree bytes",
                ));
            }
            let v = buf[*pos..*pos + n].to_vec();
            *pos += n;
            Ok(Value::Bytes(v))
        }
        TypeId::Json => {
            let n = read_u32(buf, pos)? as usize;
            if *pos + n > buf.len() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated btree json",
                ));
            }
            let v = buf[*pos..*pos + n].to_vec();
            *pos += n;
            Ok(Value::Json(v.into()))
        }
        TypeId::Empty => Ok(Value::Empty),
    }
}

fn type_id_from_u8(v: u8) -> io::Result<TypeId> {
    match v {
        0 => Ok(TypeId::Empty),
        1 => Ok(TypeId::Int),
        2 => Ok(TypeId::Float),
        3 => Ok(TypeId::Bool),
        4 => Ok(TypeId::Str),
        5 => Ok(TypeId::DateTime),
        6 => Ok(TypeId::Uuid),
        7 => Ok(TypeId::Bytes),
        8 => Ok(TypeId::Json),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown btree value tag: {other}"),
        )),
    }
}

fn read_u8(buf: &[u8], pos: &mut usize) -> io::Result<u8> {
    if *pos >= buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated btree",
        ));
    }
    let v = buf[*pos];
    *pos += 1;
    Ok(v)
}
fn read_u16(buf: &[u8], pos: &mut usize) -> io::Result<u16> {
    if *pos + 2 > buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated btree",
        ));
    }
    let v = u16::from_le_bytes(
        buf[*pos..*pos + 2]
            .try_into()
            .unwrap_or_else(|_| unreachable!()),
    );
    *pos += 2;
    Ok(v)
}
fn read_u32(buf: &[u8], pos: &mut usize) -> io::Result<u32> {
    if *pos + 4 > buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated btree",
        ));
    }
    let v = u32::from_le_bytes(
        buf[*pos..*pos + 4]
            .try_into()
            .unwrap_or_else(|_| unreachable!()),
    );
    *pos += 4;
    Ok(v)
}
fn read_u64(buf: &[u8], pos: &mut usize) -> io::Result<u64> {
    if *pos + 8 > buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated btree",
        ));
    }
    let v = u64::from_le_bytes(
        buf[*pos..*pos + 8]
            .try_into()
            .unwrap_or_else(|_| unreachable!()),
    );
    *pos += 8;
    Ok(v)
}
fn read_i64(buf: &[u8], pos: &mut usize) -> io::Result<i64> {
    Ok(read_u64(buf, pos)? as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_btree(name: &str) -> BTree {
        let path = std::env::temp_dir().join(format!("powdb_btree_{name}_{}", std::process::id()));
        BTree::create(&path).unwrap()
    }

    #[test]
    fn test_insert_and_lookup() {
        let mut bt = temp_btree("basic");
        let rid = RowId {
            page_id: 1,
            slot_index: 0,
        };
        bt.insert(Value::Int(42), rid);
        assert_eq!(bt.lookup(&Value::Int(42)), Some(rid));
        assert_eq!(bt.lookup(&Value::Int(99)), None);
    }

    #[test]
    fn test_many_inserts_and_lookups() {
        let mut bt = temp_btree("many");
        for i in 0..1000 {
            bt.insert(
                Value::Int(i),
                RowId {
                    page_id: (i / 100) as u32,
                    slot_index: (i % 100) as u16,
                },
            );
        }
        assert_eq!(bt.len(), 1000);
        for i in 0..1000 {
            let rid = bt
                .lookup(&Value::Int(i))
                .unwrap_or_else(|| panic!("key {i} missing"));
            assert_eq!(rid.page_id, (i / 100) as u32);
            assert_eq!(rid.slot_index, (i % 100) as u16);
        }
    }

    #[test]
    fn test_range_scan() {
        let mut bt = temp_btree("range");
        for i in 0..100 {
            bt.insert(
                Value::Int(i),
                RowId {
                    page_id: 0,
                    slot_index: i as u16,
                },
            );
        }
        let results: Vec<_> = bt.range(&Value::Int(10), &Value::Int(20)).collect();
        assert_eq!(results.len(), 11); // 10..=20 inclusive
        assert_eq!(results[0].0, Value::Int(10));
        assert_eq!(results[10].0, Value::Int(20));
    }

    #[test]
    fn test_string_keys() {
        let mut bt = temp_btree("strings");
        bt.insert(
            Value::Str("alice".into()),
            RowId {
                page_id: 0,
                slot_index: 0,
            },
        );
        bt.insert(
            Value::Str("bob".into()),
            RowId {
                page_id: 0,
                slot_index: 1,
            },
        );
        bt.insert(
            Value::Str("charlie".into()),
            RowId {
                page_id: 0,
                slot_index: 2,
            },
        );
        assert_eq!(bt.lookup(&Value::Str("bob".into())).unwrap().slot_index, 1);
        assert_eq!(bt.lookup(&Value::Str("dave".into())), None);
    }

    #[test]
    fn test_delete() {
        let mut bt = temp_btree("delete");
        bt.insert(
            Value::Int(1),
            RowId {
                page_id: 0,
                slot_index: 0,
            },
        );
        bt.insert(
            Value::Int(2),
            RowId {
                page_id: 0,
                slot_index: 1,
            },
        );
        assert!(bt.delete(&Value::Int(1)));
        assert_eq!(bt.lookup(&Value::Int(1)), None);
        assert_eq!(bt.lookup(&Value::Int(2)).unwrap().slot_index, 1);
        assert_eq!(bt.len(), 1);
    }

    #[test]
    fn test_duplicate_key_updates() {
        let mut bt = temp_btree("dup");
        bt.insert(
            Value::Int(42),
            RowId {
                page_id: 0,
                slot_index: 0,
            },
        );
        bt.insert(
            Value::Int(42),
            RowId {
                page_id: 1,
                slot_index: 5,
            },
        );
        // Should update, not duplicate
        assert_eq!(bt.len(), 1);
        assert_eq!(
            bt.lookup(&Value::Int(42)).unwrap(),
            RowId {
                page_id: 1,
                slot_index: 5
            }
        );
    }

    #[test]
    fn test_large_tree_splits() {
        let mut bt = temp_btree("large");
        // Insert enough to force multiple splits
        for i in 0..5000 {
            bt.insert(
                Value::Int(i),
                RowId {
                    page_id: (i / 256) as u32,
                    slot_index: (i % 256) as u16,
                },
            );
        }
        assert_eq!(bt.len(), 5000);
        // Verify all entries
        for i in 0..5000 {
            assert!(
                bt.lookup(&Value::Int(i)).is_some(),
                "key {i} not found after splits"
            );
        }
        // Range scan across splits
        let results: Vec<_> = bt.range(&Value::Int(2000), &Value::Int(3000)).collect();
        assert_eq!(results.len(), 1001);
    }

    #[test]
    fn test_insert_int_matches_insert() {
        // Mission C Phase 15: specialized int insert path must produce a
        // tree indistinguishable from repeated generic `insert` calls
        // across the full key space, including updates (duplicate keys),
        // splits, and interleaved reads.
        let mut bt_fast = temp_btree("insert_int_fast");
        let mut bt_refn = temp_btree("insert_int_refn");
        for i in 0..5000i64 {
            let rid = RowId {
                page_id: (i / 256) as u32,
                slot_index: (i % 256) as u16,
            };
            bt_fast.insert_int(i, rid);
            bt_refn.insert(Value::Int(i), rid);
        }
        // Cross-check every key 0..5000 and a few missing ones on the
        // edges.
        for i in -5..5005 {
            assert_eq!(
                bt_fast.lookup_int(i),
                bt_refn.lookup_int(i),
                "divergence at key {i}"
            );
        }
        assert_eq!(bt_fast.len(), bt_refn.len());

        // Duplicate-key update via the fast path should land on the same
        // slot as the generic insert.
        let new_rid = RowId {
            page_id: 999,
            slot_index: 42,
        };
        bt_fast.insert_int(100, new_rid);
        bt_refn.insert(Value::Int(100), new_rid);
        assert_eq!(bt_fast.lookup_int(100), Some(new_rid));
        assert_eq!(bt_refn.lookup_int(100), Some(new_rid));
        assert_eq!(bt_fast.len(), bt_refn.len());
    }

    #[test]
    fn test_insert_int_reverse_order_splits() {
        // Exercise descending-key insertion, which stresses the leaf
        // split path because every insert lands at position 0.
        let mut bt = temp_btree("insert_int_reverse");
        for i in (0..1000i64).rev() {
            bt.insert_int(
                i,
                RowId {
                    page_id: 0,
                    slot_index: i as u16,
                },
            );
        }
        for i in 0..1000i64 {
            assert_eq!(
                bt.lookup_int(i),
                Some(RowId {
                    page_id: 0,
                    slot_index: i as u16
                }),
                "missing key {i}",
            );
        }
        assert_eq!(bt.len(), 1000);
    }

    #[test]
    fn test_lookup_int_matches_lookup() {
        // Mission D7: specialized int path must return identical results
        // to the generic `lookup` for every key, present or absent, across
        // a tree large enough to exercise multiple levels + splits.
        let mut bt = temp_btree("lookup_int");
        for i in 0..5000 {
            bt.insert(
                Value::Int(i),
                RowId {
                    page_id: (i / 256) as u32,
                    slot_index: (i % 256) as u16,
                },
            );
        }
        for i in -5..5005 {
            let generic = bt.lookup(&Value::Int(i));
            let specialized = bt.lookup_int(i);
            assert_eq!(generic, specialized, "divergence at key {i}");
        }
    }

    #[test]
    fn test_delete_many_int_matches_per_key_delete() {
        // Mission C Phase 12: batch delete must agree with repeated
        // per-key `delete_int` calls on every lookup across the full tree.
        let mut bt_batch = temp_btree("delete_many_batch");
        let mut bt_refn = temp_btree("delete_many_refn");
        for i in 0..5000i64 {
            let rid = RowId {
                page_id: (i / 256) as u32,
                slot_index: (i % 256) as u16,
            };
            bt_batch.insert(Value::Int(i), rid);
            bt_refn.insert(Value::Int(i), rid);
        }

        // Delete every 3rd key, plus some missing keys for good measure.
        let mut to_delete: Vec<i64> = (0..5000).filter(|i| i % 3 == 0).collect();
        // Inject missing keys that shouldn't affect anything.
        to_delete.push(7000);
        to_delete.push(-5);
        to_delete.sort();

        let removed = bt_batch.delete_many_int(&to_delete);
        // 5000 / 3 rounded up = 1667 present keys deleted; the two missing
        // keys should be silently skipped.
        let expected_removed = to_delete.iter().filter(|k| **k >= 0 && **k < 5000).count();
        assert_eq!(removed, expected_removed);

        for k in &to_delete {
            bt_refn.delete_int(*k);
        }

        // Cross-check every key 0..5000 (present or absent).
        for i in 0..5000i64 {
            let a = bt_batch.lookup_int(i);
            let b = bt_refn.lookup_int(i);
            assert_eq!(a, b, "divergence at key {i}");
        }
        assert_eq!(bt_batch.len(), bt_refn.len());
    }

    #[test]
    fn test_delete_many_int_empty_slice() {
        let mut bt = temp_btree("delete_many_empty");
        for i in 0..100 {
            bt.insert(
                Value::Int(i),
                RowId {
                    page_id: 0,
                    slot_index: i as u16,
                },
            );
        }
        let removed = bt.delete_many_int(&[]);
        assert_eq!(removed, 0);
        assert_eq!(bt.len(), 100);
    }

    #[test]
    fn test_delete_many_int_all_missing() {
        let mut bt = temp_btree("delete_many_missing");
        for i in 0..100 {
            bt.insert(
                Value::Int(i),
                RowId {
                    page_id: 0,
                    slot_index: i as u16,
                },
            );
        }
        let removed = bt.delete_many_int(&[1000, 2000, 3000]);
        assert_eq!(removed, 0);
        assert_eq!(bt.len(), 100);
    }

    #[test]
    fn test_delete_many_int_all_keys() {
        let mut bt = temp_btree("delete_many_all");
        let keys: Vec<i64> = (0..500).collect();
        for &i in &keys {
            bt.insert(
                Value::Int(i),
                RowId {
                    page_id: 0,
                    slot_index: i as u16,
                },
            );
        }
        let removed = bt.delete_many_int(&keys);
        assert_eq!(removed, 500);
        assert_eq!(bt.len(), 0);
        for i in &keys {
            assert_eq!(bt.lookup_int(*i), None);
        }
    }

    #[test]
    fn test_save_load_roundtrip_int_keys() {
        // Mission 3: save + load must reproduce an equivalent tree for
        // both small (single-leaf) and large (multi-level) int-keyed
        // trees. "Equivalent" means every key looks up to the same
        // RowId, and the total length matches.
        let tmp =
            std::env::temp_dir().join(format!("powdb_btree_save_int_{}.idx", std::process::id()));
        let _ = std::fs::remove_file(&tmp);

        let mut bt = BTree::create(&tmp).unwrap();
        for i in 0..2000i64 {
            bt.insert_int(
                i,
                RowId {
                    page_id: (i / 256) as u32,
                    slot_index: (i % 256) as u16,
                },
            );
        }
        bt.save().unwrap();

        let reloaded = BTree::load(&tmp).unwrap();
        assert_eq!(reloaded.len(), bt.len());
        for i in 0..2000i64 {
            let orig = bt.lookup_int(i);
            let round = reloaded.lookup_int(i);
            assert_eq!(orig, round, "mismatch at key {i}");
            assert!(round.is_some());
        }
        // Missing keys stay missing.
        assert_eq!(reloaded.lookup_int(-1), None);
        assert_eq!(reloaded.lookup_int(9999), None);
        // Range scan across splits should match order.
        let orig_range: Vec<_> = bt.range(&Value::Int(500), &Value::Int(600)).collect();
        let round_range: Vec<_> = reloaded.range(&Value::Int(500), &Value::Int(600)).collect();
        assert_eq!(orig_range, round_range);

        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_save_load_roundtrip_mixed_value_types() {
        // String keys exercise the variable-length value encoder path.
        let tmp =
            std::env::temp_dir().join(format!("powdb_btree_save_mixed_{}.idx", std::process::id()));
        let _ = std::fs::remove_file(&tmp);

        let mut bt = BTree::create(&tmp).unwrap();
        let keys = [
            (
                "alice",
                RowId {
                    page_id: 1,
                    slot_index: 0,
                },
            ),
            (
                "bob",
                RowId {
                    page_id: 2,
                    slot_index: 1,
                },
            ),
            (
                "charlie",
                RowId {
                    page_id: 3,
                    slot_index: 2,
                },
            ),
            (
                "diana",
                RowId {
                    page_id: 4,
                    slot_index: 3,
                },
            ),
            (
                "",
                RowId {
                    page_id: 5,
                    slot_index: 4,
                },
            ),
            (
                "unicode ⚡",
                RowId {
                    page_id: 6,
                    slot_index: 5,
                },
            ),
        ];
        for (k, rid) in keys.iter() {
            bt.insert(Value::Str((*k).into()), *rid);
        }
        bt.save().unwrap();

        let reloaded = BTree::load(&tmp).unwrap();
        assert_eq!(reloaded.len(), keys.len());
        for (k, expected_rid) in keys.iter() {
            let got = reloaded.lookup(&Value::Str((*k).into()));
            assert_eq!(got, Some(*expected_rid), "mismatch at key {k:?}");
        }
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_save_load_empty_tree() {
        // A freshly created empty tree must round-trip (one empty leaf,
        // root = 0).
        let tmp =
            std::env::temp_dir().join(format!("powdb_btree_save_empty_{}.idx", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let mut bt = BTree::create(&tmp).unwrap();
        bt.save().unwrap();
        let reloaded = BTree::load(&tmp).unwrap();
        assert_eq!(reloaded.len(), 0);
        assert!(reloaded.is_empty());
        assert_eq!(reloaded.lookup(&Value::Int(42)), None);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_reverse_insert_order() {
        let mut bt = temp_btree("reverse");
        for i in (0..500).rev() {
            bt.insert(
                Value::Int(i),
                RowId {
                    page_id: 0,
                    slot_index: i as u16,
                },
            );
        }
        assert_eq!(bt.len(), 500);
        // Range scan should return sorted order
        let results: Vec<_> = bt.range(&Value::Int(0), &Value::Int(499)).collect();
        for (j, (k, _)) in results.iter().enumerate() {
            assert_eq!(*k, Value::Int(j as i64));
        }
    }

    // ─── Non-unique index tests ──────────────────────────────────────────

    #[test]
    fn test_non_unique_insert_and_lookup_prefix() {
        let mut bt = temp_btree("nonunique_basic");
        // Insert 5 rows with 3 distinct values.
        let rids = [
            RowId {
                page_id: 0,
                slot_index: 0,
            },
            RowId {
                page_id: 0,
                slot_index: 1,
            },
            RowId {
                page_id: 0,
                slot_index: 2,
            },
            RowId {
                page_id: 0,
                slot_index: 3,
            },
            RowId {
                page_id: 0,
                slot_index: 4,
            },
        ];
        bt.insert_non_unique(Value::Str("Eng".into()), rids[0]);
        bt.insert_non_unique(Value::Str("Eng".into()), rids[1]);
        bt.insert_non_unique(Value::Str("Sales".into()), rids[2]);
        bt.insert_non_unique(Value::Str("Eng".into()), rids[3]);
        bt.insert_non_unique(Value::Str("HR".into()), rids[4]);

        // All 5 entries should exist.
        assert_eq!(bt.len(), 5);

        // lookup_prefix for "Eng" should return 3 RowIds.
        let eng_rids = bt.lookup_prefix(&Value::Str("Eng".into()));
        assert_eq!(eng_rids.len(), 3, "Eng should have 3 rows");
        assert!(eng_rids.contains(&rids[0]));
        assert!(eng_rids.contains(&rids[1]));
        assert!(eng_rids.contains(&rids[3]));

        // lookup_prefix for "Sales" should return 1 RowId.
        let sales_rids = bt.lookup_prefix(&Value::Str("Sales".into()));
        assert_eq!(sales_rids.len(), 1);
        assert_eq!(sales_rids[0], rids[2]);

        // lookup_prefix for "HR" should return 1 RowId.
        let hr_rids = bt.lookup_prefix(&Value::Str("HR".into()));
        assert_eq!(hr_rids.len(), 1);
        assert_eq!(hr_rids[0], rids[4]);

        // lookup_prefix for missing value should return empty.
        let missing = bt.lookup_prefix(&Value::Str("Legal".into()));
        assert!(missing.is_empty());
    }

    #[test]
    fn test_non_unique_int_insert_and_lookup() {
        let mut bt = temp_btree("nonunique_int");
        let rids: Vec<RowId> = (0..10)
            .map(|i| RowId {
                page_id: 0,
                slot_index: i,
            })
            .collect();

        // Three distinct int keys: 100, 200, 300.
        bt.insert_non_unique_int(100, rids[0]);
        bt.insert_non_unique_int(200, rids[1]);
        bt.insert_non_unique_int(100, rids[2]);
        bt.insert_non_unique_int(300, rids[3]);
        bt.insert_non_unique_int(100, rids[4]);
        bt.insert_non_unique_int(200, rids[5]);

        assert_eq!(bt.len(), 6);

        let hits = bt.lookup_prefix_int(100);
        assert_eq!(hits.len(), 3);
        assert!(hits.contains(&rids[0]));
        assert!(hits.contains(&rids[2]));
        assert!(hits.contains(&rids[4]));

        let hits = bt.lookup_prefix_int(200);
        assert_eq!(hits.len(), 2);

        let hits = bt.lookup_prefix_int(300);
        assert_eq!(hits.len(), 1);

        let hits = bt.lookup_prefix_int(999);
        assert!(hits.is_empty());
    }

    #[test]
    fn test_non_unique_range_rids() {
        let mut bt = temp_btree("nonunique_range");
        let rids: Vec<RowId> = (0..6u32)
            .map(|i| RowId {
                page_id: i,
                slot_index: 0,
            })
            .collect();
        for (i, rid) in rids.iter().enumerate() {
            bt.insert_non_unique_int((i as i64) * 10, *rid); // 0,10,20,30,40,50
        }
        // 10 <= v <= 30 → rids[1..=3]
        let hits = bt.range_rids(Some(&Value::Int(10)), Some(&Value::Int(30)));
        assert_eq!(hits, vec![rids[1], rids[2], rids[3]]);
        // unbounded below
        let hits = bt.range_rids(None, Some(&Value::Int(10)));
        assert_eq!(hits, vec![rids[0], rids[1]]);
        // unbounded above
        let hits = bt.range_rids(Some(&Value::Int(40)), None);
        assert_eq!(hits, vec![rids[4], rids[5]]);
        // duplicates within the range all come back
        bt.insert_non_unique_int(
            20,
            RowId {
                page_id: 99,
                slot_index: 7,
            },
        );
        let hits = bt.range_rids(Some(&Value::Int(20)), Some(&Value::Int(20)));
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn test_non_unique_delete() {
        let mut bt = temp_btree("nonunique_delete");
        let rids = [
            RowId {
                page_id: 1,
                slot_index: 0,
            },
            RowId {
                page_id: 1,
                slot_index: 1,
            },
            RowId {
                page_id: 1,
                slot_index: 2,
            },
        ];
        bt.insert_non_unique(Value::Str("Eng".into()), rids[0]);
        bt.insert_non_unique(Value::Str("Eng".into()), rids[1]);
        bt.insert_non_unique(Value::Str("Eng".into()), rids[2]);

        assert_eq!(bt.len(), 3);

        // Delete one specific (value, rid) pair.
        let deleted = bt.delete_non_unique(&Value::Str("Eng".into()), rids[1]);
        assert!(deleted, "delete should succeed");
        assert_eq!(bt.len(), 2);

        // The remaining two should still be findable.
        let remaining = bt.lookup_prefix(&Value::Str("Eng".into()));
        assert_eq!(remaining.len(), 2);
        assert!(remaining.contains(&rids[0]));
        assert!(remaining.contains(&rids[2]));
        assert!(!remaining.contains(&rids[1]));
    }

    #[test]
    fn test_non_unique_delete_int() {
        let mut bt = temp_btree("nonunique_delete_int");
        let rids = [
            RowId {
                page_id: 2,
                slot_index: 0,
            },
            RowId {
                page_id: 2,
                slot_index: 1,
            },
            RowId {
                page_id: 2,
                slot_index: 2,
            },
        ];
        bt.insert_non_unique_int(42, rids[0]);
        bt.insert_non_unique_int(42, rids[1]);
        bt.insert_non_unique_int(42, rids[2]);

        assert_eq!(bt.len(), 3);

        bt.delete_non_unique_int(42, rids[0]);
        let remaining = bt.lookup_prefix_int(42);
        assert_eq!(remaining.len(), 2);
        assert!(!remaining.contains(&rids[0]));
    }

    #[test]
    fn test_non_unique_save_load_roundtrip() {
        let tmp = std::env::temp_dir().join(format!(
            "powdb_btree_nonunique_roundtrip_{}.idx",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&tmp);

        let rids = [
            RowId {
                page_id: 0,
                slot_index: 0,
            },
            RowId {
                page_id: 0,
                slot_index: 1,
            },
            RowId {
                page_id: 0,
                slot_index: 2,
            },
        ];

        {
            let mut bt = BTree::create(&tmp).unwrap();
            bt.insert_non_unique(Value::Str("Eng".into()), rids[0]);
            bt.insert_non_unique(Value::Str("Eng".into()), rids[1]);
            bt.insert_non_unique(Value::Str("Sales".into()), rids[2]);
            bt.save().unwrap();
        }

        let reloaded = BTree::load(&tmp).unwrap();
        assert_eq!(reloaded.len(), 3);

        let eng = reloaded.lookup_prefix(&Value::Str("Eng".into()));
        assert_eq!(eng.len(), 2);
        assert!(eng.contains(&rids[0]));
        assert!(eng.contains(&rids[1]));

        let sales = reloaded.lookup_prefix(&Value::Str("Sales".into()));
        assert_eq!(sales.len(), 1);
        assert_eq!(sales[0], rids[2]);

        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_non_unique_many_duplicates() {
        // Stress test: 1000 rows with only 5 distinct values.
        let mut bt = temp_btree("nonunique_many");
        let depts = ["Eng", "Sales", "HR", "Legal", "Support"];
        for i in 0..1000u16 {
            let dept = depts[i as usize % 5];
            bt.insert_non_unique(
                Value::Str(dept.into()),
                RowId {
                    page_id: (i / 256) as u32,
                    slot_index: i,
                },
            );
        }
        assert_eq!(bt.len(), 1000);

        for dept in &depts {
            let hits = bt.lookup_prefix(&Value::Str((*dept).into()));
            assert_eq!(hits.len(), 200, "each dept should have 200 rows");
        }

        // Delete all "HR" rows one by one.
        for i in (2..1000u16).step_by(5) {
            let rid = RowId {
                page_id: (i / 256) as u32,
                slot_index: i,
            };
            bt.delete_non_unique(&Value::Str("HR".into()), rid);
        }
        let hr = bt.lookup_prefix(&Value::Str("HR".into()));
        assert!(hr.is_empty(), "all HR rows should be deleted");
        assert_eq!(bt.len(), 800);
    }

    #[test]
    fn bounded_raw_order_visits_only_offset_plus_limit_scalars() {
        let path = std::env::temp_dir().join(format!(
            "powdb_btree_bounded_raw_{}.idx",
            std::process::id()
        ));
        let mut tree = BTree::create_v2(&path).unwrap();
        for value in 0..10_000u32 {
            tree.insert_duplicate(
                Value::Int(value as i64),
                RowId {
                    page_id: value,
                    slot_index: 0,
                },
            );
        }

        let (asc, asc_visited, asc_peak_group) = tree.bounded_ordered_rids_with_stats(false, 10, 5);
        assert_eq!(
            asc.iter().map(|rid| rid.page_id).collect::<Vec<_>>(),
            vec![10, 11, 12, 13, 14]
        );
        assert_eq!(asc_visited, 15);
        assert_eq!(asc_peak_group, 0);

        let (desc, desc_visited, desc_peak_group) =
            tree.bounded_ordered_rids_with_stats(true, 10, 5);
        assert_eq!(
            desc.iter().map(|rid| rid.page_id).collect::<Vec<_>>(),
            vec![9_989, 9_988, 9_987, 9_986, 9_985]
        );
        assert_eq!(
            desc_visited, 16,
            "descending traversal reads one lookahead key to close the final tie group"
        );
        assert_eq!(desc_peak_group, 1);
    }

    #[test]
    fn bounded_raw_order_keeps_empty_side_set_last_in_both_directions() {
        let path = std::env::temp_dir().join(format!(
            "powdb_btree_bounded_empty_{}.idx",
            std::process::id()
        ));
        let mut tree = BTree::create_v2(&path).unwrap();
        for value in 1..=3u32 {
            tree.insert_duplicate(
                Value::Int(value as i64),
                RowId {
                    page_id: value,
                    slot_index: 0,
                },
            );
        }
        tree.insert_empty(RowId {
            page_id: 4,
            slot_index: 0,
        });
        tree.insert_empty(RowId {
            page_id: 5,
            slot_index: 0,
        });

        assert_eq!(
            tree.bounded_ordered_rids_nulls_last(false, 2, 3)
                .iter()
                .map(|rid| rid.page_id)
                .collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
        assert_eq!(
            tree.bounded_ordered_rids_nulls_last(true, 2, 3)
                .iter()
                .map(|rid| rid.page_id)
                .collect::<Vec<_>>(),
            vec![1, 4, 5]
        );
        assert_eq!(
            tree.bounded_ordered_rids_nulls_last(true, 0, 2)
                .iter()
                .map(|rid| rid.page_id)
                .collect::<Vec<_>>(),
            vec![3, 2],
            "a satisfied descending scalar limit must not append null-side RIDs"
        );
    }

    #[test]
    fn bounded_descending_order_preserves_rid_order_inside_duplicate_key_groups() {
        let path = std::env::temp_dir().join(format!(
            "powdb_btree_bounded_desc_ties_{}.idx",
            std::process::id()
        ));
        let mut tree = BTree::create_v2(&path).unwrap();

        // More than one leaf of duplicate keys catches the boundary case: a
        // reverse leaf walk must reverse key groups, not every RID entry.
        for page_id in 0..600 {
            tree.insert_duplicate(
                Value::Int(20),
                RowId {
                    page_id,
                    slot_index: 0,
                },
            );
        }
        for page_id in [1_000, 1_001] {
            tree.insert_duplicate(
                Value::Int(30),
                RowId {
                    page_id,
                    slot_index: 0,
                },
            );
        }

        let (first_window, _, first_peak_group) = tree.bounded_ordered_rids_with_stats(true, 0, 4);
        assert_eq!(
            first_window
                .iter()
                .map(|rid| rid.page_id)
                .collect::<Vec<_>>(),
            vec![1_000, 1_001, 0, 1]
        );
        assert_eq!(
            first_peak_group, 2,
            "the earlier key group fills two result slots, leaving only two buffered tie rows"
        );

        let (offset_window, _, offset_peak_group) =
            tree.bounded_ordered_rids_with_stats(true, 1, 3);
        assert_eq!(
            offset_window
                .iter()
                .map(|rid| rid.page_id)
                .collect::<Vec<_>>(),
            vec![1_001, 0, 1],
            "OFFSET/LIMIT must cut through a tie in stable RID order"
        );
        assert_eq!(
            offset_peak_group, 2,
            "buffering tracks the remaining window, not the 600-row tie cardinality"
        );
    }

    // --- Per-index statistics ---------------------------------------------

    fn stat_rid(page: u32, slot: u16) -> RowId {
        RowId {
            page_id: page,
            slot_index: slot,
        }
    }

    fn temp_btree_v2(name: &str) -> BTree {
        let path =
            std::env::temp_dir().join(format!("powdb_btree_v2_{name}_{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        BTree::create_v2(&path).unwrap()
    }

    /// Remove a tree's backing file (and any leftover `.tmp` sibling) so the
    /// stats tests do not litter the temp directory.
    fn cleanup(bt: &BTree) {
        let path = bt.file_path().to_path_buf();
        let _ = std::fs::remove_file(&path);
        if let Some(name) = path.file_name() {
            let mut tmp = name.to_os_string();
            tmp.push(".tmp");
            let _ = std::fs::remove_file(path.with_file_name(tmp));
        }
    }

    #[test]
    fn stats_exact_through_insert_replace_delete_int() {
        let mut bt = temp_btree("stats_int");
        assert_eq!(bt.stats().total_entries, 0);
        assert_eq!(bt.stats().distinct_keys, 0);

        for i in 0..50i64 {
            bt.insert_int(i, stat_rid(0, i as u16));
        }
        assert_eq!(bt.stats().total_entries, 50);
        assert_eq!(bt.stats().distinct_keys, 50);

        // Replacing an existing key must move neither counter.
        bt.insert_int(10, stat_rid(9, 9));
        assert_eq!(bt.stats().total_entries, 50);
        assert_eq!(bt.stats().distinct_keys, 50);

        assert!(bt.delete_int(10));
        assert_eq!(bt.stats().total_entries, 49);
        assert_eq!(bt.stats().distinct_keys, 49);

        // Deleting a missing key changes nothing.
        assert!(!bt.delete_int(10));
        assert_eq!(bt.stats().total_entries, 49);
        assert_eq!(bt.stats().distinct_keys, 49);
    }

    #[test]
    fn stats_exact_through_insert_replace_delete_str() {
        let mut bt = temp_btree("stats_str");
        for i in 0..30 {
            bt.insert(Value::Str(format!("k{i:02}")), stat_rid(0, i as u16));
        }
        assert_eq!(bt.stats().total_entries, 30);
        assert_eq!(bt.stats().distinct_keys, 30);

        bt.insert(Value::Str("k05".into()), stat_rid(7, 7));
        assert_eq!(bt.stats().total_entries, 30);
        assert_eq!(bt.stats().distinct_keys, 30);

        assert!(bt.delete(&Value::Str("k05".into())));
        assert_eq!(bt.stats().total_entries, 29);
        assert_eq!(bt.stats().distinct_keys, 29);
    }

    #[test]
    fn stats_non_unique_composite_duplicates() {
        let mut bt = temp_btree("stats_nonuniq");
        // Same logical key "A" gets three distinct rids.
        bt.insert_non_unique(Value::Str("A".into()), stat_rid(0, 1));
        bt.insert_non_unique(Value::Str("A".into()), stat_rid(0, 2));
        bt.insert_non_unique(Value::Str("A".into()), stat_rid(0, 3));
        assert_eq!(bt.stats().total_entries, 3);
        assert_eq!(bt.stats().distinct_keys, 1);

        // A second logical key.
        bt.insert_non_unique(Value::Str("B".into()), stat_rid(1, 1));
        assert_eq!(bt.stats().total_entries, 4);
        assert_eq!(bt.stats().distinct_keys, 2);

        // Delete one of A's rids: still one distinct A.
        assert!(bt.delete_non_unique(&Value::Str("A".into()), stat_rid(0, 2)));
        assert_eq!(bt.stats().total_entries, 3);
        assert_eq!(bt.stats().distinct_keys, 2);

        // Delete A's remaining rids: A disappears as a distinct key.
        assert!(bt.delete_non_unique(&Value::Str("A".into()), stat_rid(0, 1)));
        assert!(bt.delete_non_unique(&Value::Str("A".into()), stat_rid(0, 3)));
        assert_eq!(bt.stats().total_entries, 1);
        assert_eq!(bt.stats().distinct_keys, 1);

        // Re-inserting the same (col_val, rid) is a no-op replace, not a bump.
        bt.insert_non_unique(Value::Str("B".into()), stat_rid(1, 1));
        assert_eq!(bt.stats().total_entries, 1);
        assert_eq!(bt.stats().distinct_keys, 1);
    }

    #[test]
    fn stats_non_unique_int_composite() {
        let mut bt = temp_btree("stats_nonuniq_int");
        for slot in 0..5u16 {
            bt.insert_non_unique_int(100, stat_rid(0, slot));
        }
        bt.insert_non_unique_int(200, stat_rid(1, 0));
        assert_eq!(bt.stats().total_entries, 6);
        assert_eq!(bt.stats().distinct_keys, 2);

        for slot in 0..5u16 {
            assert!(bt.delete_non_unique_int(100, stat_rid(0, slot)));
        }
        assert_eq!(bt.stats().total_entries, 1);
        assert_eq!(bt.stats().distinct_keys, 1);
    }

    #[test]
    fn stats_empty_values_excluded_and_counted() {
        let mut bt = temp_btree_v2("stats_empty");
        bt.insert_duplicate(Value::Int(1), stat_rid(0, 0));
        bt.insert_duplicate(Value::Int(1), stat_rid(0, 1));
        bt.insert_duplicate(Value::Int(2), stat_rid(0, 2));
        // Empty (missing/null) keys route to the side list.
        bt.insert_duplicate(Value::Empty, stat_rid(9, 0));
        bt.insert_empty(stat_rid(9, 1));

        let stats = bt.stats();
        assert_eq!(stats.total_entries, 3, "empties excluded from total");
        assert_eq!(stats.distinct_keys, 2, "empties excluded from distinct");
        assert_eq!(stats.empty_count, 2, "both empties counted separately");

        assert!(bt.delete_empty(stat_rid(9, 0)));
        assert_eq!(bt.stats().empty_count, 1);
        assert_eq!(bt.stats().total_entries, 3);
        assert_eq!(bt.stats().distinct_keys, 2);
    }

    #[test]
    fn stats_duplicate_raw_tree() {
        // Expression-index shape: duplicate raw keys repeat physically.
        let mut bt = temp_btree_v2("stats_dup_raw");
        bt.insert_duplicate(Value::Int(7), stat_rid(0, 0));
        bt.insert_duplicate(Value::Int(7), stat_rid(0, 1));
        bt.insert_duplicate(Value::Int(7), stat_rid(0, 2));
        bt.insert_duplicate(Value::Int(8), stat_rid(0, 3));
        assert_eq!(bt.stats().total_entries, 4);
        assert_eq!(bt.stats().distinct_keys, 2);

        // Removing one rid of key 7 keeps 7 as a distinct key.
        assert!(bt.delete_pair(&Value::Int(7), stat_rid(0, 1)));
        assert_eq!(bt.stats().total_entries, 3);
        assert_eq!(bt.stats().distinct_keys, 2);

        // Removing 7's last two rids drops it as a distinct key.
        assert!(bt.delete_pair(&Value::Int(7), stat_rid(0, 0)));
        assert!(bt.delete_pair(&Value::Int(7), stat_rid(0, 2)));
        assert_eq!(bt.stats().total_entries, 1);
        assert_eq!(bt.stats().distinct_keys, 1);
    }

    #[test]
    fn stats_duplicate_raw_distinct_survives_multi_leaf_delete() {
        // Regression: a raw key's duplicates span several leaves once they
        // exceed the 256-key leaf order. The distinct-maintenance probe used to
        // descend to a single rightmost candidate leaf (`lookup` / `last_rid`),
        // so deleting the duplicates that happen to live in the boundary leaves
        // misreported the key as gone and drifted `distinct_keys` permanently.
        // `raw_key_present` walks the leaf chain, so the count stays truthful.
        let mut bt = temp_btree_v2("stats_dup_multileaf");
        for slot in 0..600u16 {
            bt.insert_duplicate(Value::Int(7), stat_rid(0, slot));
            bt.insert_duplicate(Value::Int(8), stat_rid(1, slot));
        }
        assert_eq!(bt.stats().total_entries, 1200);
        assert_eq!(bt.stats().distinct_keys, 2);

        // Delete the 500 highest-rid pairs of key 7. 100 low-rid copies remain,
        // so 7 is still a distinct key alongside 8.
        for slot in (100..600u16).rev() {
            assert!(bt.delete_pair(&Value::Int(7), stat_rid(0, slot)));
        }
        assert_eq!(bt.stats().total_entries, 700);
        assert_eq!(
            bt.stats().distinct_keys,
            2,
            "7 is still present via its low-rid copies in the leftmost leaves"
        );
    }

    #[test]
    fn stats_composite_embedded_nul() {
        // v3 escaped encoding: "A" and "A\0" are two distinct values. The live
        // incremental counter and the leaf-chain recount must agree on 2 both
        // in memory and after a save/load round trip and a checkpoint recount.
        let mut bt = temp_btree_v2("stats_embedded_nul");
        bt.key_kind = KeyKind::Composite;
        bt.insert_non_unique(Value::Str("A".into()), stat_rid(0, 5));
        bt.insert_non_unique(Value::Str("A\0".into()), stat_rid(0, 3));
        bt.insert_non_unique(Value::Str("A".into()), stat_rid(0, 7));
        assert_eq!(bt.stats().total_entries, 3);
        assert_eq!(
            bt.stats().distinct_keys,
            2,
            "\"A\" and \"A\\0\" are two distinct values under v3 escaping"
        );

        // Checkpoint recount agrees with the live counter.
        bt.save().unwrap();
        assert_eq!(bt.stats().total_entries, 3);
        assert_eq!(bt.stats().distinct_keys, 2, "recount agrees: 2 distinct");

        // Reload from disk (v3 file) and switch to composite counting: still 2.
        let mut reloaded = BTree::load(bt.file_path()).unwrap();
        reloaded.mark_composite();
        assert_eq!(reloaded.stats().total_entries, 3);
        assert_eq!(reloaded.stats().distinct_keys, 2, "2 distinct after reload");
        cleanup(&bt);
    }

    #[test]
    fn stats_heal_after_rollback() {
        let dir = std::env::temp_dir().join(format!("powdb_stats_rollback_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("rollback.idx");

        // Committed state on disk: 20 keys.
        {
            let mut bt = BTree::create(&path).unwrap();
            for i in 0..20i64 {
                bt.insert_int(i, stat_rid(0, i as u16));
            }
            bt.save_to(&path).unwrap();
        }
        let committed = BTree::load(&path).unwrap().stats();
        assert_eq!(committed.total_entries, 20);
        assert_eq!(committed.distinct_keys, 20);

        // Load, mutate in memory, then discard the dirty flag (ROLLBACK): the
        // uncommitted mutations never reach disk, so a fresh load heals to the
        // committed counters.
        {
            let mut bt = BTree::load(&path).unwrap();
            for i in 20..40i64 {
                bt.insert_int(i, stat_rid(1, i as u16));
            }
            assert_eq!(bt.stats().total_entries, 40);
            bt.discard_dirty();
        }
        let after_rollback = BTree::load(&path).unwrap().stats();
        assert_eq!(after_rollback, committed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stats_insert_duplicate_rebuild_fallback() {
        // Force the out-of-order rebuild branch: insert a smaller rid after a
        // larger one under the same raw key. The rebuild path recomputes both
        // counters via a full leaf walk, so they must match a ground-truth
        // reload.
        let mut bt = temp_btree_v2("stats_dup_rebuild");
        bt.insert_duplicate(Value::Int(5), stat_rid(0, 10));
        bt.insert_duplicate(Value::Int(5), stat_rid(0, 20));
        // Now insert an out-of-order (smaller) rid for the same key.
        bt.insert_duplicate(Value::Int(5), stat_rid(0, 1));
        bt.insert_duplicate(Value::Int(6), stat_rid(0, 30));

        assert_eq!(bt.stats().total_entries, 4);
        assert_eq!(bt.stats().distinct_keys, 2);

        // Ground truth: reload from a save recomputes independently.
        let path =
            std::env::temp_dir().join(format!("powdb_dup_rebuild_gt_{}", std::process::id()));
        bt.save_to(&path).unwrap();
        let reloaded = BTree::load(&path).unwrap();
        assert_eq!(reloaded.stats().total_entries, bt.stats().total_entries);
        assert_eq!(reloaded.stats().distinct_keys, bt.stats().distinct_keys);
        let _ = std::fs::remove_file(&path);
    }

    // ─── NUL-safe composite keys (v0.16 Workstream 1) ─────────────────────

    /// The three-key repro from the plan: an embedded-NUL value must not
    /// interleave with the keys of a shorter plain value. Before the escape
    /// fix, `lookup_prefix("A")` returned the "A\0" rid too, and
    /// `lookup_prefix("A\0")` returned all three.
    #[test]
    fn lookup_prefix_embedded_nul_disjoint() {
        let mut bt = temp_btree_v2("nul_lookup_prefix");
        bt.key_kind = KeyKind::Composite;
        bt.insert_non_unique(Value::Str("A".into()), stat_rid(0, 5));
        bt.insert_non_unique(Value::Str("A\0".into()), stat_rid(0, 3));
        bt.insert_non_unique(Value::Str("A".into()), stat_rid(0, 7));

        let mut a = bt.lookup_prefix(&Value::Str("A".into()));
        a.sort_by_key(|r| r.slot_index);
        assert_eq!(
            a,
            vec![stat_rid(0, 5), stat_rid(0, 7)],
            "\"A\" must not see \"A\\0\""
        );

        let a_nul = bt.lookup_prefix(&Value::Str("A\0".into()));
        assert_eq!(
            a_nul,
            vec![stat_rid(0, 3)],
            "\"A\\0\" must see only its own rid"
        );

        // Same guarantee after a save/load round trip (v3 file).
        bt.save().unwrap();
        let mut reloaded = BTree::load(bt.file_path()).unwrap();
        reloaded.mark_composite();
        let mut a2 = reloaded.lookup_prefix(&Value::Str("A".into()));
        a2.sort_by_key(|r| r.slot_index);
        assert_eq!(a2, vec![stat_rid(0, 5), stat_rid(0, 7)]);
        assert_eq!(
            reloaded.lookup_prefix(&Value::Str("A\0".into())),
            vec![stat_rid(0, 3)]
        );
        assert_eq!(reloaded.format_version(), BTREE_VERSION, "v3 file on disk");
        cleanup(&bt);
    }

    /// The escape encoding must be prefix-free and order-preserving over
    /// arbitrary byte content: NUL at start/middle/end, repeated NUL, 0xFF
    /// adjacency, and the empty string. Prefix-freeness is what makes each
    /// value's composite-key range disjoint once the raw rid is appended;
    /// order-preservation is what keeps Str range scans correct.
    #[test]
    fn escape_encoding_prefix_free_and_ordered() {
        let samples: Vec<Vec<u8>> = vec![
            vec![],
            vec![0],
            vec![0, 0],
            vec![0x41],
            vec![0x41, 0],
            vec![0, 0x41],
            vec![0x41, 0, 0x42],
            vec![0x41, 0x42],
            vec![0x42],
            vec![0xFF],
            vec![0x41, 0xFF],
            vec![0, 0xFF],
            vec![0xFF, 0],
            vec![0x41, 0, 0xFF],
            vec![0xFF, 0xFF],
        ];
        let enc = |s: &[u8]| {
            let mut b = Vec::new();
            encode_str_escaped(&mut b, s);
            b
        };
        for a in &samples {
            for b in &samples {
                let (ea, eb) = (enc(a), enc(b));
                assert_eq!(
                    ea.cmp(&eb),
                    a.cmp(b),
                    "order must be preserved for {a:?} vs {b:?}"
                );
                if a == b {
                    assert_eq!(ea, eb);
                } else {
                    assert!(
                        !ea.starts_with(&eb) && !eb.starts_with(&ea),
                        "encoding must be prefix-free for {a:?} vs {b:?}"
                    );
                }
            }
        }
    }

    /// Prefix-freeness turned into the property that actually matters: no
    /// value's escaped encoding plus any rid can land inside another value's
    /// [rid_min, rid_max] composite range. The rid's top byte can be 0xFF (the
    /// original bug's bridge), so probe that corner explicitly.
    #[test]
    fn escape_ranges_disjoint_under_any_rid() {
        let values = [
            Value::Str("A".into()),
            Value::Str("A\0".into()),
            Value::Str("A\0\0".into()),
            Value::Str("".into()),
            Value::Str("\0".into()),
            Value::Str("B".into()),
        ];
        // A rid whose big-endian bytes start with 0xFF (page_id high bits set).
        let hot_rid = RowId {
            page_id: 0xFFFF_FFFF,
            slot_index: 0xFFFF,
        };
        for target in &values {
            let lo = BTree::make_prefix_start(target);
            let hi = BTree::make_prefix_end(target);
            for other in &values {
                for rid in [stat_rid(0, 0), hot_rid] {
                    let key = BTree::make_composite_key(other, rid);
                    let in_range = key >= lo && key <= hi;
                    assert_eq!(
                        in_range,
                        other == target,
                        "{other:?} (rid {rid:?}) in range of {target:?}?"
                    );
                }
            }
        }
    }

    /// Twin-engine parity: a non-unique index over mixed NUL/plain strings must
    /// return exactly the ground-truth rids for every value, through inserts,
    /// deletes, and delete-then-reinsert (the shape index-driven updates take).
    #[test]
    fn nonunique_twin_parity_mixed_nul() {
        use std::collections::BTreeMap;

        let mut bt = temp_btree_v2("twin_nul");
        bt.key_kind = KeyKind::Composite;
        // Ground truth: value bytes -> set of rids.
        let mut truth: BTreeMap<Vec<u8>, Vec<RowId>> = BTreeMap::new();

        let values = ["A", "A\0", "A\0\0", "", "\0", "AB", "A\0B", "B"];
        let mut n = 0u16;
        for round in 0..4u16 {
            for v in values {
                let rid = stat_rid(round as u32, n);
                n = n.wrapping_add(1);
                bt.insert_non_unique(Value::Str(v.into()), rid);
                truth.entry(v.as_bytes().to_vec()).or_default().push(rid);
            }
        }

        let check = |bt: &BTree, truth: &BTreeMap<Vec<u8>, Vec<RowId>>| {
            for (bytes, expected) in truth {
                let v = Value::Str(String::from_utf8(bytes.clone()).unwrap());
                let mut got = bt.lookup_prefix(&v);
                got.sort_by_key(|r| (r.page_id, r.slot_index));
                let mut want = expected.clone();
                want.sort_by_key(|r| (r.page_id, r.slot_index));
                assert_eq!(got, want, "lookup parity for {bytes:?}");
            }
        };
        check(&bt, &truth);

        // Delete every "A\0" rid (index-driven delete): "A" must be untouched.
        let a_nul = truth.remove("A\0".as_bytes()).unwrap();
        for rid in &a_nul {
            assert!(bt.delete_non_unique(&Value::Str("A\0".into()), *rid));
        }
        truth.insert(b"A\0".to_vec(), Vec::new());
        check(&bt, &truth);

        // Delete-then-reinsert an "A" rid at a fresh rid (update shape).
        let moved = truth.get_mut(b"A".as_slice()).unwrap().pop().unwrap();
        assert!(bt.delete_non_unique(&Value::Str("A".into()), moved));
        let new_rid = stat_rid(99, 1);
        bt.insert_non_unique(Value::Str("A".into()), new_rid);
        truth.get_mut(b"A".as_slice()).unwrap().push(new_rid);
        check(&bt, &truth);

        // Distinct-key stats: 7 remaining distinct values ("A\0" now empty but
        // still a key... its rids are gone, so it drops out).
        bt.save().unwrap();
        let live: u64 = truth.values().filter(|rids| !rids.is_empty()).count() as u64;
        assert_eq!(bt.stats().distinct_keys, live, "distinct == live values");
        cleanup(&bt);
    }
}
