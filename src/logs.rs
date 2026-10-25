use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
fn previous_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_os_string();
    p.push(".1");
    p.into()
}
fn open_log(path: &Path) -> io::Result<File> {
    let f = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !f.metadata()?.is_file() {
        return Err(io::Error::other("log is not a regular file"));
    }
    Ok(f)
}
pub struct LogWriter {
    path: PathBuf,
    max_bytes: u64,
    file: File,
}
impl LogWriter {
    pub fn new(path: &Path, max_bytes: u64) -> io::Result<Self> {
        if max_bytes == 0 || max_bytes > 16 * 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid log limit",
            ));
        }
        let file = open_log(path)?;
        if file.metadata()?.len() > max_bytes {
            file.set_len(max_bytes)?;
        }
        let previous = previous_path(path);
        match OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(previous)
        {
            Ok(f) => {
                if !f.metadata()?.is_file() {
                    return Err(io::Error::other("previous log is not regular"));
                }
                if f.metadata()?.len() > max_bytes {
                    f.set_len(max_bytes)?;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(Self {
            path: path.into(),
            max_bytes,
            file,
        })
    }
    pub fn append(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let len = self.file.metadata()?.len();
            if len >= self.max_bytes {
                fs::rename(&self.path, previous_path(&self.path))?;
                self.file = open_log(&self.path)?;
            }
            let room = (self.max_bytes - self.file.metadata()?.len()) as usize;
            let n = room.min(bytes.len());
            self.file.write_all(&bytes[..n])?;
            bytes = &bytes[n..];
        }
        Ok(())
    }
}
fn read_bounded(path: &Path) -> io::Result<Vec<u8>> {
    let mut f = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let m = f.metadata()?;
    if !m.is_file() {
        return Err(io::Error::other("log is not regular"));
    }
    const CAP: u64 = 16 * 1024 * 1024;
    f.seek(SeekFrom::Start(m.len().saturating_sub(CAP)))?;
    let mut b = Vec::new();
    f.take(CAP).read_to_end(&mut b)?;
    Ok(b)
}
pub fn tail(path: &Path, lines: usize) -> io::Result<String> {
    if lines == 0 {
        return Ok(String::new());
    }
    let mut b = read_bounded(&previous_path(path))?;
    b.extend(read_bounded(path)?);
    let mut end = b.len();
    if end > 0 && b[end - 1] == b'\n' {
        end -= 1;
    }
    let mut count = 0;
    let mut start = 0;
    for i in (0..end).rev() {
        if b[i] == b'\n' {
            count += 1;
            if count == lines {
                start = i + 1;
                break;
            }
        }
    }
    Ok(String::from_utf8_lossy(&b[start..]).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refuses_symlink_log() {
        let dir = std::env::temp_dir().join(format!("yactl-log-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let target = dir.join("target");
        std::fs::write(&target, b"preserve").unwrap();
        let path = dir.join("a.log");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(LogWriter::new(&path, 12).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"preserve");
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn rotation_bounds_and_tail() {
        let dir = std::env::temp_dir().join(format!("yactl-logs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("a.log");
        let mut w = LogWriter::new(&path, 12).unwrap();
        w.append(b"first\n").unwrap();
        w.append(b"second\nthird\n").unwrap();
        assert_eq!(tail(&path, 2).unwrap(), "second\nthird\n");
        w.append(&[b'x'; 100]).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() <= 12);
        assert!(std::fs::metadata(previous_path(&path)).unwrap().len() <= 12);
        assert_eq!(tail(&path, 0).unwrap(), "");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
