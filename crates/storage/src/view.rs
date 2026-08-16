use rustc_hash::FxHashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const VIEW_FILE: &str = "views.bin";
const VIEW_MAGIC: &[u8; 4] = b"BVIW";
const VIEW_VERSION: u16 = 1;

/// Definition of a materialized view.
#[derive(Debug, Clone)]
pub struct ViewDef {
    /// View name (used as the backing table name too).
    pub name: String,
    /// Source PowQL query text. Re-executed on refresh.
    pub query: String,
    /// Tables this view depends on. Mutations to any of these mark the view
    /// dirty.
    pub depends_on: Vec<String>,
    /// Whether the cached result set is stale.
    pub dirty: bool,
}

/// Registry of all materialized views. Lives alongside the `Catalog` in the
/// `Engine` struct. Provides dirty-tracking and persistence.
pub struct ViewRegistry {
    views: FxHashMap<String, ViewDef>,
    /// Reverse index: base table name → list of view names that depend on it.
    /// Maintained in sync with `views` on every register/unregister.
    deps: FxHashMap<String, Vec<String>>,
    data_dir: PathBuf,
}

impl ViewRegistry {
    /// Create a new empty registry.
    pub fn new(data_dir: &Path) -> Self {
        ViewRegistry {
            views: FxHashMap::default(),
            deps: FxHashMap::default(),
            data_dir: data_dir.to_path_buf(),
        }
    }

    /// Open an existing registry from disk, or return an empty one if no
    /// views file exists.
    pub fn open(data_dir: &Path) -> io::Result<Self> {
        let path = data_dir.join(VIEW_FILE);
        if !path.exists() {
            return Ok(Self::new(data_dir));
        }
        let defs = read_view_file(&path)?;
        let mut reg = Self::new(data_dir);
        for def in defs {
            reg.insert_def(def);
        }
        Ok(reg)
    }

