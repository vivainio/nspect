//! On-disk cache for tree-sitter `.cs` parses.
//!
//! Keyed by repo-root-relative path + (mtime_ns, len). Stored as a single
//! bincode blob so a fresh run pays one fopen instead of one-per-file (slow
//! on Windows/WSL filesystems). `CACHE_VERSION` invalidates the entire file
//! whenever the cached struct shape changes — readers that find a mismatch
//! return an empty cache rather than risk decoding stale layouts.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::source_scan::FileDecls;

/// Bump whenever `FileDecls` (or anything it transitively encodes) changes
/// shape. A mismatch causes the cache to be discarded on load.
const CACHE_VERSION: u32 = 3;

/// Magic bytes to spot truncated / wrong-format files cheaply.
const CACHE_MAGIC: [u8; 4] = *b"NSPC";

#[derive(Debug, Default, bincode::Encode, bincode::Decode)]
pub struct Cache {
    /// Path is stored as a string (repo-root-relative). Using `String`
    /// instead of `PathBuf` keeps the encoding portable and avoids the
    /// platform-specific `OsString` shape.
    entries: HashMap<String, Entry>,
}

#[derive(Debug, Clone, bincode::Encode, bincode::Decode)]
struct Entry {
    mtime_ns: i128,
    len: u64,
    decls: FileDecls,
}

impl Cache {
    /// Look up a file by canonical repo-relative path. Returns `Some(decls)`
    /// only when the cached entry's mtime+len match the live file's.
    pub fn get(&self, key: &str, mtime_ns: i128, len: u64) -> Option<&FileDecls> {
        let e = self.entries.get(key)?;
        (e.mtime_ns == mtime_ns && e.len == len).then_some(&e.decls)
    }

    pub fn insert(&mut self, key: String, mtime_ns: i128, len: u64, decls: FileDecls) {
        self.entries.insert(
            key,
            Entry {
                mtime_ns,
                len,
                decls,
            },
        );
    }

    /// Drop entries whose paths weren't touched by this run. Pass the set of
    /// keys observed during the scan; everything else is evicted so the
    /// cache doesn't grow unbounded as files are deleted/renamed.
    pub fn retain_keys(&mut self, live: &std::collections::HashSet<String>) {
        self.entries.retain(|k, _| live.contains(k));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Read a cache file. Missing file or version mismatch both return an empty
/// cache — caller treats those identically.
pub fn load(path: &Path) -> Cache {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return Cache::default(),
    };
    if bytes.len() < 8 || bytes[..4] != CACHE_MAGIC {
        return Cache::default();
    }
    let version = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if version != CACHE_VERSION {
        return Cache::default();
    }
    let body = &bytes[8..];
    match bincode::decode_from_slice::<Cache, _>(body, bincode::config::standard()) {
        Ok((c, _)) => c,
        Err(e) => {
            tracing::warn!("source-scan cache decode failed; starting fresh: {e}");
            Cache::default()
        }
    }
}

pub fn save(path: &Path, cache: &Cache) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating cache dir {}", parent.display()))?;
    }
    let body = bincode::encode_to_vec(cache, bincode::config::standard())
        .context("encoding source-scan cache")?;
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&CACHE_MAGIC);
    out.extend_from_slice(&CACHE_VERSION.to_le_bytes());
    out.extend_from_slice(&body);
    std::fs::write(path, &out).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Default cache file location for a given scan root.
pub fn default_path(scan_root: &Path) -> PathBuf {
    scan_root.join(".nspect").join("cache").join("source_scan.bin")
}

/// Read the (mtime_ns, len) stamp the cache compares against. `None` if the
/// file's metadata can't be read or its mtime can't be expressed as nanos.
pub fn stamp(path: &Path) -> Option<(i128, u64)> {
    let md = std::fs::metadata(path).ok()?;
    let len = md.len();
    let mtime = md.modified().ok()?;
    let dur = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos() as i128;
    Some((dur, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("nspect-cache-test-{pid}-{nanos}-{name}"));
        p
    }

    #[test]
    fn roundtrips_empty_cache() {
        let p = tmp_path("empty.bin");
        save(&p, &Cache::default()).unwrap();
        let c = load(&p);
        assert_eq!(c.len(), 0);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn missing_file_yields_empty_cache() {
        let c = load(Path::new("/no/such/path/here.bin"));
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn version_mismatch_discards_cache() {
        let p = tmp_path("ver.bin");
        // Hand-build a header with the wrong version.
        let mut out = Vec::new();
        out.extend_from_slice(&CACHE_MAGIC);
        out.extend_from_slice(&(CACHE_VERSION.wrapping_add(1)).to_le_bytes());
        out.extend_from_slice(&[0u8; 16]);
        std::fs::write(&p, &out).unwrap();
        assert_eq!(load(&p).len(), 0);
    }

    #[test]
    fn stamp_mismatch_misses() {
        let mut c = Cache::default();
        c.insert("Foo.cs".into(), 100, 50, FileDecls::default());
        assert!(c.get("Foo.cs", 100, 50).is_some());
        assert!(c.get("Foo.cs", 101, 50).is_none());
        assert!(c.get("Foo.cs", 100, 51).is_none());
    }
}
