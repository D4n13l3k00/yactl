//! Linux LP64 evdev UAPI. No write ioctls and no EVIOCGRAB.
use super::{Descriptor, Record, Snapshot, Source, records};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read},
    mem::{offset_of, size_of},
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    },
    path::Path,
};

#[repr(C)]
struct Layout {
    time: libc::timeval,
    kind: u16,
    code: u16,
    value: i32,
}
const _: () = {
    assert!(size_of::<libc::c_long>() == 8);
    assert!(size_of::<Layout>() == 24);
    assert!(
        offset_of!(Layout, kind) == 16
            && offset_of!(Layout, code) == 18
            && offset_of!(Layout, value) == 20
    );
};
fn get<const N: usize>(file: &File, number: u8) -> io::Result<[u8; N]> {
    let mut buffer = [0; N];
    // Linux asm-generic _IOC(_IOC_READ, 'E', number, N), used on AArch64
    // and x86_64. Only fixed-sized initialized byte buffers cross this ABI.
    let request = 0x8000_4500u32 | ((N as u32) << 16) | u32::from(number);
    // SAFETY: the read-only request encodes exactly the initialized buffer size;
    // the kernel cannot write beyond it. All callers use input UAPI requests.
    let result = unsafe { libc::ioctl(file.as_raw_fd(), request as _, buffer.as_mut_ptr()) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    if result as usize > N {
        return Err(io::Error::other("invalid input ioctl response"));
    }
    Ok(buffer)
}
fn bit(bytes: &[u8], number: u16) -> bool {
    bytes
        .get(usize::from(number / 8))
        .is_some_and(|byte| byte & (1 << (number % 8)) != 0)
}
pub(super) struct Evdev {
    file: File,
    descriptor: Descriptor,
}
impl Evdev {
    fn open(path: &Path, expected: &str, id: String) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.file_type().is_char_device() || libc::major(metadata.rdev()) != 13 {
            return Err(io::Error::other(
                "input node is not an input character device",
            ));
        }
        let name = get::<64>(&file, 0x06)?;
        let length = name
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| io::Error::other("oversized input name"))?;
        let name = std::str::from_utf8(&name[..length]).map_err(io::Error::other)?;
        if name != expected {
            return Err(io::Error::other("input device identity changed"));
        }
        let types = get::<4>(&file, 0x20)?;
        let keys = if bit(&types, 1) {
            let bits = get::<96>(&file, 0x21)?;
            (0..768).filter(|code| bit(&bits, *code)).collect()
        } else {
            Vec::new()
        };
        let switches = if bit(&types, 5) {
            let bits = get::<4>(&file, 0x25)?;
            (0..32).filter(|code| bit(&bits, *code)).collect()
        } else {
            Vec::new()
        };
        let descriptor = Descriptor {
            id,
            name: name.into(),
            keys,
            switches,
        };
        if descriptor.keys.is_empty() && descriptor.switches.is_empty() {
            return Err(io::Error::other("input device has no supported events"));
        }
        Ok(Self { file, descriptor })
    }
}
impl Source for Evdev {
    fn descriptor(&self) -> Descriptor {
        self.descriptor.clone()
    }
    fn read(&mut self) -> io::Result<Vec<Record>> {
        let mut bytes = [0; 24 * 32];
        match self.file.read(&mut bytes) {
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "input device disconnected",
            )),
            Ok(size) => records(&bytes[..size]),
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::Interrupted =>
            {
                Ok(Vec::new())
            }
            Err(error) => Err(error),
        }
    }
    fn snapshot(&self) -> io::Result<Snapshot> {
        let keys = if self.descriptor.keys.is_empty() {
            [0; 96]
        } else {
            get::<96>(&self.file, 0x18)?
        };
        let switches = if self.descriptor.switches.is_empty() {
            [0; 4]
        } else {
            get::<4>(&self.file, 0x1b)?
        };
        Ok(Snapshot {
            keys_down: self
                .descriptor
                .keys
                .iter()
                .copied()
                .filter(|code| bit(&keys, *code))
                .collect(),
            switches_on: self
                .descriptor
                .switches
                .iter()
                .copied()
                .filter(|code| bit(&switches, *code))
                .collect(),
        })
    }
}
#[derive(Debug, PartialEq, Eq)]
struct Candidate {
    event: usize,
    selection: usize,
    name: String,
}
fn candidates(class: &Path, names: &[String]) -> Vec<Candidate> {
    if names.is_empty() {
        return Vec::new();
    }
    let mut result = Vec::new();
    for event in 0..32 {
        let path = class.join(format!("event{event}/device/name"));
        let name = (|| {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path)?;
            if !file.metadata()?.is_file() {
                return Err(io::Error::other("invalid input name attribute"));
            }
            let mut bytes = String::new();
            file.take(65).read_to_string(&mut bytes)?;
            if bytes.len() > 64 {
                return Err(io::Error::other("oversized input name"));
            }
            Ok(bytes.strip_suffix('\n').unwrap_or(&bytes).to_owned())
        })();
        if let Ok(name) = name
            && let Some(selection) = names.iter().position(|wanted| wanted == &name)
        {
            result.push(Candidate {
                event,
                selection,
                name,
            });
        }
    }
    result
}
pub(super) fn discover(names: &[String]) -> Vec<Box<dyn Source>> {
    let mut sources: Vec<Box<dyn Source>> = Vec::new();
    for candidate in candidates(Path::new("/sys/class/input"), names) {
        if sources
            .iter()
            .any(|known| known.descriptor().name == candidate.name)
        {
            continue;
        }
        let path = Path::new("/dev/input").join(format!("event{}", candidate.event));
        if let Ok(source) = Evdev::open(
            &path,
            &candidate.name,
            format!("input:device-{}", candidate.selection),
        ) {
            sources.push(Box::new(source));
            if sources.len() == names.len() {
                break;
            }
        }
    }
    sources
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_selects_names_before_opening_any_evdev_node() {
        let root = std::env::temp_dir().join(format!("yactl-input-names-{}", std::process::id()));
        for (index, name) in ["input_btrcu", "mute", "awinic_sar"]
            .into_iter()
            .enumerate()
        {
            let directory = root.join(format!("event{index}/device"));
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("name"), format!("{name}\n")).unwrap();
        }
        let selected = vec!["awinic_sar".into(), "mute".into()];
        assert_eq!(
            candidates(&root, &selected),
            vec![
                Candidate {
                    event: 1,
                    selection: 1,
                    name: "mute".into()
                },
                Candidate {
                    event: 2,
                    selection: 0,
                    name: "awinic_sar".into()
                }
            ]
        );
        assert!(candidates(&root, &[]).is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn unrelated_files_and_character_devices_cannot_be_input_sources() {
        assert!(Evdev::open(Path::new("/proc/self/stat"), "test", "input:test".into()).is_err());
        assert!(Evdev::open(Path::new("/dev/null"), "test", "input:test".into()).is_err());
    }
}
