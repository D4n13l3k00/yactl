//! Read-only, bounded input events. No exclusive grabs or input injection.
mod evdev;
use crate::events::Journal;
use crate::settings::InputBackend;
use serde::Serialize;
use serde_json::json;
use std::{
    io,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize)]
struct Descriptor {
    id: String,
    name: String,
    keys: Vec<u16>,
    switches: Vec<u16>,
}
#[derive(Clone, Debug, Default, Serialize)]
struct Snapshot {
    keys_down: Vec<u16>,
    switches_on: Vec<u16>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Record {
    kind: u16,
    code: u16,
    value: i32,
}
fn records(bytes: &[u8]) -> io::Result<Vec<Record>> {
    if !bytes.len().is_multiple_of(24) || bytes.len() > 24 * 32 {
        return Err(io::Error::other("invalid input read size"));
    }
    Ok(bytes
        .as_chunks::<24>()
        .0
        .iter()
        .map(|record| Record {
            kind: u16::from_ne_bytes(record[16..18].try_into().unwrap()),
            code: u16::from_ne_bytes(record[18..20].try_into().unwrap()),
            value: i32::from_ne_bytes(record[20..24].try_into().unwrap()),
        })
        .collect())
}
trait Source: Send {
    fn descriptor(&self) -> Descriptor;
    fn read(&mut self) -> io::Result<Vec<Record>>;
    fn snapshot(&self) -> io::Result<Snapshot>;
}
struct Device {
    source: Box<dyn Source>,
    descriptor: Descriptor,
    current: Snapshot,
    discard: bool,
    ready: bool,
    error: Option<&'static str>,
    polled: Instant,
}
fn ordered_subset(values: &[u16], allowed: &[u16]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
        && values
            .iter()
            .all(|value| allowed.binary_search(value).is_ok())
}
impl Snapshot {
    fn validate(&self, descriptor: &Descriptor) -> io::Result<()> {
        if !ordered_subset(&self.keys_down, &descriptor.keys)
            || !ordered_subset(&self.switches_on, &descriptor.switches)
        {
            return Err(io::Error::other("invalid input state"));
        }
        Ok(())
    }
}
impl Device {
    fn open(source: Box<dyn Source>, journal: &Journal) -> io::Result<Self> {
        let descriptor = source.descriptor();
        if descriptor.keys.len() > 64
            || descriptor.switches.len() > 32
            || !descriptor.keys.windows(2).all(|v| v[0] < v[1])
            || !descriptor.switches.windows(2).all(|v| v[0] < v[1])
            || descriptor.keys.iter().any(|v| *v >= 768)
            || descriptor.switches.iter().any(|v| *v >= 32)
        {
            return Err(io::Error::other("unsupported input capabilities"));
        }
        let current = source.snapshot()?;
        current.validate(&descriptor)?;
        let result = Self {
            source,
            descriptor,
            current,
            discard: false,
            ready: true,
            error: None,
            polled: Instant::now(),
        };
        result.publish_snapshot(journal, "initial")?;
        Ok(result)
    }
    fn publish_snapshot(&self, journal: &Journal, reason: &str) -> io::Result<()> {
        self.publish(journal,"input_snapshot", json!({"reason":reason,"keys_down":self.current.keys_down,"switches_on":self.current.switches_on}))
    }
    fn publish(&self, journal: &Journal, kind: &str, data: serde_json::Value) -> io::Result<()> {
        journal
            .publish(&self.descriptor.id, kind, data)
            .map(|_| ())
            .map_err(|_| io::Error::other("input event publication failed"))
    }
    fn poll(&mut self, journal: &Journal) -> io::Result<bool> {
        let records = self.source.read()?;
        let changed = !records.is_empty();
        if records.len() > 32 {
            return Err(io::Error::other("input source exceeded read budget"));
        }
        for record in records {
            if record.kind == 0 {
                if record.code == 3 {
                    self.discard = true;
                    self.ready = false;
                    self.publish(
                        journal,
                        "source_loss",
                        json!({"reason":"syn_dropped","count":null}),
                    )?;
                } else if record.code == 0 && self.discard {
                    let current = self.source.snapshot()?;
                    current.validate(&self.descriptor)?;
                    self.current = current;
                    self.discard = false;
                    self.ready = true;
                    self.publish_snapshot(journal, "resync")?;
                }
                continue;
            }
            if self.discard {
                continue;
            }
            let (kind, allowed, current, max) = match record.kind {
                1 => ("key", &self.descriptor.keys, &mut self.current.keys_down, 2),
                5 => (
                    "switch",
                    &self.descriptor.switches,
                    &mut self.current.switches_on,
                    1,
                ),
                _ => continue,
            };
            if allowed.binary_search(&record.code).is_err() || !(0..=max).contains(&record.value) {
                return Err(io::Error::other("invalid input event"));
            }
            match current.binary_search(&record.code) {
                Ok(index) if record.value == 0 => {
                    current.remove(index);
                }
                Err(index) if record.value != 0 => current.insert(index, record.code),
                _ => {}
            }
            self.publish(
                journal,
                kind,
                json!({"type":record.kind,"code":record.code,"value":record.value}),
            )?;
        }
        self.polled = Instant::now();
        Ok(changed)
    }
}

struct Simulated {
    pressed: bool,
    next: Instant,
}
impl Source for Simulated {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            id: "input:simulated".into(),
            name: "simulated".into(),
            keys: vec![115],
            switches: vec![14],
        }
    }
    fn read(&mut self) -> io::Result<Vec<Record>> {
        if Instant::now() < self.next {
            return Ok(Vec::new());
        }
        self.next = Instant::now() + Duration::from_millis(100);
        self.pressed = !self.pressed;
        Ok(vec![
            Record {
                kind: 1,
                code: 115,
                value: i32::from(self.pressed),
            },
            Record {
                kind: 5,
                code: 14,
                value: i32::from(self.pressed),
            },
        ])
    }
    fn snapshot(&self) -> io::Result<Snapshot> {
        Ok(if self.pressed {
            Snapshot {
                keys_down: vec![115],
                switches_on: vec![14],
            }
        } else {
            Snapshot::default()
        })
    }
}
#[derive(Clone)]
struct Cached {
    descriptor: Descriptor,
    current: Snapshot,
    ready: bool,
    error: Option<&'static str>,
    polled: Instant,
}
impl Cached {
    fn from_device(device: &Device) -> Self {
        Self {
            descriptor: device.descriptor.clone(),
            current: device.current.clone(),
            ready: device.ready,
            error: device.error,
            polled: device.polled,
        }
    }
    fn stale(&self) -> bool {
        self.polled.elapsed() >= Duration::from_secs(3)
    }
    fn is_ready(&self) -> bool {
        self.ready && !self.stale()
    }
    fn last_error(&self) -> Option<&'static str> {
        self.error.or_else(|| self.stale().then_some("input_stale"))
    }
    fn view(&self) -> serde_json::Value {
        json!({"id":self.descriptor.id,"name":self.descriptor.name,"keys":self.descriptor.keys,"switches":self.descriptor.switches,"ready":self.is_ready(),"error":self.last_error(),"age_ms":self.polled.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,"state":if self.is_ready() {serde_json::to_value(&self.current).unwrap()} else {serde_json::Value::Null}})
    }
}
struct State {
    devices: Vec<Cached>,
    error: Option<&'static str>,
    stopped: bool,
}
#[derive(Clone)]
pub struct Input {
    state: Arc<Mutex<State>>,
    stop: mpsc::SyncSender<()>,
    kind: InputBackend,
    selected: Vec<String>,
}
impl Input {
    pub fn start(config: &crate::settings::Input, cucumber: bool, journal: Arc<Journal>) -> Self {
        let kind = config.backend;
        let selected = config.devices.clone();
        let mut input = Self::start_sources(kind, cucumber, journal, move || match kind {
            InputBackend::Simulate => vec![Box::new(Simulated {
                pressed: false,
                next: Instant::now() + Duration::from_millis(100),
            }) as Box<dyn Source>],
            InputBackend::Evdev => evdev::discover(&selected),
            InputBackend::Off => Vec::new(),
        });
        input.selected = config.devices.clone();
        input
    }
    fn start_sources(
        kind: InputBackend,
        cucumber: bool,
        journal: Arc<Journal>,
        sources: impl FnOnce() -> Vec<Box<dyn Source>> + Send + 'static,
    ) -> Self {
        let (stop, receive) = mpsc::sync_channel(1);
        let state = Arc::new(Mutex::new(State {
            devices: Vec::new(),
            error: None,
            stopped: kind == InputBackend::Off,
        }));
        let handle = Self {
            state: state.clone(),
            stop,
            kind,
            selected: Vec::new(),
        };
        if kind == InputBackend::Off {
            return handle;
        }
        if kind == InputBackend::Evdev && !cucumber {
            let mut state = state.lock().unwrap();
            state.error = Some("unsupported_device");
            state.stopped = true;
            return handle;
        }
        let worker_state = state.clone();
        if thread::Builder::new()
            .name("yactl-input".into())
            .spawn(move || {
                let sources = sources();
                let mut devices = Vec::new();
                let mut stopping = false;
                for source in sources {
                    if !matches!(receive.try_recv(), Err(mpsc::TryRecvError::Empty)) {
                        stopping = true;
                        break;
                    }
                    if let Ok(device) = Device::open(source, &journal) {
                        devices.push(device);
                    }
                }
                {
                    let mut state = worker_state.lock().unwrap();
                    state.devices = devices.iter().map(Cached::from_device).collect();
                    if devices.is_empty() {
                        state.error = Some("no_verified_input_devices");
                    }
                }
                while !stopping && !devices.is_empty() {
                    match receive.recv_timeout(Duration::from_millis(20)) {
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        _ => break,
                    }
                    for (index, device) in devices.iter_mut().enumerate() {
                        if device.error.is_some() {
                            continue;
                        }
                        let changed = match device.poll(&journal) {
                            Ok(changed) => changed,
                            Err(_) => {
                                device.ready = false;
                                device.error = Some("input_read_failed");
                                let _ = journal.publish(
                                    &device.descriptor.id,
                                    "source_loss",
                                    json!({"reason":"input_read_failed","count":null}),
                                );
                                true
                            }
                        };
                        if changed {
                            worker_state.lock().unwrap().devices[index] =
                                Cached::from_device(device);
                        } else {
                            worker_state.lock().unwrap().devices[index].polled = device.polled;
                        }
                    }
                }
                let mut state = worker_state.lock().unwrap();
                state.stopped = true;
                for device in &mut state.devices {
                    device.ready = false;
                }
            })
            .is_err()
        {
            let mut state = state.lock().unwrap();
            state.error = Some("input_worker_start_failed");
            state.stopped = true;
        }
        handle
    }
    pub fn status(&self) -> serde_json::Value {
        let state = self.state.lock().unwrap();
        let missing = if self.kind == InputBackend::Evdev {
            self.selected
                .iter()
                .filter(|name| {
                    !state
                        .devices
                        .iter()
                        .any(|device| &device.descriptor.name == *name)
                })
                .cloned()
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        json!({"id":"input.events.v1","supported":!state.devices.is_empty(),"ready":state.devices.iter().any(Cached::is_ready),"simulated":self.kind==InputBackend::Simulate,"error":state.error.or_else(||state.devices.iter().find_map(Cached::last_error)),"stale_ms":3000,"missing_devices":missing,"devices":state.devices.iter().map(Cached::view).collect::<Vec<_>>()})
    }
    pub fn health(&self) -> Option<serde_json::Value> {
        if self.kind == InputBackend::Off {
            return None;
        }
        let mut value = self.status();
        value.as_object_mut().unwrap().remove("devices");
        Some(value)
    }
    pub fn capability(&self) -> Option<serde_json::Value> {
        let state = self.state.lock().unwrap();
        if state.devices.is_empty() {
            return None;
        }
        Some(
            json!({"id":"input.events.v1","ready":state.devices.iter().any(Cached::is_ready),"simulated":self.kind==InputBackend::Simulate,"stale_ms":3000,"events":"/v1/events","status":"/v1/input","devices":state.devices.iter().map(|device|json!({"id":device.descriptor.id,"name":device.descriptor.name,"keys":device.descriptor.keys,"switches":device.descriptor.switches,"ready":device.is_ready()})).collect::<Vec<_>>()}),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
        time::Duration,
    };
    #[test]
    fn stale_input_cache_never_claims_a_current_pressed_state() {
        let cached = Cached {
            descriptor: Descriptor {
                id: "input:test".into(),
                name: "test".into(),
                keys: vec![115],
                switches: vec![14],
            },
            current: Snapshot {
                keys_down: vec![115],
                switches_on: vec![14],
            },
            ready: true,
            error: None,
            polled: Instant::now() - Duration::from_secs(4),
        };
        let view = cached.view();
        assert_eq!(view["ready"], false);
        assert_eq!(view["state"], serde_json::Value::Null);
        assert_eq!(view["error"], "input_stale");
    }
    struct Fake {
        queue: VecDeque<Vec<Record>>,
        sampled: Arc<Mutex<usize>>,
    }
    impl Source for Fake {
        fn descriptor(&self) -> Descriptor {
            Descriptor {
                id: "input:test".into(),
                name: "test".into(),
                keys: vec![115],
                switches: vec![14],
            }
        }
        fn read(&mut self) -> io::Result<Vec<Record>> {
            Ok(self.queue.pop_front().unwrap_or_default())
        }
        fn snapshot(&self) -> io::Result<Snapshot> {
            let mut sampled = self.sampled.lock().unwrap();
            *sampled += 1;
            Ok(if *sampled == 1 {
                Snapshot::default()
            } else {
                Snapshot {
                    keys_down: vec![115],
                    switches_on: vec![14],
                }
            })
        }
    }
    fn record(kind: u16, code: u16, value: i32) -> Record {
        Record { kind, code, value }
    }
    #[tokio::test]
    async fn dropped_events_are_discarded_until_report_then_snapshot_restores_state() {
        let sampled = Arc::new(Mutex::new(0));
        let source = Fake {
            sampled: sampled.clone(),
            queue: VecDeque::from([
                vec![record(1, 115, 1), record(0, 3, 0), record(1, 115, 0)],
                vec![record(5, 14, 1), record(0, 0, 0), record(1, 115, 2)],
            ]),
        };
        let journal = Journal::default();
        let mut device = Device::open(Box::new(source), &journal).unwrap();
        device.poll(&journal).unwrap();
        assert_eq!(
            *sampled.lock().unwrap(),
            1,
            "snapshot queried before SYN_REPORT"
        );
        device.poll(&journal).unwrap();
        assert_eq!(*sampled.lock().unwrap(), 2);
        let batch = journal.poll(0, 16, Duration::ZERO).await.unwrap();
        assert_eq!(
            batch
                .events
                .iter()
                .map(|e| e.kind.as_str())
                .collect::<Vec<_>>(),
            [
                "input_snapshot",
                "key",
                "source_loss",
                "input_snapshot",
                "key"
            ]
        );
        assert_eq!(batch.events[2].data["reason"], "syn_dropped");
        assert_eq!(batch.events[3].data["keys_down"], serde_json::json!([115]));
        assert_eq!(batch.events[3].data["switches_on"], serde_json::json!([14]));
        assert_eq!(batch.events[4].data["value"], 2);
    }
    #[tokio::test]
    async fn unsupported_codes_and_invalid_values_never_reach_readers() {
        for invalid in [
            record(1, 116, 1),
            record(1, 115, 3),
            record(5, 14, 2),
            record(1, 115, -1),
        ] {
            let source = Fake {
                sampled: Arc::new(Mutex::new(0)),
                queue: VecDeque::from([vec![record(2, 115, 1), invalid]]),
            };
            let journal = Journal::default();
            let mut device = Device::open(Box::new(source), &journal).unwrap();
            assert!(device.poll(&journal).is_err());
            let batch = journal.poll(0, 16, Duration::ZERO).await.unwrap();
            assert!(
                batch
                    .events
                    .iter()
                    .all(|event| event.kind != "key" && event.kind != "switch")
            );
        }
    }
    struct Gated {
        started: mpsc::SyncSender<()>,
        proceed: Mutex<mpsc::Receiver<()>>,
    }
    impl Source for Gated {
        fn descriptor(&self) -> Descriptor {
            Descriptor {
                id: "input:gated".into(),
                name: "gated".into(),
                keys: vec![115],
                switches: Vec::new(),
            }
        }
        fn read(&mut self) -> io::Result<Vec<Record>> {
            Ok(Vec::new())
        }
        fn snapshot(&self) -> io::Result<Snapshot> {
            self.started.send(()).unwrap();
            self.proceed
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(1))
                .unwrap();
            Ok(Snapshot::default())
        }
    }
    #[test]
    fn shutdown_during_probe_is_not_consumed_and_forgotten_before_polling() {
        let (started, wait) = mpsc::sync_channel(1);
        let (proceed, receive) = mpsc::channel();
        let samples = Arc::new(Mutex::new(0));
        let second_samples = samples.clone();
        let input = Input::start_sources(
            InputBackend::Simulate,
            true,
            Arc::new(Journal::default()),
            move || {
                vec![
                    Box::new(Gated {
                        started,
                        proceed: Mutex::new(receive),
                    }) as Box<dyn Source>,
                    Box::new(Fake {
                        queue: VecDeque::new(),
                        sampled: second_samples,
                    }),
                ]
            },
        );
        wait.recv_timeout(Duration::from_secs(1)).unwrap();
        input.shutdown();
        proceed.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_millis(300);
        while !input.state.lock().unwrap().stopped {
            assert!(
                Instant::now() < deadline,
                "shutdown was lost after the first probe"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            *samples.lock().unwrap(),
            0,
            "second source probed after shutdown"
        );
        assert_eq!(input.status()["ready"], false);
    }
    #[test]
    fn unknown_device_cannot_enable_native_input_support() {
        let input = Input::start(
            &crate::settings::Input {
                backend: InputBackend::Evdev,
                devices: vec!["mute".into()],
            },
            false,
            Arc::new(Journal::default()),
        );
        assert_eq!(input.status()["error"], "unsupported_device");
        assert_eq!(input.status()["devices"], json!([]));
        assert!(input.capability().is_none());
    }
    #[test]
    fn native_records_decode_without_alignment_assumptions_and_reject_partial_reads() {
        let mut bytes = [0u8; 24];
        bytes[16..18].copy_from_slice(&1u16.to_ne_bytes());
        bytes[18..20].copy_from_slice(&115u16.to_ne_bytes());
        bytes[20..24].copy_from_slice(&(-1i32).to_ne_bytes());
        assert_eq!(records(&bytes).unwrap(), [record(1, 115, -1)]);
        assert!(records(&bytes[..23]).is_err());
        assert!(records(&vec![0; 24 * 33]).is_err());
    }
}
