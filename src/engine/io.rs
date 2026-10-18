//! Safe output: write to a temporary sibling, then publish it without ever replacing a file.

use super::error::{Error, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// Largest input file the engine will read into memory.
pub const MAX_INPUT_BYTES: u64 = 512 * 1024 * 1024;

pub fn read_limited(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len > MAX_INPUT_BYTES {
        return Err(Error::TooLarge { width: 0, height: 0 });
    }
    let mut buf = Vec::with_capacity(len as usize);
    // `take` also bounds files that grow while we read them.
    file.take(MAX_INPUT_BYTES + 1).read_to_end(&mut buf).map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
    if buf.len() as u64 > MAX_INPUT_BYTES {
        return Err(Error::TooLarge { width: 0, height: 0 });
    }
    Ok(buf)
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Writes `data` as `dir/stem.ext` (or `stem (n).ext`) and returns the path. Never overwrites.
///
/// The bytes go to a hidden temporary file first, so a crash or a full disk never leaves a
/// half-written output under the final name. Publishing uses a hard link, which fails when the
/// name is taken (unlike `rename`, which replaces on Windows); file systems without hard links
/// fall back to a checked rename.
pub fn commit_unique(dir: &Path, stem: &str, ext: &str, data: &[u8]) -> Result<PathBuf> {
    let tmp = dir.join(format!(".radial-{}-{}.tmp", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| Error::Io(format!("{}: {e}", dir.display())))?;
    let wrote = file.write_all(data).and_then(|()| file.flush());
    drop(file);
    if let Err(e) = wrote {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Io(e.to_string()));
    }
    let result = publish(&tmp, dir, stem, ext);
    let _ = std::fs::remove_file(&tmp);
    result
}

fn publish(tmp: &Path, dir: &Path, stem: &str, ext: &str) -> Result<PathBuf> {
    for _ in 0..256 {
        let out = crate::convert::unique_path(dir, stem, ext, |p| p.exists());
        match std::fs::hard_link(tmp, &out) {
            Ok(()) => return Ok(out),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => {
                // No hard links on this volume: rename is only safe if the name is still free.
                if out.exists() {
                    continue;
                }
                std::fs::rename(tmp, &out).map_err(|e| Error::Io(format!("{}: {e}", out.display())))?;
                return Ok(out);
            }
        }
    }
    Err(Error::Io("could not pick a free output name".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("radial-io-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn writes_the_bytes_and_leaves_no_temp_file() {
        let d = tmp("basic");
        let p = commit_unique(&d, "a", "txt", b"hello").unwrap();
        assert_eq!(p, d.join("a.txt"));
        assert_eq!(std::fs::read(&p).unwrap(), b"hello");
        let names: Vec<_> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 1, "{names:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn never_overwrites_an_existing_file() {
        let d = tmp("clash");
        std::fs::write(d.join("a.txt"), b"mine").unwrap();
        let p = commit_unique(&d, "a", "txt", b"new").unwrap();
        assert_eq!(p, d.join("a (1).txt"));
        assert_eq!(std::fs::read(d.join("a.txt")).unwrap(), b"mine");
        let p2 = commit_unique(&d, "a", "txt", b"newer").unwrap();
        assert_eq!(p2, d.join("a (2).txt"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn missing_directory_is_a_typed_io_error_and_nothing_is_created() {
        let d = tmp("missing").join("nope");
        assert!(matches!(commit_unique(&d, "a", "txt", b"x"), Err(Error::Io(_))));
        assert!(!d.exists());
    }

    #[test]
    fn concurrent_writers_get_distinct_names() {
        let d = tmp("race");
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let d = d.clone();
                std::thread::spawn(move || commit_unique(&d, "x", "bin", &[i as u8]).unwrap())
            })
            .collect();
        let mut paths: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), 8);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn read_limited_reads_and_reports_missing_files() {
        let d = tmp("read");
        std::fs::write(d.join("f"), b"abc").unwrap();
        assert_eq!(read_limited(&d.join("f")).unwrap(), b"abc");
        assert!(matches!(read_limited(&d.join("none")), Err(Error::Io(_))));
        let _ = std::fs::remove_dir_all(&d);
    }
}
