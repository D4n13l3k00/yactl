//! Independent implementation of the observed Cucumber sysfs clock interface.
use super::{Frame, segment_names};
use crate::{resource::Adapter, settings::ClockBackend};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{FileExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};
type Values = BTreeMap<String, u8>;
trait Nodes: Send {
    fn read(&self) -> io::Result<Values>;
    fn write(&mut self, name: &str, value: u8) -> io::Result<()>;
    fn unlatch(&mut self) -> io::Result<()>;
    fn limits(&self) -> &Values;
}
struct Node {
    file: File,
    path: PathBuf,
}
impl Node {
    fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            file: open_node(path, true)?,
            path: path.to_owned(),
        })
    }
}
struct Sysfs {
    nodes: BTreeMap<String, Node>,
    latches: [Node; 2],
    limits: Values,
}
fn open_node(path: &Path, write: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(write)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other(
            "clock node must be a regular sysfs attribute",
        ));
    }
    Ok(file)
}
fn read_byte(file: &File) -> io::Result<u8> {
    let mut bytes = [0; 8];
    let size = file.read_at(&mut bytes, 0)?;
    if size == bytes.len() {
        return Err(io::Error::other("oversized clock attribute"));
    }
    std::str::from_utf8(&bytes[..size])
        .ok()
        .and_then(|s| s.trim().parse::<u8>().ok())
        .ok_or_else(|| io::Error::other("invalid clock attribute"))
}
fn write_byte(node: &Node, value: u8) -> io::Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&node.path)?;
    let text = value.to_string();
    let n = file.write_at(text.as_bytes(), 0)?;
    if n != text.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "partial clock attribute write",
        ));
    }
    Ok(())
}
impl Sysfs {
    fn open(directory: &Path) -> io::Result<Self> {
        let mut nodes = BTreeMap::new();
        let mut limits = Values::new();
        for name in segment_names() {
            let directory = directory.join(format!("cucumber:white:clock-{name}"));
            let limit = read_byte(&open_node(&directory.join("max_brightness"), false)?)?;
            if limit == 0 {
                return Err(io::Error::other("clock brightness limit must be positive"));
            }
            let node = Node::open(&directory.join("brightness"))?;
            if read_byte(&node.file)? > limit {
                return Err(io::Error::other(
                    "clock brightness exceeds its advertised limit",
                ));
            }
            limits.insert(name.clone(), limit);
            nodes.insert(name, node);
        }
        let latches = [
            Node::open(&directory.join("cucumber:sync:clock-0/brightness"))?,
            Node::open(&directory.join("cucumber:sync:clock-1/brightness"))?,
        ];
        for latch in &latches {
            if read_byte(&latch.file)? > 1 {
                return Err(io::Error::other("invalid clock sync latch"));
            }
        }
        // Advisory ownership among yactl workers, independent of state-directory paths.
        if unsafe {
            libc::flock(
                nodes.values().next().unwrap().file.as_raw_fd(),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        } != 0
        {
            return Err(io::Error::other("clock is owned by another adapter"));
        }
        Ok(Self {
            nodes,
            latches,
            limits,
        })
    }
}
impl Nodes for Sysfs {
    fn read(&self) -> io::Result<Values> {
        self.nodes
            .iter()
            .map(|(name, file)| Ok((name.clone(), read_byte(&file.file)?)))
            .collect()
    }
    fn write(&mut self, name: &str, value: u8) -> io::Result<()> {
        let file = self
            .nodes
            .get(name)
            .ok_or_else(|| io::Error::other("unknown clock segment"))?;
        write_byte(file, value)
    }
    fn unlatch(&mut self) -> io::Result<()> {
        for latch in &self.latches {
            if read_byte(&latch.file)? != 0 {
                write_byte(latch, 0)?;
            }
        }
        Ok(())
    }
    fn limits(&self) -> &Values {
        &self.limits
    }
}
struct Simulated {
    values: Values,
    limits: Values,
}
impl Simulated {
    fn new() -> Self {
        Self {
            values: Frame::blank().segments,
            limits: segment_names().into_iter().map(|n| (n, 255)).collect(),
        }
    }
}
impl Nodes for Simulated {
    fn read(&self) -> io::Result<Values> {
        Ok(self.values.clone())
    }
    fn write(&mut self, name: &str, value: u8) -> io::Result<()> {
        *self
            .values
            .get_mut(name)
            .ok_or_else(|| io::Error::other("unknown clock segment"))? = value;
        Ok(())
    }
    fn unlatch(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn limits(&self) -> &Values {
        &self.limits
    }
}
pub struct Backend {
    nodes: Box<dyn Nodes>,
    original: Option<Values>,
    last: Option<Values>,
    frame: Frame,
}
impl Backend {
    pub fn open(kind: ClockBackend) -> io::Result<Self> {
        let nodes: Box<dyn Nodes> = match kind {
            ClockBackend::Simulate => Box::new(Simulated::new()),
            ClockBackend::Sysfs => Box::new(Sysfs::open(Path::new("/sys/class/leds"))?),
            ClockBackend::Off => return Err(io::Error::other("clock disabled")),
        };
        Self::from_nodes(nodes)
    }
    fn from_nodes(nodes: Box<dyn Nodes>) -> io::Result<Self> {
        let frame = Frame {
            sequence: 0,
            segments: nodes.read()?,
        };
        Ok(Self {
            nodes,
            original: None,
            last: None,
            frame,
        })
    }
}
impl Adapter<Frame> for Backend {
    fn details(&self) -> serde_json::Value {
        serde_json::json!({"brightness_max":self.nodes.limits().values().copied().min().unwrap_or(0),"brightness_limits":self.nodes.limits()})
    }
    fn capture(&mut self) -> io::Result<()> {
        let current = self.nodes.read()?;
        self.frame.segments = current.clone();
        self.original = Some(current);
        self.last = None;
        Ok(())
    }
    fn write(&mut self, frame: &Frame) -> io::Result<()> {
        let limits = self.nodes.limits();
        if frame.segments.len() != limits.len()
            || frame
                .segments
                .iter()
                .any(|(name, value)| limits.get(name).is_none_or(|limit| value > limit))
        {
            return Err(io::Error::other("invalid clock frame or brightness limit"));
        }
        let before = self.nodes.read()?;
        if self.last.as_ref() != Some(&before) {
            self.original = Some(before.clone());
        }
        self.nodes.unlatch()?;
        let mut expected = before.clone();
        let mut attempted = Vec::new();
        for (name, value) in &frame.segments {
            expected.insert(name.clone(), *value);
            attempted.push(name.clone());
            if let Err(error) = self.nodes.write(name, *value) {
                // Restore only attributes that still contain our attempted value.
                let current = self.nodes.read()?;
                let mut rollback_error = None;
                for name in attempted {
                    if current.get(&name) == frame.segments.get(&name) {
                        match self.nodes.write(&name, before[&name]) {
                            Ok(()) => {
                                expected.insert(name.clone(), before[&name]);
                            }
                            Err(e) => {
                                rollback_error = Some(e);
                            }
                        }
                    }
                }
                self.last = Some(expected);
                self.frame.segments = self.nodes.read()?;
                return Err(io::Error::other(match rollback_error {
                    Some(rollback) => {
                        format!("clock write failed: {error}; rollback failed: {rollback}")
                    }
                    None => format!("clock write failed: {error}"),
                }));
            }
        }
        self.last = Some(expected);
        self.frame = frame.clone();
        Ok(())
    }
    fn restore(&mut self) -> io::Result<()> {
        if let Some(original) = &self.original {
            if self
                .last
                .as_ref()
                .is_some_and(|ours| self.nodes.read().is_ok_and(|now| now == *ours))
            {
                self.nodes.unlatch()?;
                for (name, value) in original {
                    self.last.as_mut().unwrap().insert(name.clone(), *value);
                    self.nodes.write(name, *value)?;
                }
            }
            self.frame.segments = self.nodes.read()?;
        }
        self.original = None;
        self.last = None;
        self.frame.sequence = 0;
        Ok(())
    }
    fn current(&self) -> Frame {
        self.frame.clone()
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::Command;
    use crate::resource::ResourceFrame;
    use std::{
        fs,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture {
        directory: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!(
                "yactl-clock-sysfs-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            for name in segment_names() {
                let node = directory.join(format!("cucumber:white:clock-{name}"));
                fs::create_dir_all(&node).unwrap();
                fs::write(node.join("brightness"), "7\n").unwrap();
                fs::write(node.join("max_brightness"), "255\n").unwrap();
            }
            for index in 0..2 {
                let node = directory.join(format!("cucumber:sync:clock-{index}"));
                fs::create_dir_all(&node).unwrap();
                fs::write(node.join("brightness"), "1\n").unwrap();
            }
            Self { directory }
        }
        fn segment(&self, name: &str) -> PathBuf {
            self.directory
                .join(format!("cucumber:white:clock-{name}/brightness"))
        }
        fn values(&self) -> Values {
            segment_names()
                .into_iter()
                .map(|name| {
                    let value = fs::read_to_string(self.segment(&name))
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    (name, value)
                })
                .collect()
        }
        fn backend(&self) -> Backend {
            Backend::from_nodes(Box::new(Sysfs::open(&self.directory).unwrap())).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.directory).unwrap();
        }
    }
    fn time(sequence: u64) -> Frame {
        Command::Time {
            sequence,
            time: "23:59".into(),
            brightness: 80,
        }
        .into_frame()
        .unwrap()
    }
    #[test]
    fn sysfs_probe_is_read_only_and_writes_unlatch_then_restore() {
        let fixture = Fixture::new();
        let before = fixture.values();
        let mut backend = fixture.backend();
        assert_eq!(fixture.values(), before);
        assert_eq!(
            fs::read_to_string(fixture.directory.join("cucumber:sync:clock-0/brightness")).unwrap(),
            "1\n"
        );
        backend.capture().unwrap();
        backend.write(&time(1)).unwrap();
        assert_eq!(fs::read_to_string(fixture.segment("a1")).unwrap(), "80");
        assert_eq!(fs::read_to_string(fixture.segment("c1")).unwrap(), "0");
        for index in 0..2 {
            assert_eq!(
                fs::read_to_string(
                    fixture
                        .directory
                        .join(format!("cucumber:sync:clock-{index}/brightness"))
                )
                .unwrap(),
                "0"
            );
        }
        backend.restore().unwrap();
        assert_eq!(fixture.values(), before);
    }
    #[test]
    fn newer_firmware_frame_is_preserved_and_becomes_next_snapshot() {
        let fixture = Fixture::new();
        let mut backend = fixture.backend();
        backend.capture().unwrap();
        backend.write(&time(1)).unwrap();
        fs::write(fixture.segment("a1"), "99").unwrap();
        let firmware = fixture.values();
        backend.restore().unwrap();
        assert_eq!(fixture.values(), firmware);
        backend.capture().unwrap();
        backend.write(&time(1)).unwrap();
        fs::write(fixture.segment("a1"), "42").unwrap();
        let newer = fixture.values();
        backend.write(&time(2)).unwrap();
        backend.restore().unwrap();
        assert_eq!(fixture.values(), newer);
    }
    #[test]
    fn incomplete_layouts_and_invalid_frames_do_not_write() {
        let fixture = Fixture::new();
        fs::remove_file(fixture.segment("q5")).unwrap();
        assert!(Sysfs::open(&fixture.directory).is_err());
        fs::write(fixture.segment("q5"), "7").unwrap();
        let mut backend = fixture.backend();
        backend.capture().unwrap();
        let before = fixture.values();
        let mut invalid = time(1);
        invalid.segments.remove("a1");
        invalid.segments.insert("../escape".into(), 1);
        assert!(backend.write(&invalid).is_err());
        assert_eq!(fixture.values(), before);
        assert_eq!(
            fs::read_to_string(fixture.directory.join("cucumber:sync:clock-0/brightness")).unwrap(),
            "1\n"
        );
    }
    #[test]
    fn brightness_limits_and_exclusive_ownership_are_verified() {
        let fixture = Fixture::new();
        fs::write(
            fixture
                .directory
                .join("cucumber:white:clock-a1/max_brightness"),
            "10",
        )
        .unwrap();
        let mut backend = fixture.backend();
        assert!(Sysfs::open(&fixture.directory).is_err());
        assert_eq!(backend.details()["brightness_max"], 10);
        assert_eq!(
            time(1).validate(&backend.details()),
            Err("invalid_brightness")
        );
        backend.capture().unwrap();
        let before = fixture.values();
        assert!(backend.write(&time(1)).is_err());
        assert_eq!(fixture.values(), before);
    }
    struct FaultNodes {
        values: Arc<Mutex<Values>>,
        limits: Values,
        writes: usize,
        fail: usize,
        persistent: bool,
    }
    impl Nodes for FaultNodes {
        fn read(&self) -> io::Result<Values> {
            Ok(self.values.lock().unwrap().clone())
        }
        fn write(&mut self, name: &str, value: u8) -> io::Result<()> {
            self.writes += 1;
            if self.writes == self.fail || (self.persistent && self.writes > self.fail) {
                return Err(io::Error::other("injected write failure"));
            }
            *self.values.lock().unwrap().get_mut(name).unwrap() = value;
            Ok(())
        }
        fn unlatch(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn limits(&self) -> &Values {
            &self.limits
        }
    }
    #[test]
    fn partial_write_rolls_back_and_reports_failed_rollback() {
        for persistent in [false, true] {
            let before: Values = segment_names().into_iter().map(|name| (name, 7)).collect();
            let values = Arc::new(Mutex::new(before.clone()));
            let nodes = FaultNodes {
                values: values.clone(),
                limits: segment_names().into_iter().map(|n| (n, 255)).collect(),
                writes: 0,
                fail: 3,
                persistent,
            };
            let mut backend = Backend::from_nodes(Box::new(nodes)).unwrap();
            backend.capture().unwrap();
            let error = backend.write(&time(1)).unwrap_err().to_string();
            assert!(error.contains("injected write failure"));
            if persistent {
                assert!(error.contains("rollback failed"));
            } else {
                assert_eq!(*values.lock().unwrap(), before);
            }
        }
    }
}
