//! Midi RGB frames; ownership is implemented by the shared resource worker.
mod backend;
use crate::{
    resource::{Adapter, Config, Resource, ResourceFrame},
    settings::LightingBackend,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Frame {
    pub sequence: u64,
    pub ring: [[u8; 3]; 24],
    pub logo: [[u8; 3]; 6],
}
impl Frame {
    fn blank() -> Self {
        Self {
            sequence: 0,
            ring: [[0; 3]; 24],
            logo: [[0; 3]; 6],
        }
    }
    fn bgr(&self) -> (Vec<u8>, Vec<u8>) {
        let bytes = |pixels: &[[u8; 3]]| pixels.iter().flat_map(|c| [c[2], c[1], c[0]]).collect();
        (bytes(&self.ring), bytes(&self.logo))
    }
    fn from_bgr(ring: &[u8], logo: &[u8]) -> Self {
        let mut frame = Self::blank();
        for (out, input) in frame
            .ring
            .iter_mut()
            .zip(ring.as_chunks::<3>().0.iter())
            .chain(frame.logo.iter_mut().zip(logo.as_chunks::<3>().0.iter()))
        {
            *out = [input[2], input[1], input[0]];
        }
        frame
    }
}

impl ResourceFrame for Frame {
    fn sequence(&self) -> u64 {
        self.sequence
    }
}
pub type Lighting = Resource<Frame>;
pub fn start(kind: LightingBackend) -> Lighting {
    Resource::start(
        Config {
            enabled: kind != LightingBackend::Off,
            simulated: kind == LightingBackend::Simulate,
            id: "lighting.rgb.v1",
            thread: "yactl-lighting",
            details: json!({"ring_pixels":24,"logo_pixels":6,"color_order":"rgb"}),
        },
        Frame::blank(),
        move || backend::Backend::open(kind).map(|b| Box::new(b) as Box<dyn Adapter<Frame>>),
    )
}
impl Adapter<Frame> for backend::Backend {
    fn capture(&mut self) -> std::io::Result<()> {
        self.capture()
    }
    fn write(&mut self, frame: &Frame) -> std::io::Result<()> {
        self.write(frame)
    }
    fn restore(&mut self) -> std::io::Result<()> {
        self.restore()
    }
    fn current(&self) -> Frame {
        self.current()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frame_encoding_preserves_rgb_and_rejects_incomplete_frames() {
        let mut frame = Frame::blank();
        frame.ring[0] = [1, 2, 3];
        frame.logo[0] = [4, 5, 6];
        let (ring, logo) = frame.bgr();
        assert_eq!(&ring[..3], &[3, 2, 1]);
        assert_eq!(&logo[..3], &[6, 5, 4]);
        let restored = Frame::from_bgr(&ring, &logo);
        assert_eq!(restored.ring[0], [1, 2, 3]);
        assert_eq!(restored.logo[0], [4, 5, 6]);
        assert!(serde_json::from_str::<Frame>(r#"{"sequence":1,"ring":[],"logo":[]}"#).is_err());
        let mut invalid = serde_json::to_value(frame).unwrap();
        invalid["ring"][0][0] = json!(256);
        assert!(serde_json::from_value::<Frame>(invalid).is_err());
    }
}
