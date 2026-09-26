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
        let text: String = name.iter().map(|&b| b as char).collect();
        let text = text.replace('\\', "/");
        let relative = text.trim_start_matches('/');
        let path = normalize(&self.base.join(relative));
        if path != self.base && !path.starts_with(&self.base) {
            return Err(PcdrvError::Escape(relative.to_owned()));
        }
        Ok(path)
    }

    fn alloc(&mut self, file: File) -> i32 {
        let handle = self.next_fd;
        self.next_fd += 1;
        self.files.insert(
            handle,
            OpenFile {
                file,
                pos: 0,
                written: 0,
            },
        );
        handle
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
        self.created += 1;
        Ok(self.alloc(file))
    }

    /// PCopen: bit 0 of `flags` opens for writing too.
    pub fn open(&mut self, name: &[u8], flags: u32) -> Result<i32, PcdrvError> {
        let path = self.resolve_name(name)?;
        let file = OpenOptions::new().read(true).write(flags & 1 != 0).open(&path)?;
        Ok(self.alloc(file))
    }

    /// PCclose: 0, also for a handle that is not open.
    pub fn close(&mut self, handle: i32) -> i32 {
        self.files.remove(&handle);
        0
    }

    fn expect(&mut self, handle: i32) -> Result<&mut OpenFile, PcdrvError> {
        self.files.get_mut(&handle).ok_or(PcdrvError::BadHandle(handle))
    }

    /// Refuse a write of `length` bytes before its payload is fetched from
    /// the target, if it would breach a quota.
    pub fn check_write(&mut self, handle: i32, length: u64) -> Result<(), PcdrvError> {
        let (quota, total) = (self.quota, self.total_bytes);
        let f = self.expect(handle)?;
        if f.written + length > quota.per_file_bytes {
            return Err(PcdrvError::Quota(format!(
                "per-file output quota exceeded (cap {} bytes)",
                quota.per_file_bytes
            )));
        }
        if total + length > quota.total_bytes {
            return Err(PcdrvError::Quota(format!(
                "total output quota exceeded (cap {} bytes)",
                quota.total_bytes
            )));
        }
        Ok(())
    }

    /// PCwrite at the handle's position; returns the byte count.
    pub fn write(&mut self, handle: i32, data: &[u8]) -> Result<i32, PcdrvError> {
        self.check_write(handle, data.len() as u64)?;
        let f = self.expect(handle)?;
        f.file.seek(SeekFrom::Start(f.pos))?;
        f.file.write_all(data)?;
        f.pos += data.len() as u64;
        f.written += data.len() as u64;
        self.total_bytes += data.len() as u64;
        Ok(data.len() as i32)
    }

    /// PCread of up to `length` bytes at the handle's position.
    pub fn read(&mut self, handle: i32, length: u32) -> Result<Vec<u8>, PcdrvError> {
        let f = self.expect(handle)?;
        if length > MAX_READ_BYTES {
            return Err(PcdrvError::Quota(format!(
                "read length quota exceeded (cap {MAX_READ_BYTES} bytes)"
            )));
        }
        let mut out = vec![0u8; length as usize];
        f.file.seek(SeekFrom::Start(f.pos))?;
        let mut n = 0;
        while n < out.len() {
            let got = f.file.read(&mut out[n..])?;
            if got == 0 {
                break;
            }
            n += got;
        }
        out.truncate(n);
        f.pos += n as u64;
        Ok(out)
    }

    /// PClseek: whence 0 set, 1 current, 2 end; a negative result clamps to 0.
    pub fn seek(&mut self, handle: i32, offset: i32, whence: u32) -> Result<i32, PcdrvError> {
        let f = self.expect(handle)?;
        let size = f.file.metadata()?.len() as i64;
        let pos = match whence {
            1 => f.pos as i64 + offset as i64,
            2 => size + offset as i64,
            _ => offset as i64,
        }
        .max(0);
        f.pos = pos as u64;
        Ok(pos as i32)
    }

    pub fn close_all(&mut self) {
        self.files.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jail() {
        let dir = tempfile::tempdir().unwrap();
        let s = PcdrvServer::new(dir.path(), Quota::default()).unwrap();
        let base = s.base().to_path_buf();
        assert_eq!(s.resolve_name(b"IN.TXT").unwrap(), base.join("IN.TXT"));
        assert_eq!(s.resolve_name(b"\\sub\\A.BIN").unwrap(), base.join("sub/A.BIN"));
        assert_eq!(s.resolve_name(b"///x").unwrap(), base.join("x"));
        assert_eq!(s.resolve_name(b"a/../b").unwrap(), base.join("b"));
        assert!(s.resolve_name(b"../x").is_err());
        assert!(s.resolve_name(b"..\\..\\etc\\passwd").is_err());
        assert!(s.resolve_name(b"a/../../x").is_err());
        // Back into the base by name is inside, as with path.resolve.
        let back = format!("../{}/y", base.file_name().unwrap().to_str().unwrap());
        assert_eq!(s.resolve_name(back.as_bytes()).unwrap(), base.join("y"));
    }

    #[test]
    fn files_and_quotas() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = PcdrvServer::new(
            dir.path(),
            Quota {
                per_file_bytes: 10,
                total_bytes: 15,
                file_count: 2,
            },
        )
        .unwrap();
        let a = s.create(b"d/A").unwrap();
        assert_eq!(a, 3);
        assert_eq!(s.write(a, b"0123456789").unwrap(), 10);
        assert!(matches!(s.write(a, b"x"), Err(PcdrvError::Quota(_))));
        assert_eq!(s.seek(a, 2, 0).unwrap(), 2);
        assert_eq!(s.read(a, 3).unwrap(), b"234");
        assert_eq!(s.seek(a, -1, 2).unwrap(), 9);
        assert_eq!(s.seek(a, -100, 1).unwrap(), 0);
        let b = s.create(b"B").unwrap();
        assert!(matches!(s.write(b, b"123456"), Err(PcdrvError::Quota(_))));
        assert!(matches!(s.create(b"C"), Err(PcdrvError::Quota(_))));
        assert_eq!(s.close(a), 0);
        assert!(matches!(s.read(a, 1), Err(PcdrvError::BadHandle(3))));
        assert!(s.open(b"missing", 0).is_err());
        let r = s.open(b"d/A", 0).unwrap();
        assert!(s.read(r, MAX_READ_BYTES + 1).is_err());
        assert_eq!(std::fs::read(dir.path().join("d/A")).unwrap(), b"0123456789");
    }
}
