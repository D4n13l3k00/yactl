//! Cucumber white-segment clock. Its ownership is independent of RGB lighting.
mod backend;
use crate::{
    resource::{Adapter, Config, Resource, ResourceFrame},
    settings::ClockBackend,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

pub fn segment_names() -> Vec<String> {
    let mut names = Vec::with_capacity(45);
    for position in 1..=4 {
        for part in b'a'..=b'g' {
            names.push(format!("{}{position}", char::from(part)));
        }
    }
    for name in [
        "h1", "h2", "l1", "l2", "m1", "m2", "p1", "p2", "p3", "p4", "p5", "q1", "q2", "q3", "q4",
        "q5", "deg",
    ] {
        names.push(name.into());
    }
    names.sort();
    names
}
#[derive(Clone, Debug, Serialize)]
pub struct Frame {
    pub sequence: u64,
    pub segments: BTreeMap<String, u8>,
}
impl Frame {
    fn blank() -> Self {
        Self {
            sequence: 0,
            segments: segment_names().into_iter().map(|name| (name, 0)).collect(),
        }
    }
}
impl ResourceFrame for Frame {
    fn sequence(&self) -> u64 {
        self.sequence
    }
    fn validate(&self, details: &serde_json::Value) -> Result<(), &'static str> {
        let limits = details["brightness_limits"]
            .as_object()
            .ok_or("provider_unavailable")?;
        for (name, value) in &self.segments {
            let max = limits
                .get(name)
                .and_then(serde_json::Value::as_u64)
                .ok_or("invalid_clock_segment")?;
            if u64::from(*value) > max {
                return Err("invalid_brightness");
            }
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Command {
    Time {
        sequence: u64,
        time: String,
        #[serde(default = "default_brightness")]
        brightness: u8,
    },
    Segments {
        sequence: u64,
        segments: BTreeMap<String, u8>,
    },
}
fn default_brightness() -> u8 {
    170
}
impl Command {
    pub fn into_frame(self) -> Result<Frame, &'static str> {
        let mut frame = Frame::blank();
        match self {
            Self::Segments { sequence, segments } => {
                frame.sequence = sequence;
                for (name, value) in segments {
                    let Some(out) = frame.segments.get_mut(&name) else {
                        return Err("invalid_clock_segment");
                    };
                    *out = value;
                }
            }
            Self::Time {
                sequence,
                time,
                brightness,
            } => {
                let bytes = time.as_bytes();
                if bytes.len() != 5
                    || bytes[2] != b':'
                    || ![bytes[0], bytes[1], bytes[3], bytes[4]]
                        .iter()
                        .all(u8::is_ascii_digit)
                {
                    return Err("invalid_clock_time");
                }
                let digits = [
                    bytes[0] - b'0',
                    bytes[1] - b'0',
                    bytes[3] - b'0',
                    bytes[4] - b'0',
                ];
                if digits[0] * 10 + digits[1] > 23 || digits[2] * 10 + digits[3] > 59 {
                    return Err("invalid_clock_time");
                }
                // Conventional seven-segment masks, low bit through high bit = a..g.
                const DIGITS: [u8; 10] =
                    [0x3f, 0x06, 0x5b, 0x4f, 0x66, 0x6d, 0x7d, 0x07, 0x7f, 0x6f];
                frame.sequence = sequence;
                for (position, digit) in digits.into_iter().enumerate() {
                    for part in 0..7 {
                        if DIGITS[usize::from(digit)] & (1 << part) != 0 {
                            *frame
                                .segments
                                .get_mut(&format!("{}{}", char::from(b'a' + part), position + 1))
                                .unwrap() = brightness;
                        }
                    }
                }
                *frame.segments.get_mut("p3").unwrap() = brightness;
                *frame.segments.get_mut("q3").unwrap() = brightness;
            }
        }
        Ok(frame)
    }
}
pub type Clock = Resource<Frame>;
pub fn start(kind: ClockBackend) -> Clock {
    Resource::start(
        Config {
            enabled: kind != ClockBackend::Off,
            simulated: kind == ClockBackend::Simulate,
            id: "display.clock.segment.v1",
            thread: "yactl-clock",
            details: json!({"layout":"cucumber","segments":segment_names(),"brightness_max":255,"time_format":"HH:MM"}),
        },
        Frame::blank(),
        move || {
            backend::Backend::open(kind).map(|backend| Box::new(backend) as Box<dyn Adapter<Frame>>)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clock_time_has_exact_digits_separators_and_zeroed_extra_segments() {
        let frame = Command::Time {
            sequence: 3,
            time: "12:34".into(),
            brightness: 60,
        }
        .into_frame()
        .unwrap();
        assert_eq!(frame.sequence, 3);
        assert_eq!(frame.segments.len(), 45);
        let lit = frame
            .segments
            .iter()
            .filter(|(_, value)| **value != 0)
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            lit,
            vec![
                "a2", "a3", "b1", "b2", "b3", "b4", "c1", "c3", "c4", "d2", "d3", "e2", "f4", "g2",
                "g3", "g4", "p3", "q3"
            ]
        );
        assert!(frame.segments.values().all(|v| *v == 0 || *v == 60));
        for time in ["24:00", "23:60", "1:23", "aa:bb", "12:34\n", "１２:３４"] {
            assert!(
                Command::Time {
                    sequence: 1,
                    time: time.into(),
                    brightness: 1
                }
                .into_frame()
                .is_err()
            );
        }
    }
    #[test]
    fn raw_frames_are_complete_and_unknown_paths_are_rejected() {
        let frame = Command::Segments {
            sequence: 1,
            segments: BTreeMap::from([("a1".into(), 42)]),
        }
        .into_frame()
        .unwrap();
        assert_eq!(frame.segments["a1"], 42);
        assert_eq!(frame.segments["b1"], 0);
        assert!(
            Command::Segments {
                sequence: 1,
                segments: BTreeMap::from([("../escape".into(), 1)])
            }
            .into_frame()
            .is_err()
        );
        for input in [
            r#"{"sequence":1,"time":"12:34","segments":{}}"#,
            r#"{"sequence":1,"time":"12:34","brightness":256}"#,
            r#"{"sequence":1,"segments":{},"unknown":1}"#,
        ] {
            assert!(serde_json::from_str::<Command>(input).is_err());
        }
    }
}
