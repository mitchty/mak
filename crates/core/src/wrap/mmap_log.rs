//! A growable, mmap-backed append-only log file to surive if the process is
//! signalled.
use std::{
    fs::{File, OpenOptions},
    io,
    path::Path,
};

use memmap2::MmapMut;

/// Initial log file mmap capacity, doubled when it needs to grow. Stupid algorithm.
const INITIAL_CAPACITY: usize = 64 * 1024;

pub(super) struct MmapLog {
    file: File,
    mmap: MmapMut,
    capacity: usize,
    len: usize,
}

impl MmapLog {
    pub(super) fn create(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path)?;
        file.set_len(INITIAL_CAPACITY as u64)?;
        let mmap = unsafe { MmapMut::map_mut(&file)? };
        Ok(Self {
            file,
            mmap,
            capacity: INITIAL_CAPACITY,
            len: 0,
        })
    }

    pub(super) fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.ensure_capacity(self.len + bytes.len())?;
        self.mmap[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
        Ok(())
    }

    /// Force a sync of data.
    pub(super) fn sync(&self) -> io::Result<()> {
        self.mmap.flush()
    }

    /// Flush and trim the log file.
    pub(super) fn finish(self) -> io::Result<()> {
        self.mmap.flush()?;
        self.file.set_len(self.len as u64)
    }

    fn ensure_capacity(&mut self, needed: usize) -> io::Result<()> {
        if needed <= self.capacity {
            return Ok(());
        }
        let mut capacity = self.capacity.max(INITIAL_CAPACITY);
        while capacity < needed {
            capacity *= 2;
        }
        self.file.set_len(capacity as u64)?;
        self.mmap = unsafe { MmapMut::map_mut(&self.file)? };
        self.capacity = capacity;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "cilogger-mmap-log-test-{name}-{}",
            std::process::id()
        ))
    }

    #[test]
    fn appends_and_reads_back_content() {
        let path = temp_path("appends");
        let mut log = MmapLog::create(&path).expect("create log");
        log.append(b"hello\n").expect("append");
        log.append(b"world\n").expect("append");
        log.finish().expect("finish");

        let mut contents = String::new();
        File::open(&path)
            .expect("open")
            .read_to_string(&mut contents)
            .expect("read");
        assert_eq!(contents, "hello\nworld\n");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn grows_past_the_initial_capacity_and_truncates_on_finish() {
        let path = temp_path("grows");
        let mut log = MmapLog::create(&path).expect("create log");

        let chunk = vec![b'x'; INITIAL_CAPACITY + 1];
        log.append(&chunk).expect("append past capacity");
        assert!(log.capacity > INITIAL_CAPACITY);
        log.finish().expect("finish");

        let metadata = std::fs::metadata(&path).expect("metadata");
        assert_eq!(metadata.len(), chunk.len() as u64);

        let _ = std::fs::remove_file(&path);
    }
}
