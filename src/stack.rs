//! A newest-wins stack of seg files spanning a step range.
//!
//! Erigon keeps a domain's state across several files covering successive *step* ranges
//! (e.g. `v1.1-accounts.0-1024.kv`, `v1.1-accounts.1024-2048.kv`, …). A newer file may
//! override a key carried by an older one, so a point lookup must consult the files
//! newest-first and return the first hit. [`KvStack`] wraps an ordered set of
//! [`KvReader`]s and implements exactly that semantics, resolving the `.kvei` bloom salt
//! once and enabling each file's filter against it.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::reader::KvReader;
use crate::salt::Salt;

/// A stack of seg files for one domain, queried newest-first so overrides win.
///
/// The salt is resolved once at [`open`](KvStack::open) time (brute-forced from the
/// oldest file when [`Salt::Find`] is used) and then applied to every file's bloom, so a
/// wrong or absent salt only disables the negative-lookup speedup — it can never cause a
/// missed key.
pub struct KvStack {
    /// Ordered oldest → newest; queried in reverse so the newest match wins.
    readers: Vec<KvReader>,
    /// The resolved salt, if one was supplied or found.
    salt: Option<u32>,
}

impl KvStack {
    /// Open an explicit set of `.kv` files as a newest-wins stack. The files are sorted
    /// oldest → newest by their `<from>-<to>` step range (parsed from the file name), so
    /// the caller need not pre-sort them. Errors if `paths` is empty or any file fails to
    /// open.
    ///
    /// `salt` controls the `.kvei` bloom accelerator (see [`Salt`]): the salt is resolved
    /// once (brute-forced from the oldest file for [`Salt::Find`]) and each file's bloom
    /// is enabled only if it self-validates against that file's real keys.
    pub fn open<I, P>(paths: I, salt: Salt) -> Result<KvStack>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut paths: Vec<PathBuf> = paths
            .into_iter()
            .map(|p| p.as_ref().to_path_buf())
            .collect();
        if paths.is_empty() {
            return Err(Error::format("KvStack::open: no .kv files supplied"));
        }
        paths.sort_by_key(|p| step_key(p));
        let mut readers = Vec::with_capacity(paths.len());
        for p in &paths {
            readers.push(KvReader::open(p)?);
        }
        let salt = resolve_and_enable(&mut readers, salt);
        Ok(KvStack { readers, salt })
    }

    /// Open every `.kv` in `dir` whose file name contains `name_filter`, as a newest-wins
    /// stack. Use `name_filter` to select a single domain (e.g. `"accounts"`) so files
    /// from different domains in the same directory are not mixed. Errors if no matching
    /// `.kv` file is found.
    pub fn open_dir(dir: impl AsRef<Path>, name_filter: &str, salt: Salt) -> Result<KvStack> {
        let dir = dir.as_ref();
        let mut kvs: Vec<PathBuf> = std::fs::read_dir(dir)
            .map_err(|e| Error::format(format!("read_dir {}: {e}", dir.display())))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                if p.extension().is_none_or(|x| x != "kv") {
                    return false;
                }
                p.file_name()
                    .map(|s| s.to_string_lossy().contains(name_filter))
                    .unwrap_or(false)
            })
            .collect();
        if kvs.is_empty() {
            return Err(Error::format(format!(
                "no .kv files matching {name_filter:?} in {}",
                dir.display()
            )));
        }
        kvs.sort_by_key(|p| step_key(p));
        KvStack::open(kvs, salt)
    }

    /// The resolved bloom salt, if one was supplied or found.
    pub fn salt(&self) -> Option<u32> {
        self.salt
    }

    /// Number of files with an active (validated) bloom accelerator.
    pub fn bloom_count(&self) -> usize {
        self.readers.iter().filter(|r| r.bloom_active()).count()
    }

    /// Number of files in the stack.
    pub fn len(&self) -> usize {
        self.readers.len()
    }

    /// Whether the stack has no files. (Never true for a stack from [`open`](KvStack::open),
    /// which rejects an empty input.)
    pub fn is_empty(&self) -> bool {
        self.readers.is_empty()
    }

    /// The readers, oldest → newest.
    pub fn readers(&self) -> &[KvReader] {
        &self.readers
    }

    /// Iterate `(file name, key count)` for each file, oldest → newest.
    pub fn files(&self) -> impl Iterator<Item = (&str, u64)> {
        self.readers.iter().map(|r| (r.name(), r.key_count()))
    }

    /// Look up `key` across all files, newest-first; returns the value from the newest
    /// file that contains it (overrides win), or `None` if no file has it.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        for r in self.readers.iter().rev() {
            if let Some(v) = r.get(key)? {
                return Ok(Some(v));
            }
        }
        Ok(None)
    }
}

/// Resolve the salt once (brute-forcing from the oldest file for [`Salt::Find`]) and
/// enable each file's bloom against it. Returns the resolved salt, if any.
fn resolve_and_enable(readers: &mut [KvReader], salt: Salt) -> Option<u32> {
    let resolved = match salt {
        Salt::None => None,
        Salt::Known(s) => Some(s),
        Salt::Find(threads) => readers.first().and_then(|r| r.find_salt(threads)),
    };
    if let Some(s) = resolved {
        for r in readers.iter_mut() {
            r.enable_bloom(Salt::Known(s));
        }
    }
    resolved
}

/// Sort key from a `…<name>.<from>-<to>.kv` file name: the `<from>` step (then `<to>`),
/// so files order oldest → newest (override files have a higher `<from>`). Names without
/// a recognizable range sort first.
fn step_key(p: &Path) -> (u64, u64) {
    let name = p
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    for seg in name.split('.') {
        if let Some((a, b)) = seg.split_once('-')
            && let (Ok(a), Ok(b)) = (a.parse::<u64>(), b.parse::<u64>())
        {
            return (a, b);
        }
    }
    (0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn step_key_parses_range() {
        assert_eq!(step_key(Path::new("v1.1-accounts.0-1024.kv")), (0, 1024));
        assert_eq!(
            step_key(Path::new("v1.1-accounts.1024-2048.kv")),
            (1024, 2048)
        );
        // No range segment -> sorts first.
        assert_eq!(step_key(Path::new("accounts.kv")), (0, 0));
    }

    #[test]
    fn empty_open_errors() {
        let empty: Vec<&Path> = Vec::new();
        assert!(KvStack::open(empty, Salt::None).is_err());
    }
}
