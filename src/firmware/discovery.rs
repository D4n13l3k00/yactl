use serde::{
    Deserializer,
    de::{self, DeserializeSeed, IgnoredAny, MapAccess, Visitor},
};
use std::{fmt, fs::OpenOptions, io::Read, os::unix::fs::OpenOptionsExt, path::Path};

pub(super) fn read(path: &Path, selected: [bool; 2]) -> Result<[Option<u16>; 2], &'static str> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| "discovery_unavailable")?;
    if !file
        .metadata()
        .map_err(|_| "discovery_unavailable")?
        .is_file()
    {
        return Err("discovery_unavailable");
    }
    let mut bytes = Vec::new();
    file.take(262145)
        .read_to_end(&mut bytes)
        .map_err(|_| "discovery_unavailable")?;
    parse(&bytes, selected).map_err(|_| "invalid_discovery_config")
}
struct Ports([bool; 2]);
impl<'de> DeserializeSeed<'de> for Ports {
    type Value = [Option<u16>; 2];
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}
impl<'de> Visitor<'de> for Ports {
    type Value = [Option<u16>; 2];
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a firmware service map")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut ports = [None, None];
        let mut seen = [false, false];
        let mut fields = 0;
        while let Some(name) = map.next_key::<String>()? {
            fields += 1;
            if fields > 128 {
                return Err(de::Error::custom("too many firmware fields"));
            }
            let selected = match name.as_str() {
                "aliced" if self.0[0] => Some(0),
                "alarmd" if self.0[1] => Some(1),
                _ => None,
            };
            if let Some(index) = selected {
                if seen[index] {
                    return Err(de::Error::custom("duplicate service"));
                }
                seen[index] = true;
                ports[index] = map.next_value_seed(Port)?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(ports)
    }
}
struct Port;
impl<'de> DeserializeSeed<'de> for Port {
    type Value = Option<u16>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}
impl<'de> Visitor<'de> for Port {
    type Value = Option<u16>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a service port map")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut port = None;
        let mut fields = 0;
        while let Some(name) = map.next_key::<String>()? {
            fields += 1;
            if fields > 128 {
                return Err(de::Error::custom("too many service fields"));
            }
            if name == "port" {
                if port.is_some() {
                    return Err(de::Error::custom("duplicate service port"));
                }
                let number = map.next_value::<u16>()?;
                if number == 0 {
                    return Err(de::Error::custom("invalid service port"));
                }
                port = Some(number);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(port)
    }
}
fn parse(bytes: &[u8], selected: [bool; 2]) -> Result<[Option<u16>; 2], ()> {
    if bytes.len() > 262144 {
        return Err(());
    }
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let ports = Ports(selected).deserialize(&mut decoder).map_err(|_| ())?;
    decoder.end().map_err(|_| ())?;
    Ok(ports)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normal_station_sized_configuration_can_have_ignored_data_before_the_port() {
        let bytes =
            serde_json::json!({"ignored":"x".repeat(75500),"aliced":{"port":9875}}).to_string();
        assert_eq!(
            parse(bytes.as_bytes(), [true, false]),
            Ok([Some(9875), None])
        );
    }
    #[test]
    fn discovery_returns_only_selected_numeric_ports_and_ignores_other_values() {
        let bytes=br#"{"ignored":{"token":"synthetic-value"},"aliced":{"port":9875,"other":[1,2]},"alarmd":{"port":"not-requested"}}"#;
        assert_eq!(parse(bytes, [true, false]), Ok([Some(9875), None]));
        assert_eq!(
            parse(br#"{"aliced":{},"alarmd":{"port":9876}}"#, [true, true]),
            Ok([None, Some(9876)])
        );
        for bytes in [
            br#"{"aliced":{"port":0}}"#.as_slice(),
            br#"{"aliced":{"port":65536}}"#,
            br#"{"aliced":{"port":"9875"}}"#,
            br#"{"aliced":{"port":1,"port":2}}"#,
            br#"{"aliced":{},"aliced":{}}"#,
            br#"{"aliced":{"port":9875}} trailing"#,
        ] {
            assert!(parse(bytes, [true, false]).is_err());
        }
        assert!(parse(&vec![b' '; 262145], [true, false]).is_err());
    }
    #[test]
    fn file_errors_do_not_expose_paths_or_configuration() {
        assert_eq!(
            read(Path::new("/nonexistent/yactl-fixture"), [true, false]),
            Err("discovery_unavailable")
        );
        assert_eq!(
            read(Path::new("/dev/null"), [true, false]),
            Err("discovery_unavailable")
        );
    }
}