    /// Register a new view. Does NOT create the backing table or run the
    /// query — the executor handles that.
    pub fn register(&mut self, def: ViewDef) -> io::Result<()> {
        if self.views.contains_key(&def.name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("view '{}' already exists", def.name),
            ));
        }
        self.insert_def(def);
        self.persist()
    }

    /// Remove a view from the registry. Does NOT drop the backing table.
    pub fn unregister(&mut self, name: &str) -> io::Result<()> {
        let def = self.views.remove(name).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("view '{name}' not found"))
        })?;
        for table in &def.depends_on {
            if let Some(list) = self.deps.get_mut(table) {
                list.retain(|v| v != name);
                if list.is_empty() {
                    self.deps.remove(table);
                }
            }
        }
        self.persist()
    }

    /// Look up a view by name.
    pub fn get(&self, name: &str) -> Option<&ViewDef> {
        self.views.get(name)
    }

    /// Check whether `name` is a registered materialized view.
    #[inline]
    pub fn is_view(&self, name: &str) -> bool {
        self.views.contains_key(name)
    }

    /// Mark a view as dirty (needs refresh before next read), writing the flag
    /// through to `views.bin`.
    ///
    /// The flag has to reach disk or it does not exist: it lived in memory
    /// only, so mutating a base table and then exiting the process lost it,
    /// and the view came back CLEAN over pre-mutation rows and served them
    /// forever. Nothing after the restart could notice — the flag is the only
    /// record that a refresh is owed.
    ///
    /// Only the `false` → `true` transition writes. Every subsequent mutation
    /// finds the flag already set and costs nothing, so the price is at most
    /// one small write per refresh cycle rather than one per write statement.
    ///
    /// On a write failure the in-memory flag stays dirty — the safe value —
    /// and the error is returned. Callers must not treat it as success: a view
    /// whose dirty flag was not recorded is exactly the bug above.
    pub fn mark_dirty(&mut self, view_name: &str) -> io::Result<()> {
        let changed = match self.views.get_mut(view_name) {
            Some(def) if !def.dirty => {
                def.dirty = true;
                true
            }
            _ => false,
        };
        if changed {
            self.persist()
        } else {
            Ok(())
        }
    }

    /// Set the dirty flag in memory only, leaving `views.bin` untouched.
    ///
    /// For callers whose whole contract is that they do not modify the
    /// database: engine open marks a view with an unparseable source dirty so
    /// reads report that error instead of serving unrecomputable rows, and a
    /// read-only open does that over a directory it must leave byte-identical
    /// (and which may sit on read-only storage). The state is rederived on
    /// every open, so nothing is lost by not persisting it.
    pub fn mark_dirty_in_memory(&mut self, view_name: &str) {
        if let Some(def) = self.views.get_mut(view_name) {
            def.dirty = true;
        }
    }

    /// Mark a view as clean after a successful refresh, writing the flag
    /// through to `views.bin`.
    ///
    /// The caller must have made the refreshed contents durable first. This
    /// records "the stored rows are current"; if the rows it refers to are
    /// still only buffered, a crash in between leaves a clean flag over stale
    /// rows, which is the wrong-answer direction.
    ///
    /// On a write failure the in-memory flag is put back to dirty so it agrees
    /// with what is still on disk, and the error is returned: an extra refresh
    /// is cheap, a silently stale view is not.
    pub fn mark_clean(&mut self, view_name: &str) -> io::Result<()> {
        let changed = match self.views.get_mut(view_name) {
            Some(def) if def.dirty => {
                def.dirty = false;
                true
            }
            _ => false,
        };
        if !changed {
            return Ok(());
        }
        if let Err(e) = self.persist() {
            if let Some(def) = self.views.get_mut(view_name) {
                def.dirty = true;
            }
            return Err(e);
        }
        Ok(())
    }

    /// Clear the dirty flag in memory only, leaving `views.bin` untouched.
    ///
    /// For a refresh inside an explicit transaction, where there is nothing to
    /// commit yet and so no point at which the refreshed rows are known to be
    /// durable. Leaving the on-disk flag dirty costs one redundant refresh
    /// after the next open and can never serve a stale row.
    pub fn mark_clean_in_memory(&mut self, view_name: &str) {
        if let Some(def) = self.views.get_mut(view_name) {
            def.dirty = false;
        }
    }

    /// Check whether a view needs refresh.
    #[inline]
    pub fn is_dirty(&self, view_name: &str) -> bool {
        self.views.get(view_name).is_some_and(|d| d.dirty)
    }

    /// Mark all views that depend on `table` as dirty, writing the flags
    /// through to `views.bin`. Called by the executor on INSERT/UPDATE/DELETE
    /// on a base table.
    ///
    /// Returns immediately (no-op) when no views exist or no views depend
    /// on the given table — the hot path for tables with no dependents is
    /// a single `FxHashMap::get` returning `None`.
    ///
    /// This runs on every write to a table that does have views, so it must
    /// not put a file write in the write path. It does not: only a view that
    /// is currently clean is a transition, and after the first mutation
    /// following a refresh there are none, so the steady state is a hash
    /// lookup and a scan of a short name list with no allocation and no I/O.
    /// The write happens at most once per refresh cycle.
    ///
    /// Propagation is **transitive**. A view is itself a valid source for
    /// another view, so `V2 as V1` records `V1` in its `depends_on` and the
    /// one-level walk this used to do never reached it: mutating the base
    /// table marked `V1` dirty and left `V2` clean over `V1`'s pre-mutation
    /// contents, permanently, because nothing else ever revisits a clean view.
    /// Only a clean-to-dirty transition is enqueued, so an already-dirty layer
    /// stops the walk and the steady-state cost is unchanged.
    ///
    /// See [`ViewRegistry::mark_dirty`] for why the flag has to be on disk
    /// at all, and for the failure contract.
    #[inline]
    pub fn mark_dependents_dirty(&mut self, table: &str) -> io::Result<()> {
        // Fast exit before anything is cloned: no dependent of `table` is
        // clean, so there is no transition here and none downstream either
        // (a dirty view's own dependents were marked when it went dirty).
        let any_clean = self.deps.get(table).is_some_and(|names| {
            names
                .iter()
                .any(|n| self.views.get(n.as_str()).is_some_and(|d| !d.dirty))
        });
        if !any_clean {
            return Ok(());
        }

        // Walk the dependency graph breadth-first, enqueuing only the views
        // this call actually transitions. `dirty` doubles as the visited set,
        // so a dependency cycle terminates on the second visit instead of
        // looping.
        let mut queue: Vec<String> = vec![table.to_string()];
        while let Some(source) = queue.pop() {
            let names: Vec<String> = self.deps.get(source.as_str()).cloned().unwrap_or_default();
            for name in names {
                match self.views.get_mut(name.as_str()) {
                    Some(def) if !def.dirty => {
                        def.dirty = true;
                        queue.push(name);
                    }
                    _ => {}
                }
            }
        }
        self.persist()
    }

    /// Every view that depends on `source`, directly or through another view.
    ///
    /// Sorted, so a caller that puts these in a message gets a stable order
    /// rather than whatever the hash map happened to yield.
    pub fn dependents_of(&self, source: &str) -> Vec<String> {
        let mut found: Vec<String> = Vec::new();
        let mut queue: Vec<String> = vec![source.to_string()];
        while let Some(current) = queue.pop() {
            for name in self.deps.get(current.as_str()).into_iter().flatten() {
                if !found.iter().any(|seen| seen == name) {
                    found.push(name.clone());
                    queue.push(name.clone());
                }
            }
        }
        found.sort();
        found
    }

    /// List all view names.
    pub fn list_views(&self) -> Vec<&str> {
        self.views.keys().map(|k| k.as_str()).collect()
    }

    // ─── Internal ────────────────────────────────────────────────

    fn insert_def(&mut self, def: ViewDef) {
        let name = def.name.clone();
        for table in &def.depends_on {
            self.deps
                .entry(table.clone())
                .or_default()
                .push(name.clone());
        }
        self.views.insert(name, def);
    }

    /// Durable write-temp-then-rename, matching `catalog.rs`.
    ///
    /// The directory fsync after the rename is load-bearing, not ceremony: the
    /// whole point of persisting the dirty flag is that it survives a crash, and
    /// on ext4/xfs a rename can be reordered past a power loss even though the
    /// temp file's own contents were synced. Without it the flag could be
    /// written, acknowledged, and then not be there on the next open, which is
    /// exactly the stale-view bug this file exists to prevent.
    fn persist(&self) -> io::Result<()> {
        let path = self.data_dir.join(VIEW_FILE);
        let tmp = self.data_dir.join(format!("{VIEW_FILE}.tmp"));
        let defs: Vec<&ViewDef> = self.views.values().collect();
        write_view_file(&tmp, &defs)?;
        fs::rename(&tmp, &path)?;
        crate::catalog::sync_directory(&self.data_dir)?;
        Ok(())
    }
}

