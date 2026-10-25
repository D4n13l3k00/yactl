//! Read-only sensor polling; HTTP reads bounded, timestamped snapshots only.
use crate::settings::SensorsBackend;
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

const MAX_SENSORS: usize = 16;
const MAX_ENTRIES: usize = 128;
const MAX_ATTRIBUTE: u64 = 128;
const STALE: Duration = Duration::from_secs(3);
#[derive(Clone, Copy)]
enum Decode {
    Raw,
    Thermal,
    Illuminance,
}
impl Decode {
    fn parse(self, text: &str) -> io::Result<f64> {
        let invalid = || io::Error::other("invalid sensor reading");
        match self {
            Self::Raw => text
                .trim()
                .parse::<u32>()
                .map(f64::from)
                .map_err(|_| invalid()),
            Self::Thermal => {
                let raw = text.trim().parse::<i64>().map_err(|_| invalid())?;
                if !(-273150..=1_000_000).contains(&raw) {
                    return Err(invalid());
                }
                Ok(raw as f64 / 1000.0)
            }
            Self::Illuminance => text
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v >= 0.0)
                .ok_or_else(invalid),
        }
    }
}
fn text(path: &Path) -> io::Result<String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("sensor attributes must be regular files"));
    }
    let mut value = String::new();
    file.take(MAX_ATTRIBUTE + 1).read_to_string(&mut value)?;
    if value.len() as u64 > MAX_ATTRIBUTE {
        return Err(io::Error::other("sensor attribute too large"));
    }
    Ok(value.trim().to_owned())
}
#[derive(Clone)]
enum Source {
    Attribute(PathBuf, Decode),
    Constant(f64),
}
#[derive(Clone)]
struct Descriptor {
    id: String,
    kind: &'static str,
    unit: &'static str,
    label: String,
    source: Source,
    identity: Option<(PathBuf, String)>,
}
impl Descriptor {
    fn read(&self) -> io::Result<f64> {
        if let Some((path, expected)) = &self.identity
            && text(path)? != *expected
        {
            return Err(io::Error::other(
                "sensor identity changed; restart to rediscover",
            ));
        }
        match &self.source {
            Source::Attribute(path, decode) => decode.parse(&text(path)?),
            Source::Constant(value) => Ok(*value),
        }
    }
}
struct Cached {
    descriptor: Descriptor,
    verified: bool,
    value: Option<f64>,
    sampled: Option<Instant>,
    error: Option<&'static str>,
}
impl Cached {
    fn new(descriptor: Descriptor) -> Self {
        Self {
            descriptor,
            verified: false,
            value: None,
            sampled: None,
            error: None,
        }
    }
    fn update(&mut self, result: io::Result<f64>, now: Instant) {
        match result {
            Ok(value) => {
                self.verified = true;
                self.value = Some(value);
                self.sampled = Some(now);
                self.error = None;
            }
            Err(_) => {
                self.value = None;
                self.sampled = None;
                self.error = Some("read_failed");
            }
        }
    }
    fn view(&self, now: Instant, simulated: bool) -> Value {
        let age = self.sampled.map(|at| now.saturating_duration_since(at));
        let ready =
            self.value.is_some() && self.error.is_none() && age.is_some_and(|age| age < STALE);
        let error = if !ready && self.error.is_none() && self.sampled.is_some() {
            Some("stale_reading")
        } else {
            self.error
        };
        json!({"id":self.descriptor.id,"kind":self.descriptor.kind,"unit":self.descriptor.unit,"label":self.descriptor.label,"simulated":simulated,"supported":self.verified,"ready":ready,"value":if ready {self.value}else {None},"age_ms":age.map(|age|age.as_millis() as u64),"error":error})
    }
}
struct State {
    readings: Vec<Cached>,
    stopped: bool,
    truncated: bool,
    error: Option<String>,
}
#[derive(Clone)]
pub struct Sensors {
    state: Arc<Mutex<State>>,
    stop: mpsc::SyncSender<()>,
    kind: SensorsBackend,
}
impl Sensors {
    pub fn start(kind: SensorsBackend) -> Self {
        let state = Arc::new(Mutex::new(State {
            readings: vec![],
            stopped: kind == SensorsBackend::Off,
            truncated: false,
            error: None,
        }));
        let (stop, receive) = mpsc::sync_channel(1);
        let handle = Self {
            state: state.clone(),
            stop,
            kind,
        };
        if kind != SensorsBackend::Off {
            match thread::Builder::new()
                .name("yactl-sensors".into())
                .spawn(move || {
                    let mut truncated = false;
                    let descriptors = if kind == SensorsBackend::Simulate {
                        simulated()
                    } else {
                        discover(Path::new("/"), &mut truncated)
                    };
                    {
                        let mut state = state.lock().unwrap();
                        state.truncated = truncated;
                        state.readings = descriptors.iter().cloned().map(Cached::new).collect();
                    }
                    loop {
                        for (index, descriptor) in descriptors.iter().enumerate() {
                            let result = descriptor.read();
                            let now = Instant::now();
                            state.lock().unwrap().readings[index].update(result, now);
                        }
                        match receive.recv_timeout(Duration::from_secs(1)) {
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                            _ => break,
                        }
                    }
                    state.lock().unwrap().stopped = true;
                }) {
                Ok(_) => {}
                Err(error) => {
                    let mut state = handle.state.lock().unwrap();
                    state.error = Some(error.to_string());
                    state.stopped = true;
                }
            }
        }
        handle
    }
    pub fn list(&self) -> Vec<Value> {
        let now = Instant::now();
        self.state
            .lock()
            .unwrap()
            .readings
            .iter()
            .map(|cached| cached.view(now, self.kind == SensorsBackend::Simulate))
            .collect()
    }
    pub fn get(&self, id: &str) -> Option<Value> {
        let state = self.state.lock().unwrap();
        state
            .readings
            .iter()
            .find(|cached| cached.descriptor.id == id)
            .map(|cached| cached.view(Instant::now(), self.kind == SensorsBackend::Simulate))
    }
    pub fn capability(&self) -> Option<Value> {
        let readings = self
            .list()
            .into_iter()
            .filter(|value| value["supported"] == true)
            .collect::<Vec<_>>();
        if readings.is_empty() {
            return None;
        }
        let ready = readings.iter().any(|value| value["ready"] == true);
        Some(
            json!({"id":"sensors.read.v1","supported":true,"ready":ready,"simulated":self.kind==SensorsBackend::Simulate,"sensors":readings,"max_sensors":MAX_SENSORS,"poll_ms":1000,"stale_ms":3000}),
        )
    }
    pub fn health(&self) -> Option<Value> {
        if self.kind == SensorsBackend::Off {
            return None;
        }
        let list = self.list();
        let state = self.state.lock().unwrap();
        Some(
            json!({"id":"sensors.read.v1","supported":list.iter().any(|r|r["supported"]==true),"ready":list.iter().any(|r|r["ready"]==true),"simulated":self.kind==SensorsBackend::Simulate,"error":state.error,"discovery_truncated":state.truncated}),
        )
    }
    pub fn shutdown(&self) {
        let _ = self.stop.try_send(());
    }
    pub async fn wait_stopped(&self) {
        let _ = tokio::time::timeout(Duration::from_millis(500), async {
            while !self.state.lock().unwrap().stopped {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
    }
}
fn simulated() -> Vec<Descriptor> {
    [
        (
            "ambient-light:simulated",
            "light_intensity",
            "raw",
            "Simulated light intensity",
            1234.0,
        ),
        (
            "temperature:simulated",
            "temperature",
            "celsius",
            "Simulated thermal zone",
            0.5,
        ),
        (
            "adc:simulated:0",
            "adc",
            "raw",
            "Simulated ADC channel",
            100.0,
        ),
    ]
    .into_iter()
    .map(|(id, kind, unit, label, value)| Descriptor {
        id: id.into(),
        kind,
        unit,
        label: label.into(),
        source: Source::Constant(value),
        identity: None,
    })
    .collect()
}
fn directories(path: &Path, truncated: &mut bool) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(path) else {
        return vec![];
    };
    let mut paths = entries
        .take(MAX_ENTRIES + 1)
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    if paths.len() > MAX_ENTRIES {
        *truncated = true;
        paths.truncate(MAX_ENTRIES);
    }
    paths.retain(|path| path.is_dir());
    paths.sort();
    paths
}
fn index(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix).is_some_and(|suffix| {
        !suffix.is_empty() && suffix.len() <= 4 && suffix.bytes().all(|b| b.is_ascii_digit())
    })
}
fn add(out: &mut Vec<Descriptor>, descriptor: Descriptor, truncated: &mut bool) {
    if let Source::Attribute(path, _) = &descriptor.source
        && !path.is_file()
    {
        return;
    }
    if out.len() >= MAX_SENSORS {
        *truncated = true;
    } else {
        out.push(descriptor);
    }
}
fn discover(system: &Path, truncated: &mut bool) -> Vec<Descriptor> {
    let mut sensors = Vec::new();
    for directory in directories(&system.join("sys/class/thermal"), truncated) {
        let Some(name) = directory.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !index(name, "thermal_zone") {
            continue;
        }
        let Ok(label) = text(&directory.join("type")) else {
            continue;
        };
        if label.is_empty() {
            continue;
        }
        add(
            &mut sensors,
            Descriptor {
                id: format!("temperature:{name}"),
                kind: "temperature",
                unit: "celsius",
                identity: Some((directory.join("type"), label.clone())),
                label,
                source: Source::Attribute(directory.join("temp"), Decode::Thermal),
            },
            truncated,
        );
    }
    for directory in directories(&system.join("sys/bus/iio/devices"), truncated) {
        let Some(name) = directory.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !index(name, "iio:device") {
            continue;
        }
        let Ok(driver) = text(&directory.join("name")) else {
            continue;
        };
        if driver == "ltr308als01" {
            // Only the standard processed illuminance attribute has established lux units.
            let (attribute, kind, unit, decode) =
                if directory.join("in_illuminance_input").is_file() {
                    (
                        "in_illuminance_input",
                        "illuminance",
                        "lux",
                        Decode::Illuminance,
                    )
                } else {
                    ("in_intensity_input", "light_intensity", "raw", Decode::Raw)
                };
            add(
                &mut sensors,
                Descriptor {
                    id: format!("ambient-light:{name}"),
                    kind,
                    unit,
                    identity: Some((directory.join("name"), driver.clone())),
                    label: driver,
                    source: Source::Attribute(directory.join(attribute), decode),
                },
                truncated,
            );
        } else if driver == "meson-c2-saradc" {
            for channel in 0..32 {
                add(
                    &mut sensors,
                    Descriptor {
                        id: format!("adc:{name}:{channel}"),
                        kind: "adc",
                        unit: "raw",
                        label: format!("{driver} channel {channel}"),
                        identity: Some((directory.join("name"), driver.clone())),
                        source: Source::Attribute(
                            directory.join(format!("in_voltage{channel}_raw")),
                            Decode::Raw,
                        ),
                    },
                    truncated,
                );
            }
        }
    }
    sensors
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "yactl-sensors-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        fn write(&self, path: &str, value: &str) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, value).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    #[test]
    fn thermal_units_are_normalized_even_close_to_zero() {
        for (raw, expected) in [
            ("500", 0.5),
            ("-500", -0.5),
            ("0", 0.0),
            ("999", 0.999),
            ("33500", 33.5),
        ] {
            assert_eq!(Decode::Thermal.parse(raw).unwrap(), expected);
        }
        for raw in ["NaN", "inf", "-inf", "1e999", "", "bad"] {
            assert!(Decode::Thermal.parse(raw).is_err());
            assert!(Decode::Raw.parse(raw).is_err());
            assert!(Decode::Illuminance.parse(raw).is_err());
        }
        assert!(Decode::Thermal.parse("-274000").is_err());
        assert!(Decode::Raw.parse("-1").is_err());
        assert!(Decode::Raw.parse("4.5").is_err());
        assert_eq!(Decode::Illuminance.parse("12.5").unwrap(), 12.5);
    }
    #[test]
    fn cached_values_are_hidden_after_failure_or_staleness() {
        let descriptor = simulated().remove(0);
        let mut cached = Cached::new(descriptor);
        let now = Instant::now();
        assert_eq!(cached.view(now, true)["supported"], false);
        cached.update(Ok(1234.0), now - STALE);
        let stale = cached.view(now, true);
        assert_eq!(stale["supported"], true);
        assert_eq!(stale["ready"], false);
        assert!(stale["value"].is_null());
        assert_eq!(stale["error"], "stale_reading");
        cached.update(Err(io::Error::other("unavailable")), now);
        let failed = cached.view(now, true);
        assert_eq!(failed["supported"], true);
        assert!(failed["value"].is_null());
        assert_eq!(failed["error"], "read_failed");
        cached.update(Ok(42.0), now);
        let recovered = cached.view(now, true);
        assert_eq!(recovered["value"], 42.0);
        assert_eq!(recovered["ready"], true);
        assert!(recovered["error"].is_null());
    }
    #[test]
    fn sysfs_discovery_uses_driver_and_attribute_evidence_for_units() {
        let fixture = Fixture::new();
        fixture.write("sys/class/thermal/thermal_zone0/type", "cpu_thermal");
        fixture.write("sys/class/thermal/thermal_zone0/temp", "500");
        fixture.write("sys/bus/iio/devices/iio:device0/name", "ltr308als01");
        fixture.write("sys/bus/iio/devices/iio:device0/in_intensity_input", "1234");
        fixture.write("sys/bus/iio/devices/iio:device1/name", "meson-c2-saradc");
        fixture.write("sys/bus/iio/devices/iio:device1/in_voltage0_raw", "100");
        fixture.write("sys/bus/iio/devices/iio:device1/in_temp_raw", "900");
        fixture.write("sys/bus/iio/devices/iio:device2/name", "unverified");
        fixture.write(
            "sys/bus/iio/devices/iio:device2/in_illuminance_input",
            "999",
        );
        fixture.write("sys/bus/iio/devices/iio:device3/name", "ltr308als01");
        fixture.write(
            "sys/bus/iio/devices/iio:device3/in_illuminance_input",
            "12.5",
        );
        let mut truncated = false;
        let found = discover(&fixture.0, &mut truncated);
        assert!(!truncated);
        assert_eq!(found.len(), 4);
        let temperature = found
            .iter()
            .find(|d| d.id == "temperature:thermal_zone0")
            .unwrap();
        assert_eq!(temperature.unit, "celsius");
        assert_eq!(temperature.read().unwrap(), 0.5);
        let light = found
            .iter()
            .find(|d| d.id == "ambient-light:iio:device0")
            .unwrap();
        assert_eq!(light.unit, "raw");
        assert_eq!(light.kind, "light_intensity");
        assert_eq!(light.read().unwrap(), 1234.0);
        let adc = found.iter().find(|d| d.id == "adc:iio:device1:0").unwrap();
        assert_eq!(adc.kind, "adc");
        assert_eq!(adc.unit, "raw");
        assert_eq!(adc.read().unwrap(), 100.0);
        let lux = found
            .iter()
            .find(|d| d.id == "ambient-light:iio:device3")
            .unwrap();
        assert_eq!(lux.unit, "lux");
        assert_eq!(lux.read().unwrap(), 12.5);
        fixture.write(
            "sys/bus/iio/devices/iio:device0/name",
            "unverified-replacement",
        );
        assert!(
            light.read().is_err(),
            "a replaced device retained the previous provider identity"
        );
    }
    #[test]
    fn discovery_and_attributes_are_bounded_and_symlinks_are_not_read() {
        let fixture = Fixture::new();
        for index in 0..MAX_SENSORS + 2 {
            fixture.write(
                &format!("sys/class/thermal/thermal_zone{index}/type"),
                "cpu",
            );
            fixture.write(
                &format!("sys/class/thermal/thermal_zone{index}/temp"),
                "1000",
            );
        }
        let mut truncated = false;
        assert_eq!(discover(&fixture.0, &mut truncated).len(), MAX_SENSORS);
        assert!(truncated);
        fixture.write("oversized", &"1".repeat(MAX_ATTRIBUTE as usize + 1));
        assert!(text(&fixture.0.join("oversized")).is_err());
        fixture.write("value", "123");
        std::os::unix::fs::symlink(fixture.0.join("value"), fixture.0.join("alias")).unwrap();
        assert!(text(&fixture.0.join("alias")).is_err());
        assert!(text(&fixture.0).is_err());
    }
}
