//! Midi framebuffer operations adapted from the independent midi-light driver.
use super::Frame;
use crate::settings::LightingBackend;
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::{fd::AsRawFd, unix::fs::FileExt},
};

struct Framebuffer {
    file: File,
    var: [u32; 40],
    count: usize,
}
impl Framebuffer {
    fn open(index: usize, name: &str, count: usize) -> io::Result<Self> {
        if fs::read_to_string(format!("/sys/class/graphics/fb{index}/name"))?.trim() != name {
            return Err(io::Error::other("unexpected framebuffer name"));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("/dev/fb{index}"))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::other("framebuffer is owned by another adapter"));
        }
        let mut var = [0u32; 40];
        // SAFETY: FBIOGET_VSCREENINFO writes a 160-byte fb_var_screeninfo buffer.
        if unsafe { libc::ioctl(file.as_raw_fd(), 0x4600, var.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if var[0] != count as u32 || var[1] != 1 || var[6] != 24 {
            return Err(io::Error::other("unexpected framebuffer geometry"));
        }
        Ok(Self { file, var, count })
    }
    fn read(&self) -> io::Result<Vec<u8>> {
        let mut bytes = vec![0; self.count * 3];
        self.file.read_exact_at(&mut bytes, 0)?;
        Ok(bytes)
    }
    fn write(&self, data: &[u8]) -> io::Result<()> {
        if data.len() != self.count * 3 {
            return Err(io::Error::other("invalid framebuffer length"));
        }
        self.file.write_all_at(data, 0)?;
        // SAFETY: FBIOPAN_DISPLAY reads the same validated fb_var_screeninfo.
        if unsafe { libc::ioctl(self.file.as_raw_fd(), 0x4606, self.var.as_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

pub struct Backend {
    hardware: Option<(Framebuffer, Framebuffer)>,
    original: Option<(Vec<u8>, Vec<u8>)>,
    last: Option<(Vec<u8>, Vec<u8>)>,
    simulated: Frame,
}
impl Backend {
    pub fn open(kind: LightingBackend) -> io::Result<Self> {
        let hardware = match kind {
            LightingBackend::Framebuffer => Some((
                Framebuffer::open(0, "ring", 24)?,
                Framebuffer::open(1, "logo", 6)?,
            )),
            LightingBackend::Simulate => None,
            LightingBackend::Off => return Err(io::Error::other("lighting disabled")),
        };
        Ok(Self {
            hardware,
            original: None,
            last: None,
            simulated: Frame::blank(),
        })
    }
    fn bytes(&self) -> io::Result<(Vec<u8>, Vec<u8>)> {
        if let Some((ring, logo)) = &self.hardware {
            Ok((ring.read()?, logo.read()?))
        } else {
            Ok(self.simulated.bgr())
        }
    }
    pub fn capture(&mut self) -> io::Result<()> {
        let (ring, logo) = self.bytes()?;
        self.simulated = Frame::from_bgr(&ring, &logo);
        self.original = Some((ring, logo));
        self.last = None;
        Ok(())
    }
    pub fn write(&mut self, frame: &Frame) -> io::Result<()> {
        let (ring, logo) = frame.bgr();
        // Track each intended zone even when a later write fails.
        self.last = Some((ring.clone(), logo.clone()));
        if let Some((r, l)) = &self.hardware {
            r.write(&ring)?;
            l.write(&logo)?;
        }
        self.simulated = frame.clone();

        Ok(())
    }
    pub fn restore(&mut self) -> io::Result<()> {
        if let Some((ring, logo)) = self.original.take()
            && let Some((last_ring, last_logo)) = &self.last
        {
            if let Some((r, l)) = &self.hardware {
                if r.read()? == *last_ring {
                    r.write(&ring)?;
                }
                if l.read()? == *last_logo {
                    l.write(&logo)?;
                }
                let (ring, logo) = self.bytes()?;
                self.simulated = Frame::from_bgr(&ring, &logo);
            } else if self.bytes()? == (last_ring.clone(), last_logo.clone()) {
                self.simulated = Frame::from_bgr(&ring, &logo);
            }
        }
        self.last = None;
        Ok(())
    }
    pub fn current(&self) -> Frame {
        self.simulated.clone()
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
    #[test]
    fn restores_snapshot_but_preserves_newer_external_output() {
        let mut backend = Backend::open(LightingBackend::Simulate).unwrap();
        let mut baseline = Frame::blank();
        baseline.ring[0] = [11, 22, 33];
        baseline.logo[0] = [44, 55, 66];
        backend.simulated = baseline.clone();
        backend.capture().unwrap();
        let mut frame = Frame::blank();
        frame.ring[0] = [1, 2, 3];
        frame.sequence = 1;
        backend.write(&frame).unwrap();
        backend.restore().unwrap();
        assert_eq!(backend.current().ring[0], [11, 22, 33]);
        assert_eq!(backend.current().logo[0], [44, 55, 66]);
        backend.capture().unwrap();
        backend.write(&frame).unwrap();
        backend.simulated.ring[0] = [99, 88, 77];
        backend.restore().unwrap();
        assert_eq!(backend.current().ring[0], [99, 88, 77]);
    }
}