// ─── Binary format ──────────────────────────────────────────────────────
//
// Layout:
//   magic       [4]    = "BVIW"
//   version     u16    = 1
//   n_views     u32
//   for each view:
//     name_len    u32
//     name        utf8
//     query_len   u32
//     query       utf8
//     n_deps      u16
//     for each dep:
//       dep_len   u32
//       dep_name  utf8
//     dirty       u8

fn write_view_file(path: &Path, defs: &[&ViewDef]) -> io::Result<()> {
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    buf.extend_from_slice(VIEW_MAGIC);
    buf.extend_from_slice(&VIEW_VERSION.to_le_bytes());
    buf.extend_from_slice(&(defs.len() as u32).to_le_bytes());

    for def in defs {
        let name = def.name.as_bytes();
        buf.extend_from_slice(&(name.len() as u32).to_le_bytes());
        buf.extend_from_slice(name);

        let query = def.query.as_bytes();
        buf.extend_from_slice(&(query.len() as u32).to_le_bytes());
        buf.extend_from_slice(query);

        buf.extend_from_slice(&(def.depends_on.len() as u16).to_le_bytes());
        for dep in &def.depends_on {
            let d = dep.as_bytes();
            buf.extend_from_slice(&(d.len() as u32).to_le_bytes());
            buf.extend_from_slice(d);
        }

        buf.push(if def.dirty { 1 } else { 0 });
    }

    let mut f = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    f.write_all(&buf)?;
    f.sync_data()?;
    Ok(())
}

