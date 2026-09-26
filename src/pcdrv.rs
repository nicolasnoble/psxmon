//! PCDRV host file server, rooted at a directory. Names from the target are
//! flattened (backslashes to slashes, leading slashes dropped), resolved
//! lexically against the root, and refused if they land outside it. Writes
//! are held to per-file, per-run and file-count quotas, and a single read is
//! capped, since the target picks every length. Mirrors runner-agent
//! `unirom/pcdrv.ts`.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quota {
    /// Cap on bytes written to one file.
    pub per_file_bytes: u64,
    /// Cap on bytes written across the run.
    pub total_bytes: u64,
    /// Cap on files created across the run.
    pub file_count: u32,
}

impl Default for Quota {
    fn default() -> Self {
        Quota {
            per_file_bytes: 16 << 20,
            total_bytes: 64 << 20,
            file_count: 256,
        }
    }
}

/// Ceiling on one PCread: the length comes from the target's a2.
pub const MAX_READ_BYTES: u32 = 2 << 20;

#[derive(Debug, thiserror::Error)]
pub enum PcdrvError {
    #[error("path escapes base: {0}")]
    Escape(String),
    #[error("bad PCDRV handle {0}")]
    BadHandle(i32),
    #[error("{0}")]
    Quota(String),
    #[error("{0} out of range")]
    Range(&'static str),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

struct OpenFile {
    file: File,
    pos: u64,
    written: u64,
}

pub struct PcdrvServer {
    base: PathBuf,
    files: BTreeMap<i32, OpenFile>,
    next_fd: i32,
    quota: Quota,
    total_bytes: u64,
    created: u32,
}

/// `path` with `.` and `..` folded away without touching the file system,
/// as node's `path.resolve` does. `..` at the root stays at the root.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

impl PcdrvServer {
    pub fn new(base: impl AsRef<Path>, quota: Quota) -> std::io::Result<Self> {
        let base = normalize(&std::path::absolute(base.as_ref())?);
        Ok(PcdrvServer {
            base,
            files: BTreeMap::new(),
            next_fd: 3,
            quota,
            total_bytes: 0,
            created: 0,
        })
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    /// Where a target-supplied name lands under the base, or an error if it
    /// would escape it. Names are Latin-1 on the target side.
    pub fn resolve_name(&self, name: &[u8]) -> Result<PathBuf, PcdrvError> {
        let text: String = name.iter().copied().map(char::from).collect();
        let text = text.replace('\\', "/");
        let relative = text.trim_start_matches('/');
        let path = normalize(&self.base.join(relative));
        if path != self.base && !path.starts_with(&self.base) {
            return Err(PcdrvError::Escape(relative.to_owned()));
        }
        Ok(path)
    }

    fn alloc(&mut self, file: File) -> Result<i32, PcdrvError> {
        let handle = self.next_fd;
        self.next_fd = handle
            .checked_add(1)
            .ok_or(PcdrvError::Range("file handles"))?;
        self.files.insert(
            handle,
            OpenFile {
                file,
                pos: 0,
                written: 0,
            },
        );
        Ok(handle)
    }

    /// PCcreat: create or truncate for reading and writing.
    pub fn create(&mut self, name: &[u8]) -> Result<i32, PcdrvError> {
        if self.created >= self.quota.file_count {
            return Err(PcdrvError::Quota(format!(
                "output file count quota exceeded ({})",
                self.quota.file_count
            )));
        }
        let path = self.resolve_name(name)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        self.created = self.created.saturating_add(1);
        self.alloc(file)
    }

    /// PCopen: bit 0 of `flags` opens for writing too.
    pub fn open(&mut self, name: &[u8], flags: u32) -> Result<i32, PcdrvError> {
        let path = self.resolve_name(name)?;
        let file = OpenOptions::new()
            .read(true)
            .write(flags & 1 != 0)
            .open(&path)?;
        self.alloc(file)
    }

    /// PCclose: 0, also for a handle that is not open.
    pub fn close(&mut self, handle: i32) -> i32 {
        self.files.remove(&handle);
        0
    }

    fn expect(&mut self, handle: i32) -> Result<&mut OpenFile, PcdrvError> {
        self.files
            .get_mut(&handle)
            .ok_or(PcdrvError::BadHandle(handle))
    }

    /// Refuse a write of `length` bytes before its payload is fetched from
    /// the target, if it would breach a quota.
    pub fn check_write(&mut self, handle: i32, length: u64) -> Result<(), PcdrvError> {
        let (quota, total) = (self.quota, self.total_bytes);
        let f = self.expect(handle)?;
        if f.written.saturating_add(length) > quota.per_file_bytes {
            return Err(PcdrvError::Quota(format!(
                "per-file output quota exceeded (cap {} bytes)",
                quota.per_file_bytes
            )));
        }
        if total.saturating_add(length) > quota.total_bytes {
            return Err(PcdrvError::Quota(format!(
                "total output quota exceeded (cap {} bytes)",
                quota.total_bytes
            )));
        }
        Ok(())
    }

    /// PCwrite at the handle's position; returns the byte count.
    pub fn write(&mut self, handle: i32, data: &[u8]) -> Result<i32, PcdrvError> {
        let result = i32::try_from(data.len()).map_err(|_| PcdrvError::Range("PCwrite length"))?;
        let len = u64::try_from(data.len()).map_err(|_| PcdrvError::Range("PCwrite length"))?;
        self.check_write(handle, len)?;
        let f = self.expect(handle)?;
        f.file.seek(SeekFrom::Start(f.pos))?;
        f.file.write_all(data)?;
        // Bounded by the quotas checked above.
        f.pos = f.pos.saturating_add(len);
        f.written = f.written.saturating_add(len);
        self.total_bytes = self.total_bytes.saturating_add(len);
        Ok(result)
    }

    /// PCread of up to `length` bytes at the handle's position.
    pub fn read(&mut self, handle: i32, length: u32) -> Result<Vec<u8>, PcdrvError> {
        let f = self.expect(handle)?;
        if length > MAX_READ_BYTES {
            return Err(PcdrvError::Quota(format!(
                "read length quota exceeded (cap {MAX_READ_BYTES} bytes)"
            )));
        }
        let mut out = Vec::new();
        f.file.seek(SeekFrom::Start(f.pos))?;
        let n = (&mut f.file)
            .take(u64::from(length))
            .read_to_end(&mut out)?;
        f.pos = f
            .pos
            .saturating_add(u64::try_from(n).map_err(|_| PcdrvError::Range("PCread"))?);
        Ok(out)
    }

    /// PClseek: whence 0 set, 1 current, 2 end; a negative result clamps to 0.
    pub fn seek(&mut self, handle: i32, offset: i32, whence: u32) -> Result<i32, PcdrvError> {
        let f = self.expect(handle)?;
        let range = |_| PcdrvError::Range("PClseek position");
        let base = match whence {
            1 => i64::try_from(f.pos).map_err(range)?,
            2 => i64::try_from(f.file.metadata()?.len()).map_err(range)?,
            _ => 0,
        };
        let pos = base.saturating_add(i64::from(offset)).max(0);
        // The target gets the position back as an int.
        let result = i32::try_from(pos).map_err(range)?;
        f.pos = u64::try_from(pos).map_err(range)?;
        Ok(result)
    }

    pub fn close_all(&mut self) {
        self.files.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(s: &PcdrvServer, name: &[u8]) -> PathBuf {
        s.resolve_name(name).expect("name stays inside the jail")
    }

    #[test]
    fn jail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = PcdrvServer::new(dir.path(), Quota::default()).expect("server");
        let base = s.base().to_path_buf();
        assert_eq!(resolve(&s, b"IN.TXT"), base.join("IN.TXT"));
        assert_eq!(resolve(&s, b"\\sub\\A.BIN"), base.join("sub/A.BIN"));
        assert_eq!(resolve(&s, b"///x"), base.join("x"));
        assert_eq!(resolve(&s, b"a/../b"), base.join("b"));
        assert!(s.resolve_name(b"../x").is_err());
        assert!(s.resolve_name(b"..\\..\\etc\\passwd").is_err());
        assert!(s.resolve_name(b"a/../../x").is_err());
        // Back into the base by name is inside, as with path.resolve.
        let leaf = base
            .file_name()
            .and_then(|n| n.to_str())
            .expect("tempdir has a UTF-8 name");
        assert_eq!(
            resolve(&s, format!("../{leaf}/y").as_bytes()),
            base.join("y")
        );
    }

    #[test]
    fn files_and_quotas() {
        let dir = tempfile::tempdir().expect("tempdir");
        let quota = Quota {
            per_file_bytes: 10,
            total_bytes: 15,
            file_count: 2,
        };
        let mut s = PcdrvServer::new(dir.path(), quota).expect("server");
        let a = s.create(b"d/A").expect("create d/A");
        assert_eq!(a, 3);
        assert_eq!(s.write(a, b"0123456789").expect("write within quota"), 10);
        assert!(matches!(s.write(a, b"x"), Err(PcdrvError::Quota(_))));
        assert_eq!(s.seek(a, 2, 0).expect("seek set"), 2);
        assert_eq!(s.read(a, 3).expect("read"), b"234");
        assert_eq!(s.seek(a, -1, 2).expect("seek end"), 9);
        assert_eq!(s.seek(a, -100, 1).expect("seek clamps"), 0);
        let b = s.create(b"B").expect("create B");
        assert!(matches!(s.write(b, b"123456"), Err(PcdrvError::Quota(_))));
        assert!(matches!(s.create(b"C"), Err(PcdrvError::Quota(_))));
        assert_eq!(s.close(a), 0);
        assert!(matches!(s.read(a, 1), Err(PcdrvError::BadHandle(3))));
        assert!(s.open(b"missing", 0).is_err());
        let r = s.open(b"d/A", 0).expect("open d/A");
        assert!(s.read(r, MAX_READ_BYTES.saturating_add(1)).is_err());
        assert!(s.write(r, b"z").is_err(), "opened read-only");
        let on_disk = std::fs::read(dir.path().join("d/A")).expect("d/A on disk");
        assert_eq!(on_disk, b"0123456789");
    }
}
