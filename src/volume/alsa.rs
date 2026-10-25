//! Minimal Linux UAPI bindings for the verified Cucumber master volume control.
//! UAPI reference: Linux include/uapi/sound/asound.h (Linux-syscall-note).
use super::Mixer;
use std::{
    fs::{File, OpenOptions},
    io,
    mem::{offset_of, size_of},
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, OpenOptionsExt},
    },
    path::Path,
};
#[repr(C)]
#[derive(Clone, Copy)]
struct ElemId {
    numid: u32,
    iface: u32,
    device: u32,
    subdevice: u32,
    name: [u8; 44],
    index: u32,
}
#[repr(C)]
struct ElemInfo {
    id: ElemId,
    kind: u32,
    access: u32,
    count: u32,
    owner: i32,
    value: [libc::c_long; 16],
    reserved: [u8; 64],
}
#[repr(C)]
struct ElemValue {
    id: ElemId,
    flags: u32,
    value: [libc::c_long; 128],
    reserved: [u8; 128],
}
#[repr(C)]
struct CardInfo {
    card: i32,
    pad: i32,
    id: [u8; 16],
    driver: [u8; 16],
    name: [u8; 32],
    longname: [u8; 80],
    reserved: [u8; 16],
    mixername: [u8; 80],
    components: [u8; 128],
}
const _: () = {
    assert!(size_of::<libc::c_long>() == 8);
    assert!(size_of::<ElemId>() == 64);
    assert!(size_of::<ElemInfo>() == 272 && offset_of!(ElemInfo, value) == 80);
    assert!(size_of::<ElemValue>() == 1224 && offset_of!(ElemValue, value) == 72);
    assert!(size_of::<CardInfo>() == 376);
};
const INFO: u32 = 0xc1105511;
const READ: u32 = 0xc4c85512;
const WRITE: u32 = 0xc4c85513;
const CARD: u32 = 0x81785501;
const MASTER: &[u8] = b"Master Playback Volume";
fn ioctl<T>(file: &File, request: u32, data: &mut T) -> io::Result<()> {
    if ((request >> 16) & 0x3fff) as usize != size_of::<T>() {
        return Err(io::Error::other("ALSA ioctl size mismatch"));
    }
    // SAFETY: callers pass initialized numeric UAPI structs whose exact layouts
    // are asserted above. The request's encoded size also matches the buffer.
    if unsafe { libc::ioctl(file.as_raw_fd(), request as _, std::ptr::from_mut(data)) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn info(file: &File) -> io::Result<ElemInfo> {
    // SAFETY: the complete UAPI structure consists only of integer fields/arrays.
    let mut info: ElemInfo = unsafe { std::mem::zeroed() };
    info.id.iface = 2;
    info.id.name[..MASTER.len()].copy_from_slice(MASTER);
    ioctl(file, INFO, &mut info)?;
    if info.kind != 2
        || info.count != 2
        || info.access & 3 != 3
        || info.value[0] != 0
        || info.value[1] != 255
        || !(0..=1).contains(&info.value[2])
        || info.id.iface != 2
        || info.id.name[..MASTER.len()] != *MASTER
        || info.id.name[MASTER.len()] != 0
        || info.id.index != 0
    {
        return Err(io::Error::other("unsupported master mixer layout"));
    }
    Ok(info)
}
pub struct Alsa {
    file: File,
}
impl Alsa {
    pub fn open() -> io::Result<Self> {
        if !crate::identity::Evidence::load(Path::new("/")).is_cucumber() {
            return Err(io::Error::other(
                "Cucumber volume adapter requires verified device-tree evidence",
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open("/dev/snd/controlC0")?;
        if !file.metadata()?.file_type().is_char_device() {
            return Err(io::Error::other("ALSA control must be a character device"));
        }
        // SAFETY: CardInfo contains integers/byte arrays, for which zero is valid.
        let mut card: CardInfo = unsafe { std::mem::zeroed() };
        ioctl(&file, CARD, &mut card)?;
        let name = &card.name[..card
            .name
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(card.name.len())];
        if name != b"AML-AUGESOUND" {
            return Err(io::Error::other("unexpected ALSA card"));
        }
        info(&file)?;
        Ok(Self { file })
    }
}
impl Mixer for Alsa {
    fn read(&mut self) -> io::Result<[i64; 2]> {
        let info = info(&self.file)?;
        // SAFETY: ElemValue contains integers/arrays; flags=0 selects inline values.
        let mut value: ElemValue = unsafe { std::mem::zeroed() };
        value.id = info.id;
        ioctl(&self.file, READ, &mut value)?;
        let raw = [value.value[0] as i64, value.value[1] as i64];
        if raw.iter().any(|v| !(0..=255).contains(v)) {
            return Err(io::Error::other("ALSA volume outside verified range"));
        }
        Ok(raw)
    }
    fn write(&mut self, raw: [i64; 2]) -> io::Result<()> {
        if raw.iter().any(|v| !(0..=255).contains(v)) {
            return Err(io::Error::other("invalid raw volume"));
        }
        let info = info(&self.file)?;
        // SAFETY: same fully initialized inline UAPI structure as in read().
        let mut value: ElemValue = unsafe { std::mem::zeroed() };
        value.id = info.id;
        for (out, input) in value.value.iter_mut().zip(raw) {
            *out = input as libc::c_long;
        }
        ioctl(&self.file, WRITE, &mut value)
    }
}
