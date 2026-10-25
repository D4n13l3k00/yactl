//! Read-only device evidence. Model overrides never advertise hardware support.
use crate::settings::{Device, Model};
use serde_json::{Value, json};
use std::{fs::File, io::Read, path::Path};

fn text(path: &Path, limit: u64) -> Option<String> {
    let mut data = String::new();
    File::open(path)
        .ok()?
        .take(limit + 1)
        .read_to_string(&mut data)
        .ok()?;
    if data.len() as u64 > limit {
        return None;
    }
    Some(data.trim_end_matches(['\0', '\n', '\r']).to_owned())
}

pub struct Evidence {
    board: Option<String>,
    compatible: Vec<String>,
    kernel: Option<String>,
    firmware_id: Option<String>,
    firmware_version: Option<String>,
}
impl Evidence {
    pub fn load(system: &Path) -> Self {
        let board = text(&system.join("proc/device-tree/model"), 512);
        let compatible = text(&system.join("proc/device-tree/compatible"), 2048)
            .map(|s| {
                s.split('\0')
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let os = text(&system.join("etc/os-release"), 8192).unwrap_or_default();
        let field = |key: &str| {
            os.lines().find_map(|line| {
                let (name, value) = line.split_once('=')?;
                (name == key).then(|| value.trim_matches(['"', '\'']).to_owned())
            })
        };
        Self {
            board,
            compatible,
            kernel: text(&system.join("proc/sys/kernel/osrelease"), 256),
            firmware_id: field("ID"),
            firmware_version: field("VERSION_ID"),
        }
    }
    pub fn is_cucumber(&self) -> bool {
        self.board
            .as_deref()
            .is_some_and(|s| s.starts_with("Yandex Cucumber "))
            || self
                .compatible
                .iter()
                .any(|s| s == "yandex,cucumber" || s == "yandex,cucumber-dvt2")
    }
    pub fn describe(&self, instance_id: &str, config: &Device) -> Value {
        let detected = self.is_cucumber();
        let model = config.model.unwrap_or(if detected {
            Model::Midi
        } else {
            Model::Unknown
        });
        let revision = self
            .board
            .as_deref()
            .and_then(|s| s.strip_prefix("Yandex Cucumber "));
        json!({
            "instance_id": instance_id,
            "display_name": config.display_name.as_deref().unwrap_or("yactl"),
            "model": model,
            "model_source": if config.model.is_some() {"override"} else if detected {"detected"} else {"unknown"},
            "codename": if model == Model::Midi {Some("cucumber")} else {None},
            "board_model": self.board.as_deref().unwrap_or(""),
            "board_revision": revision,
            "architecture": std::env::consts::ARCH,
            "firmware": {"id":self.firmware_id, "version":self.firmware_version, "kernel":self.kernel},
            "evidence": {"device_tree_model":self.board, "compatible":self.compatible},
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn loads_bounded_device_tree_and_os_evidence() {
        let root = std::env::temp_dir().join(format!("yactl-identity-{}", std::process::id()));
        for directory in ["proc/device-tree", "proc/sys/kernel", "etc"] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        std::fs::write(
            root.join("proc/device-tree/model"),
            b"Yandex Cucumber DVT2\0",
        )
        .unwrap();
        std::fs::write(
            root.join("proc/device-tree/compatible"),
            b"yandex,cucumber\0amlogic,a5\0",
        )
        .unwrap();
        std::fs::write(root.join("proc/sys/kernel/osrelease"), "5.4.0\n").unwrap();
        std::fs::write(
            root.join("etc/os-release"),
            "ID=test\nVERSION_ID=\"1.2\"\nSECRET=ignored\n",
        )
        .unwrap();
        let device = Evidence::load(&root).describe("id", &Device::default());
        assert_eq!(device["board_model"], "Yandex Cucumber DVT2");
        assert_eq!(
            device["evidence"]["compatible"],
            json!(["yandex,cucumber", "amlogic,a5"])
        );
        assert_eq!(
            device["firmware"],
            json!({"id":"test", "version":"1.2", "kernel":"5.4.0"})
        );
        std::fs::write(root.join("proc/device-tree/model"), "x".repeat(513)).unwrap();
        assert!(Evidence::load(&root).describe("id", &Device::default())["evidence"]["device_tree_model"].is_null());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn detected_board_keeps_evidence_when_overridden() {
        let evidence = Evidence {
            board: Some("Yandex Cucumber DVT2".into()),
            compatible: vec!["yandex,cucumber-dvt2".into(), "yandex,cucumber".into()],
            kernel: Some("5.4.0".into()),
            firmware_id: Some("test".into()),
            firmware_version: Some("1.2".into()),
        };
        let detected = evidence.describe("abc", &Device::default());
        assert_eq!(detected["model"], "midi");
        assert_eq!(detected["model_source"], "detected");
        assert_eq!(detected["board_revision"], "DVT2");
        assert_eq!(detected["firmware"]["version"], "1.2");
        let changed = evidence.describe(
            "abc",
            &Device {
                display_name: Some("Kitchen".into()),
                model: Some(Model::Unknown),
            },
        );
        assert_eq!(changed["model"], "unknown");
        assert!(changed["codename"].is_null());
        assert_eq!(changed["evidence"], detected["evidence"]);
        assert_eq!(changed["instance_id"], "abc");
    }
    #[test]
    fn unknown_board_does_not_invent_metadata() {
        let evidence = Evidence {
            board: None,
            compatible: vec![],
            kernel: None,
            firmware_id: None,
            firmware_version: None,
        };
        let device = evidence.describe("id", &Device::default());
        assert_eq!(device["model"], "unknown");
        assert_eq!(device["model_source"], "unknown");
        assert!(device["board_revision"].is_null());
        assert!(device["firmware"]["version"].is_null());
        let compatible = Evidence {
            compatible: vec!["yandex,cucumber".into()],
            ..evidence
        };
        assert_eq!(
            compatible.describe("id", &Device::default())["model"],
            "midi"
        );
    }
    #[test]
    fn station_midi_codename_is_not_retail_midi() {
        let evidence = Evidence {
            board: Some("Yandex Station Midi".into()),
            compatible: vec!["yandex,station-midi".into(), "amlogic,axg".into()],
            kernel: None,
            firmware_id: None,
            firmware_version: None,
        };
        let device = evidence.describe("id", &Device::default());
        assert_eq!(device["model"], "unknown");
        assert!(device["codename"].is_null());
    }
}
