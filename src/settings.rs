use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Read},
    net::{IpAddr, SocketAddr},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    #[default]
    Loopback,
    Lan,
    All,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Read,
    Hardware,
    Services,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Model {
    Midi,
    Unknown,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Device {
    pub display_name: Option<String>,
    pub model: Option<Model>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LightingBackend {
    #[default]
    Off,
    Simulate,
    Framebuffer,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Lighting {
    pub backend: LightingBackend,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockBackend {
    #[default]
    Off,
    Simulate,
    Sysfs,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Clock {
    pub backend: ClockBackend,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensorsBackend {
    #[default]
    Off,
    Simulate,
    Sysfs,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Sensors {
    pub backend: SensorsBackend,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeBackend {
    #[default]
    Off,
    Simulate,
    Alsa,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Volume {
    pub backend: VolumeBackend,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Firmware {
    pub aliced: Option<SocketAddr>,
    pub alarmd: Option<SocketAddr>,
    pub discover: Vec<FirmwareService>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FirmwareService {
    Aliced,
    Alarmd,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputBackend {
    #[default]
    Off,
    Simulate,
    Evdev,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Input {
    pub backend: InputBackend,
    pub devices: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Key {
    pub file: PathBuf,
    pub scopes: Vec<Scope>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Http {
    pub enabled: bool,
    pub listen: SocketAddr,
    pub access: Access,
    pub allow: Vec<String>,
}
impl Default for Http {
    fn default() -> Self {
        Self {
            enabled: true,
            listen: "127.0.0.1:8098".parse().unwrap(),
            access: Access::Loopback,
            allow: vec![],
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub version: u32,
    pub http: Http,
    pub keys: Vec<Key>,
    pub device: Device,
    pub lighting: Lighting,
    pub clock: Clock,
    pub sensors: Sensors,
    pub volume: Volume,
    pub firmware: Firmware,
    pub input: Input,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            http: Http::default(),
            keys: vec![],
            device: Device::default(),
            lighting: Lighting::default(),
            clock: Clock::default(),
            sensors: Sensors::default(),
            volume: Volume::default(),
            firmware: Firmware::default(),
            input: Input::default(),
        }
    }
}
fn invalid(text: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, text.to_string())
}

fn range(text: &str) -> io::Result<(IpAddr, u32)> {
    let (address, bits) = text
        .split_once('/')
        .ok_or_else(|| invalid("client ranges need an explicit CIDR prefix"))?;
    let address: IpAddr = address.parse().map_err(invalid)?;
    let bits: u32 = bits.parse().map_err(invalid)?;
    if bits > if address.is_ipv4() { 32 } else { 128 } {
        return Err(invalid("invalid CIDR prefix"));
    }
    Ok((address, bits))
}
fn contains(text: &str, peer: IpAddr) -> bool {
    match range(text) {
        Ok((IpAddr::V4(net), bits)) => match peer {
            IpAddr::V4(ip) => {
                bits == 0 || (u32::from(net) >> (32 - bits)) == (u32::from(ip) >> (32 - bits))
            }
            _ => false,
        },
        Ok((IpAddr::V6(net), bits)) => match peer {
            IpAddr::V6(ip) => {
                bits == 0 || (u128::from(net) >> (128 - bits)) == (u128::from(ip) >> (128 - bits))
            }
            _ => false,
        },
        _ => false,
    }
}
impl Settings {
    pub fn parse(text: &str) -> io::Result<Self> {
        let settings: Self = serde_json::from_str(text).map_err(invalid)?;
        if settings.device.display_name.as_ref().is_some_and(|name| {
            name.trim().is_empty() || name.len() > 128 || name.chars().any(char::is_control)
        }) {
            return Err(invalid(
                "device display name must be 1..128 bytes without control characters",
            ));
        }
        if settings.version != 1 || settings.keys.len() > 32 || settings.http.allow.len() > 64 {
            return Err(invalid(
                "unsupported config version or oversized access policy",
            ));
        }
        for cidr in &settings.http.allow {
            range(cidr)?;
        }
        if settings.input.devices.len() > 16
            || settings
                .input
                .devices
                .iter()
                .enumerate()
                .any(|(index, name)| {
                    name.is_empty()
                        || name.len() > 63
                        || name.chars().any(char::is_control)
                        || settings.input.devices[..index].contains(name)
                })
        {
            return Err(invalid(
                "input devices require up to 16 unique names of 1..63 bytes without controls",
            ));
        }
        if settings.input.backend == InputBackend::Evdev && settings.input.devices.is_empty() {
            return Err(invalid("evdev requires explicit input device names"));
        }
        if settings.input.backend == InputBackend::Simulate && !settings.input.devices.is_empty() {
            return Err(invalid("input simulation uses its own device selection"));
        }
        if settings.firmware.discover.len() > 2
            || settings.firmware.discover.first().is_some_and(|service| {
                settings
                    .firmware
                    .discover
                    .iter()
                    .skip(1)
                    .any(|other| other == service)
            })
        {
            return Err(invalid("firmware discovery services must be unique"));
        }
        for address in [settings.firmware.aliced, settings.firmware.alarmd]
            .into_iter()
            .flatten()
        {
            if !address.ip().is_loopback() || address.port() == 0 {
                return Err(invalid(
                    "firmware sources require explicit loopback addresses with nonzero ports",
                ));
            }
        }
        if settings.http.enabled {
            if settings.http.access == Access::Loopback && !settings.http.listen.ip().is_loopback()
            {
                return Err(invalid("loopback mode requires a loopback listener"));
            }
            if settings.http.access != Access::Loopback && settings.keys.is_empty() {
                return Err(invalid("remote HTTP requires authentication keys"));
            }
            if settings.http.access == Access::Lan && settings.http.allow.is_empty() {
                return Err(invalid("LAN mode requires explicit client CIDRs"));
            }
        }
        for key in &settings.keys {
            if !key.file.is_absolute()
                || key
                    .file
                    .components()
                    .any(|p| p == std::path::Component::ParentDir)
                || key.scopes.is_empty()
            {
                return Err(invalid(
                    "keys require an absolute file path and explicit scopes",
                ));
            }
        }
        Ok(settings)
    }
    pub fn load(root: &Path) -> io::Result<Self> {
        match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(root.join("settings.json"))
        {
            Ok(file) => {
                if !file.metadata()?.is_file() {
                    return Err(invalid("settings must be a regular file"));
                }
                let mut text = String::new();
                file.take(65537).read_to_string(&mut text)?;
                if text.len() > 65536 {
                    return Err(invalid("settings exceed 64 KiB"));
                }
                Self::parse(&text)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }
    pub fn peer_allowed(&self, peer: IpAddr) -> bool {
        match self.http.access {
            Access::Loopback => peer.is_loopback(),
            Access::Lan => {
                peer.is_loopback() || self.http.allow.iter().any(|cidr| contains(cidr, peer))
            }
            Access::All => true,
        }
    }
    pub fn credentials(&self) -> io::Result<Vec<(Vec<u8>, Vec<Scope>)>> {
        self.keys
            .iter()
            .map(|key| {
                let file = fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(&key.file)?;
                let meta = file.metadata()?;
                if !meta.is_file()
                    || meta.permissions().mode() & 0o077 != 0
                    || meta.uid() != unsafe { libc::geteuid() }
                {
                    return Err(invalid(
                        "token file must be private and owned by the API user",
                    ));
                }
                let mut token = String::new();
                file.take(258).read_to_string(&mut token)?;
                let token = token.trim();
                if !(32..=256).contains(&token.len())
                    || !token
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                {
                    return Err(invalid(
                        "token must be 32..256 ASCII letters, digits, underscores or hyphens",
                    ));
                }
                Ok((token.as_bytes().to_vec(), key.scopes.clone()))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_input_requires_an_explicit_bounded_device_selection() {
        assert!(
            Settings::parse(r#"{"input":{"backend":"evdev","devices":["mute","awinic_sar"]}}"#)
                .is_ok()
        );
        assert!(Settings::parse(r#"{"input":{"backend":"evdev"}}"#).is_err());
        for devices in [
            vec!["mute".to_owned(), "mute".to_owned()],
            vec!["bad\nname".into()],
            vec!["".into()],
            vec!["x".repeat(64)],
            (0..17).map(|n| format!("input-{n}")).collect(),
        ] {
            assert!(
                Settings::parse(
                    &serde_json::json!({"input":{"backend":"evdev","devices":devices}}).to_string()
                )
                .is_err()
            );
        }
        assert!(Settings::parse(r#"{"input":{"backend":"simulate","devices":["mute"]}}"#).is_err());
    }
    #[test]
    fn firmware_sources_are_opt_in_and_loopback_only() {
        assert!(
            Settings::parse(r#"{"firmware":{"aliced":"127.0.0.1:9875","alarmd":"[::1]:9876"}}"#)
                .is_ok()
        );
        for address in [
            "0.0.0.0:9875",
            "192.168.0.1:9875",
            "127.0.0.1:0",
            "localhost:9875",
            "[::ffff:127.0.0.1]:9875",
        ] {
            assert!(
                Settings::parse(&serde_json::json!({"firmware":{"aliced":address}}).to_string())
                    .is_err()
            );
        }
    }
    #[test]
    fn firmware_discovery_is_explicit_and_rejects_duplicate_services() {
        assert!(Settings::parse(r#"{"firmware":{"discover":["aliced","alarmd"]}}"#).is_ok());
        assert!(Settings::parse(r#"{"firmware":{"discover":["aliced","aliced"]}}"#).is_err());
        assert!(Settings::parse(r#"{"firmware":{"discover":["other"]}}"#).is_err());
        assert!(Settings::default().firmware.discover.is_empty());
    }
    #[test]
    fn device_settings_validate_display_name_and_model() {
        for name in [
            "".to_owned(),
            "  ".to_owned(),
            "bad\nname".to_owned(),
            "x".repeat(129),
        ] {
            let text = serde_json::json!({"device":{"display_name":name}}).to_string();
            assert!(Settings::parse(&text).is_err());
        }
        assert!(Settings::parse(r#"{"device":{"model":"unverified"}}"#).is_err());
        let settings =
            Settings::parse(r#"{"device":{"display_name":"Kitchen","model":"midi"}}"#).unwrap();
        assert_eq!(settings.device.display_name.as_deref(), Some("Kitchen"));
        assert_eq!(settings.device.model, Some(Model::Midi));
    }
    #[test]
    fn safe_defaults_and_remote_validation() {
        let settings = Settings::parse("{}").unwrap();
        assert!(settings.http.listen.ip().is_loopback());
        assert!(Settings::parse(r#"{"http":{"listen":"0.0.0.0:8098"}}"#).is_err());
        assert!(Settings::parse(r#"{"http":{"access":"all","listen":"0.0.0.0:8098"}}"#).is_err());
        assert!(
            Settings::parse(
                r#"{"http":{"access":"lan"},"keys":[{"file":"/token","scopes":["read"]}]}"#
            )
            .is_err()
        );
        assert!(Settings::parse(r#"{"unknown":true}"#).is_err());
    }
    #[test]
    fn cidr_families_are_explicit_and_bounded() {
        let settings=Settings::parse(r#"{"http":{"access":"lan","listen":"0.0.0.0:8098","allow":["192.168.10.0/24"]},"keys":[{"file":"/token","scopes":["read"]}]}"#).unwrap();
        assert!(settings.peer_allowed("192.168.10.5".parse().unwrap()));
        assert!(!settings.peer_allowed("192.168.11.5".parse().unwrap()));
        assert!(!settings.peer_allowed("::ffff:192.168.10.5".parse().unwrap()));
        assert!(range("0.0.0.0/33").is_err());
    }
}
