//! Receiver-side storage: `<name>.wzpart` partial files, the persisted
//! resume state `<name>.wzpart.state`, collision-safe final names and the
//! atomic rename into place.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::manifest::Bitmap;

pub(crate) const PART_EXT: &str = "wzpart";
pub(crate) const STATE_SUFFIX: &str = ".wzpart.state";

#[derive(Serialize, Deserialize)]
struct StateFile {
    version: u8,
    name: String,
    size: u64,
    chunk_size: u32,
    root: String,
    bitmap: String,
}

/// An open partial download.
pub(crate) struct PartFile {
    pub file: File,
    pub part_path: PathBuf,
    pub state_path: PathBuf,
    pub name: String,
    pub size: u64,
    pub chunk_size: u32,
    pub root: [u8; 32],
    pub bitmap: Bitmap,
}

fn part_path_for(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.{PART_EXT}"))
}

fn state_path_for(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}{STATE_SUFFIX}"))
}

/// `name`, `name (1).ext`, `name (2).ext`, ...
pub(crate) fn candidate_name(name: &str, i: u32) -> String {
    if i == 0 {
        return name.to_string();
    }
    match name.rfind('.') {
        Some(p) if p > 0 => format!("{} ({i}){}", &name[..p], &name[p..]),
        _ => format!("{name} ({i})"),
    }
}

impl PartFile {
    /// Find a resumable partial download for this exact file, or create a
    /// fresh one under a name that collides with nothing in `dir`.
    pub fn open_or_create(
        dir: &Path,
        name: &str,
        size: u64,
        chunk_size: u32,
        root: [u8; 32],
        n_chunks: u64,
    ) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        if let Some(p) = Self::find_resumable(dir, size, chunk_size, &root, n_chunks) {
            return Ok(p);
        }
        for i in 0..10_000u32 {
            let cand = candidate_name(name, i);
            let part = part_path_for(dir, &cand);
            let state = state_path_for(dir, &cand);
            if dir.join(&cand).exists() || state.exists() {
                continue;
            }
            let file = match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&part)
            {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            };
            file.set_len(size)?;
            let p = PartFile {
                file,
                part_path: part,
                state_path: state,
                name: cand,
                size,
                chunk_size,
                root,
                bitmap: Bitmap::new(n_chunks),
            };
            p.persist()?;
            return Ok(p);
        }
        Err(std::io::Error::other("no free file name"))
    }

    fn find_resumable(
        dir: &Path,
        size: u64,
        chunk_size: u32,
        root: &[u8; 32],
        n_chunks: u64,
    ) -> Option<Self> {
        let want_root = hex::encode(root);
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let fname = entry.file_name();
            let Some(fname) = fname.to_str() else {
                continue;
            };
            let Some(name) = fname.strip_suffix(STATE_SUFFIX) else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(st) = serde_json::from_str::<StateFile>(&text) else {
                continue;
            };
            if st.version != 1
                || st.size != size
                || st.chunk_size != chunk_size
                || st.root != want_root
                || st.name != name
            {
                continue;
            }
            let Some(bitmap) = hex::decode(&st.bitmap)
                .ok()
                .and_then(|b| Bitmap::from_bytes(&b, n_chunks))
            else {
                continue;
            };
            let part = part_path_for(dir, name);
            let Ok(file) = OpenOptions::new().read(true).write(true).open(&part) else {
                continue;
            };
            if file.metadata().map(|m| m.len()).ok() != Some(size) {
                continue;
            }
            return Some(PartFile {
                file,
                part_path: part,
                state_path: entry.path(),
                name: name.to_string(),
                size,
                chunk_size,
                root: *root,
                bitmap,
            });
        }
        None
    }

    /// Flush data, then atomically replace the state file.
    pub fn persist(&self) -> std::io::Result<()> {
        self.file.sync_data()?;
        let st = StateFile {
            version: 1,
            name: self.name.clone(),
            size: self.size,
            chunk_size: self.chunk_size,
            root: hex::encode(self.root),
            bitmap: hex::encode(self.bitmap.as_bytes()),
        };
        let tmp = self.state_path.with_extension("state.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&st).expect("state json"))?;
        std::fs::rename(&tmp, &self.state_path)
    }

    pub fn write_chunk(&self, idx: u32, data: &[u8]) -> std::io::Result<()> {
        write_at(&self.file, data, idx as u64 * self.chunk_size as u64)
    }

    /// fsync, reserve a collision-free final name and rename into place.
    pub fn finalize(self, dir: &Path) -> std::io::Result<PathBuf> {
        self.file.sync_all()?;
        drop(self.file);
        for i in 0..10_000u32 {
            let cand = dir.join(candidate_name(&self.name, i));
            // Reserve the name atomically so a concurrent download can't
            // take it; the rename below then replaces our own placeholder.
            match OpenOptions::new().write(true).create_new(true).open(&cand) {
                Ok(_) => {
                    std::fs::rename(&self.part_path, &cand)?;
                    let _ = std::fs::remove_file(&self.state_path);
                    return Ok(cand);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::other("no free file name"))
    }

    /// Remove partial data and state (used on cancel).
    pub fn discard(self) {
        drop(self.file);
        let _ = std::fs::remove_file(&self.part_path);
        let _ = std::fs::remove_file(&self.state_path);
    }
}

#[cfg(unix)]
pub(crate) fn write_at(f: &File, buf: &[u8], off: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.write_all_at(buf, off)
}

#[cfg(windows)]
pub(crate) fn write_at(f: &File, mut buf: &[u8], mut off: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = f.seek_write(buf, off)?;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        buf = &buf[n..];
        off += n as u64;
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn read_at(f: &File, buf: &mut [u8], off: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.read_exact_at(buf, off)
}

#[cfg(windows)]
pub(crate) fn read_at(f: &File, mut buf: &mut [u8], mut off: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = f.seek_read(buf, off)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf = &mut buf[n..];
        off += n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_names() {
        assert_eq!(candidate_name("a.txt", 0), "a.txt");
        assert_eq!(candidate_name("a.txt", 2), "a (2).txt");
        assert_eq!(candidate_name("noext", 1), "noext (1)");
        assert_eq!(candidate_name("archive.tar.gz", 1), "archive.tar (1).gz");
    }

    #[test]
    fn collision_safe_and_resume() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("x.bin"), b"existing").unwrap();
        let mut p = PartFile::open_or_create(d.path(), "x.bin", 10, 4, [1; 32], 3).unwrap();
        assert_eq!(p.name, "x (1).bin");
        p.write_chunk(1, b"abcd").unwrap();
        p.bitmap.set(1);
        p.persist().unwrap();
        drop(p);
        let p2 = PartFile::open_or_create(d.path(), "x.bin", 10, 4, [1; 32], 3).unwrap();
        assert_eq!(p2.name, "x (1).bin");
        assert!(p2.bitmap.get(1) && !p2.bitmap.get(0));
        // different root => fresh file
        let p3 = PartFile::open_or_create(d.path(), "x.bin", 10, 4, [2; 32], 3).unwrap();
        assert_eq!(p3.name, "x (2).bin");
        let out = p2.finalize(d.path()).unwrap();
        assert_eq!(out.file_name().unwrap(), "x (1).bin");
        assert!(!d.path().join("x (1).bin.wzpart").exists());
        assert!(!d.path().join("x (1).bin.wzpart.state").exists());
        p3.discard();
        assert!(!d.path().join("x (2).bin.wzpart").exists());
    }
}