fn read_view_file(path: &Path) -> io::Result<Vec<ViewDef>> {
    let mut f = fs::File::open(path)?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;

    let mut pos = 0usize;
    if buf.len() < 10 || &buf[0..4] != VIEW_MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad view magic"));
    }
    pos += 4;
    let version = u16::from_le_bytes(
        buf[pos..pos + 2]
            .try_into()
            .expect("invariant: buf.len() >= 10 checked above covers the 2-byte version field"),
    );
    pos += 2;
    if version != VIEW_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported view version: {version}"),
        ));
    }
    let n_views = u32::from_le_bytes(
        buf[pos..pos + 4]
            .try_into()
            .expect("invariant: buf.len() >= 10 checked above covers the 4-byte n_views field"),
    ) as usize;
    pos += 4;

    let mut defs = Vec::with_capacity(n_views);
    for _ in 0..n_views {
        let name = read_str(&buf, &mut pos)?;
        let query = read_str(&buf, &mut pos)?;

        let n_deps = read_u16(&buf, &mut pos)? as usize;
        let mut depends_on = Vec::with_capacity(n_deps);
        for _ in 0..n_deps {
            depends_on.push(read_str(&buf, &mut pos)?);
        }

        let dirty = read_u8(&buf, &mut pos)? != 0;
        defs.push(ViewDef {
            name,
            query,
            depends_on,
            dirty,
        });
    }
    Ok(defs)
}

fn read_u8(buf: &[u8], pos: &mut usize) -> io::Result<u8> {
    if *pos >= buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated view file",
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
            "truncated view file",
        ));
    }
    let v = u16::from_le_bytes(
        buf[*pos..*pos + 2]
            .try_into()
            .expect("invariant: bounds checked immediately above (*pos + 2 <= buf.len())"),
    );
    *pos += 2;
    Ok(v)
}

