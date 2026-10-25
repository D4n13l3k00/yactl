//! Explicit local firmware subscriptions; no outbound protocol commands.
mod decode;
mod discovery;
use crate::{events::Journal, settings};
use decode::Service;
use serde_json::{Value, json};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{io::AsyncReadExt, net::TcpStream, task::JoinHandle};

struct SourceState {
    service: Service,
    connected: bool,
    verified: bool,
    session_verified: bool,
    published: u64,
    dropped: u64,
    last_error: Option<&'static str>,
}
impl Service {
    fn source(self) -> &'static str {
        match self {
            Self::Aliced => "firmware:aliced",
            Self::Alarmd => "firmware:alarmd",
        }
    }
    fn id(self) -> &'static str {
        match self {
            Self::Aliced => "events.firmware.aliced.v1",
            Self::Alarmd => "events.firmware.alarmd.v1",
        }
    }
}
impl SourceState {
    fn ready(&self) -> bool {
        self.connected && self.session_verified
    }
    fn health(&self) -> Value {
        json!({"id":self.service.id(),"supported":self.verified,"ready":self.ready(),"connected":self.connected,"simulated":false,"protocol":"cucumber_ipc_v1","published_events":self.published,"dropped_events":self.dropped,"last_error":self.last_error})
    }
}
pub struct Firmware {
    sources: Vec<Arc<Mutex<SourceState>>>,
    tasks: Vec<JoinHandle<()>>,
}
pub struct Prepared {
    sources: Vec<(Service, Result<SocketAddr, &'static str>)>,
}
impl Firmware {
    pub fn prepare(config: &settings::Firmware) -> Prepared {
        Self::prepare_at(config, Path::new("/system/vendor/quasar/quasar.cfg"))
    }
    fn prepare_at(config: &settings::Firmware, path: &Path) -> Prepared {
        let selected = [
            config.aliced.is_none() && config.discover.contains(&settings::FirmwareService::Aliced),
            config.alarmd.is_none() && config.discover.contains(&settings::FirmwareService::Alarmd),
        ];
        let ports = if selected.iter().any(|value| *value) {
            discovery::read(path, selected)
        } else {
            Ok([None, None])
        };
        let mut sources = Vec::new();
        for (index, (service, address)) in [
            (Service::Aliced, config.aliced),
            (Service::Alarmd, config.alarmd),
        ]
        .into_iter()
        .enumerate()
        {
            if let Some(address) = address {
                sources.push((service, Ok(address)));
            } else if selected[index] {
                let address = ports.and_then(|ports| {
                    ports[index]
                        .map(|port| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
                        .ok_or("service_port_missing")
                });
                sources.push((service, address));
            }
        }
        Prepared { sources }
    }
    pub fn start(prepared: Prepared, journal: Arc<Journal>) -> Self {
        let mut result = Self {
            sources: Vec::new(),
            tasks: Vec::new(),
        };
        for (service, address) in prepared.sources {
            let state = Arc::new(Mutex::new(SourceState {
                service,
                connected: false,
                verified: false,
                session_verified: false,
                published: 0,
                dropped: 0,
                last_error: address.err(),
            }));
            result.sources.push(state.clone());
            let Ok(address) = address else {
                continue;
            };
            result.tasks.push(tokio::spawn(subscribe(
                service,
                address,
                state,
                journal.clone(),
            )));
        }
        result
    }
    pub fn health(&self) -> Vec<Value> {
        self.sources
            .iter()
            .map(|state| state.lock().unwrap().health())
            .collect()
    }
    pub fn capabilities(&self) -> Vec<Value> {
        self.sources.iter().filter_map(|state| {
            let state = state.lock().unwrap();
            state.verified.then(|| json!({"id":state.service.id(),"ready":state.ready(),"simulated":false,"source":state.service.source(),"protocol":"cucumber_ipc_v1","events":"/v1/events"}))
        }).collect()
    }
    pub fn shutdown(&self) {
        for task in &self.tasks {
            task.abort();
        }
        for state in &self.sources {
            let mut state = state.lock().unwrap();
            state.connected = false;
            state.session_verified = false;
            state.last_error = Some("stopped");
        }
    }
}
impl Drop for Firmware {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn read_frame(socket: &mut TcpStream) -> Result<Vec<u8>, &'static str> {
    let mut header = [0; 4];
    // A quiet source may stay connected indefinitely. Once the first header
    // byte arrives, one total deadline covers remaining header and payload.
    socket
        .read_exact(&mut header[..1])
        .await
        .map_err(|_| "connection_closed")?;
    tokio::time::timeout(Duration::from_secs(2), async {
        socket
            .read_exact(&mut header[1..])
            .await
            .map_err(|_| "connection_closed")?;
        let size = u32::from_le_bytes(header) as usize;
        if size > 65536 {
            return Err("oversized_frame");
        }
        let mut payload = vec![0; size];
        socket
            .read_exact(&mut payload)
            .await
            .map_err(|_| "connection_closed")?;
        Ok(payload)
    })
    .await
    .map_err(|_| "frame_timeout")?
}
async fn consume(
    service: Service,
    socket: &mut TcpStream,
    state: &Mutex<SourceState>,
    journal: &Journal,
) -> &'static str {
    let mut burst = 0;
    loop {
        let payload = match read_frame(socket).await {
            Ok(payload) => payload,
            Err(error) => return error,
        };
        let event = match decode::decode(service, &payload) {
            Ok(event) => event,
            Err(()) => return "invalid_frame",
        };
        if let Some((kind, data)) = event {
            let published = journal.publish(service.source(), kind, data);
            if published == Err(crate::events::Error::Closed) {
                return "stopped";
            }
            {
                let mut state = state.lock().unwrap();
                state.verified = true;
                state.session_verified = true;
                if published.is_ok() {
                    state.published = state.published.saturating_add(1);
                    state.last_error = None;
                } else {
                    state.dropped = state.dropped.saturating_add(1);
                    state.last_error = Some("event_too_large");
                }
            }
            if published.is_err() {
                let _ = journal.publish(
                    service.source(),
                    "source_loss",
                    json!({"reason":"event_too_large","count":1}),
                );
            }
        }
        burst += 1;
        if burst == 32 {
            // Even a loopback peer flooding irrelevant packets must yield CPU.
            tokio::time::sleep(Duration::from_millis(10)).await;
            burst = 0;
        }
    }
}
async fn subscribe(
    service: Service,
    address: SocketAddr,
    state: Arc<Mutex<SourceState>>,
    journal: Arc<Journal>,
) {
    loop {
        let connected =
            tokio::time::timeout(Duration::from_millis(500), TcpStream::connect(address)).await;
        let error = match connected {
            Ok(Ok(mut socket)) => {
                {
                    let mut state = state.lock().unwrap();
                    state.connected = true;
                    state.session_verified = false;
                    state.last_error = None;
                }
                let _ = journal.publish(
                    service.source(),
                    "source_connected",
                    json!({"protocol":"cucumber_ipc_v1"}),
                );
                let error = consume(service, &mut socket, &state, &journal).await;
                let _ = journal.publish(
                    service.source(),
                    "source_loss",
                    json!({"reason":error,"count":null}),
                );
                error
            }
            _ => "connection_failed",
        };
        {
            let mut state = state.lock().unwrap();
            state.connected = false;
            state.session_verified = false;
            state.last_error = Some(error);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[test]
    fn discovery_keeps_explicit_addresses_and_reports_only_selected_port_failures() {
        let path =
            std::env::temp_dir().join(format!("yactl-discovery-plan-{}", std::process::id()));
        std::fs::write(
            &path,
            br#"{"aliced":{"port":"unused-invalid"},"alarmd":{},"ignored":"synthetic"}"#,
        )
        .unwrap();
        let config = settings::Firmware {
            aliced: Some("127.0.0.1:9875".parse().unwrap()),
            alarmd: None,
            discover: vec![
                settings::FirmwareService::Aliced,
                settings::FirmwareService::Alarmd,
            ],
        };
        let plan = Firmware::prepare_at(&config, &path);
        assert_eq!(plan.sources[0].1, Ok("127.0.0.1:9875".parse().unwrap()));
        assert_eq!(plan.sources[1].1, Err("service_port_missing"));
        let config = settings::Firmware {
            aliced: None,
            alarmd: None,
            discover: vec![settings::FirmwareService::Aliced],
        };
        let plan = Firmware::prepare_at(&config, &path);
        assert_eq!(plan.sources[0].1, Err("invalid_discovery_config"));
        let provider = Firmware::start(plan, Arc::new(Journal::default()));
        assert!(provider.capabilities().is_empty());
        assert_eq!(
            provider.health()[0]["last_error"],
            "invalid_discovery_config"
        );
        assert_eq!(provider.health()[0]["ready"], false);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn subscription_is_read_only_and_verified_only_after_a_decoded_event() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = settings::Firmware {
            aliced: Some(listener.local_addr().unwrap()),
            alarmd: None,
            ..settings::Firmware::default()
        };
        let journal = Arc::new(Journal::default());
        let provider = Firmware::start(Firmware::prepare(&config), journal.clone());
        assert!(provider.capabilities().is_empty());
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut byte = [0];
        assert!(
            tokio::time::timeout(Duration::from_millis(30), socket.read(&mut byte))
                .await
                .is_err(),
            "subscription sent a command"
        );
        assert!(!provider.health()[0]["supported"].as_bool().unwrap());
        // One header plus one payload, deliberately fragmented.
        socket.write_all(&[6]).await.unwrap();
        socket.write_all(&[0, 0, 0, 0x1a, 4]).await.unwrap();
        socket.write_all(&[0x12, 2, b'h', b'i']).await.unwrap();
        let mut cursor = 0;
        loop {
            let batch = journal
                .poll(cursor, 16, Duration::from_secs(1))
                .await
                .unwrap();
            assert!(!batch.events.is_empty());
            cursor = batch.cursor;
            if let Some(event) = batch.events.iter().find(|event| event.kind == "speech") {
                assert_eq!(event.source, "firmware:aliced");
                assert_eq!(
                    event.data,
                    serde_json::json!({"source":"voice","transcript":"hi"})
                );
                break;
            }
        }
        assert_eq!(
            provider.capabilities()[0]["id"],
            "events.firmware.aliced.v1"
        );
        assert_eq!(provider.health()[0]["ready"], true);
        provider.shutdown();
        assert_eq!(provider.health()[0]["ready"], false);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), socket.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }
}