fn read_str(buf: &[u8], pos: &mut usize) -> io::Result<String> {
    if *pos + 4 > buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated view file",
        ));
    }
    let len = u32::from_le_bytes(
        buf[*pos..*pos + 4]
            .try_into()
            .expect("invariant: bounds checked immediately above (*pos + 4 <= buf.len())"),
    ) as usize;
    *pos += 4;
    if *pos + len > buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated view file",
        ));
    }
    let s = std::str::from_utf8(&buf[*pos..*pos + len])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 in view file"))?
        .to_string();
    *pos += len;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_registry(name: &str) -> ViewRegistry {
        let dir = std::env::temp_dir().join(format!("powdb_view_{name}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        ViewRegistry::new(&dir)
    }

    #[test]
    fn test_read_view_file_garbage_errors_not_panic() {
        // A truncated or corrupt on-disk view file (read during DB open) must
        // surface an io::Error, never panic the server.
        let dir = std::env::temp_dir().join(format!("powdb_view_garbage_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let cases: Vec<Vec<u8>> = vec![
            vec![],                                   // empty
            b"BVIW".to_vec(),                         // magic only, no version/count
            b"XXXX\x01\x00\x00\x00\x00\x00".to_vec(), // wrong magic, full header len
            // valid header claiming 1 view but no view bytes follow
            {
                let mut v = b"BVIW".to_vec();
                v.extend_from_slice(&VIEW_VERSION.to_le_bytes());
                v.extend_from_slice(&1u32.to_le_bytes());
                v
            },
            // valid header, view name claims a 0xFFFFFFFF-byte string
            {
                let mut v = b"BVIW".to_vec();
                v.extend_from_slice(&VIEW_VERSION.to_le_bytes());
                v.extend_from_slice(&1u32.to_le_bytes());
                v.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
                v
            },
        ];

        for (i, bytes) in cases.iter().enumerate() {
            let p = dir.join(format!("v{i}.bin"));
            std::fs::write(&p, bytes).unwrap();
            let result = read_view_file(&p);
            assert!(
                result.is_err(),
                "expected Err for garbage view file case {i} ({bytes:?}), got Ok"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_register_and_lookup() {
        let mut reg = temp_registry("basic");
        reg.register(ViewDef {
            name: "ActiveUsers".into(),
            query: "User filter .active = true".into(),
            depends_on: vec!["User".into()],
            dirty: false,
        })
        .unwrap();
        assert!(reg.is_view("ActiveUsers"));
        assert!(!reg.is_view("User"));
        let def = reg.get("ActiveUsers").unwrap();
        assert_eq!(def.query, "User filter .active = true");
    }

    #[test]
    fn test_dirty_tracking() {
        let mut reg = temp_registry("dirty");
        reg.register(ViewDef {
            name: "V1".into(),
            query: "T1".into(),
            depends_on: vec!["T1".into()],
            dirty: false,
        })
        .unwrap();
        assert!(!reg.is_dirty("V1"));
        reg.mark_dependents_dirty("T1").unwrap();
        assert!(reg.is_dirty("V1"));
        reg.mark_clean("V1").unwrap();
        assert!(!reg.is_dirty("V1"));
    }

    #[test]
    fn test_dirty_flag_survives_reopen() {
        // The dirty flag used to live in memory only: a mutation marked the
        // view dirty, the process exited, and the reopened registry reported
        // the view CLEAN, so every later read served pre-mutation rows with
        // no refresh and no error. Before the fix this asserted `false` after
        // the first reopen.
        let dir = std::env::temp_dir().join(format!("powdb_view_dirty_rt_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        {
            let mut reg = ViewRegistry::new(&dir);
            reg.register(ViewDef {
                name: "V1".into(),
                query: "T1".into(),
                depends_on: vec!["T1".into()],
                dirty: false,
            })
            .unwrap();
            reg.mark_dependents_dirty("T1").unwrap();
        }
        {
            let mut reg = ViewRegistry::open(&dir).unwrap();
            assert!(reg.is_dirty("V1"), "dirty flag must survive process exit");
            reg.mark_clean("V1").unwrap();
        }
        // Clean is equally durable, or every restart would refresh forever.
        let reg = ViewRegistry::open(&dir).unwrap();
        assert!(!reg.is_dirty("V1"), "clean flag must survive process exit");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_mark_dependents_dirty_is_idempotent_after_first_transition() {
        // The write path calls this on every mutation, so only the
        // false -> true transition may touch the file. Re-marking an already
        // dirty view must not rewrite it.
        let dir =
            std::env::temp_dir().join(format!("powdb_view_transition_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let mut reg = ViewRegistry::new(&dir);
        reg.register(ViewDef {
            name: "V1".into(),
            query: "T1".into(),
            depends_on: vec!["T1".into()],
            dirty: false,
        })
        .unwrap();
        reg.mark_dependents_dirty("T1").unwrap();
        let path = dir.join(VIEW_FILE);
        assert!(path.exists(), "the transition itself must write the file");
        // Removing the file makes a subsequent write unmissable and needs no
        // mtime resolution or inode plumbing to detect: if any of the repeats
        // below persists, the file comes back.
        std::fs::remove_file(&path).unwrap();
        for _ in 0..100 {
            reg.mark_dependents_dirty("T1").unwrap();
        }
        assert!(
            !path.exists(),
            "re-marking an already dirty view must not rewrite views.bin"
        );
        // The same guard on the clean side: only the true -> false transition
        // writes.
        reg.mark_clean("V1").unwrap();
        assert!(path.exists());
        std::fs::remove_file(&path).unwrap();
        for _ in 0..100 {
            reg.mark_clean("V1").unwrap();
        }
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_multi_dependency() {
        let mut reg = temp_registry("multi");
        reg.register(ViewDef {
            name: "V1".into(),
            query: "T1 inner join T2 on .id = .fk".into(),
            depends_on: vec!["T1".into(), "T2".into()],
            dirty: false,
        })
        .unwrap();
        // Mutating either dependency dirties the view
        reg.mark_dependents_dirty("T2").unwrap();
        assert!(reg.is_dirty("V1"));
    }

    #[test]
    fn test_unregister() {
        let mut reg = temp_registry("unreg");
        reg.register(ViewDef {
            name: "V1".into(),
            query: "T1".into(),
            depends_on: vec!["T1".into()],
            dirty: false,
        })
        .unwrap();
        reg.unregister("V1").unwrap();
        assert!(!reg.is_view("V1"));
        // Dependency map is cleaned up — marking T1 dirty doesn't panic
        reg.mark_dependents_dirty("T1").unwrap();
    }

    #[test]
    fn test_persist_and_reopen() {
        let dir = std::env::temp_dir().join(format!("powdb_view_persist_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        {
            let mut reg = ViewRegistry::new(&dir);
            reg.register(ViewDef {
                name: "V1".into(),
                query: "User filter .active = true".into(),
                depends_on: vec!["User".into()],
                dirty: true,
            })
            .unwrap();
        }
        // Reopen
        let reg = ViewRegistry::open(&dir).unwrap();
        assert!(reg.is_view("V1"));
        let def = reg.get("V1").unwrap();
        assert_eq!(def.query, "User filter .active = true");
        assert!(def.dirty);
        assert_eq!(def.depends_on, vec!["User".to_string()]);
    }
}
