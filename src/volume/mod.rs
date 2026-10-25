//! One bounded volume worker; all ALSA operations stay outside Tokio executors.
mod alsa;
use crate::settings::VolumeBackend;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use tokio::sync::oneshot;
const STALE: Duration = Duration::from_secs(3);
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub percent: u8,
    pub lease_ms: Option<u16>,
}
impl Request {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.percent > 100 || self.lease_ms.is_some_and(|ms| !(100..=5000).contains(&ms)) {
            Err("invalid_volume")
        } else {
            Ok(())
        }
    }
}
trait Mixer: Send {
    fn read(&mut self) -> io::Result<[i64; 2]>;
    fn write(&mut self, raw: [i64; 2]) -> io::Result<()>;
}
struct Simulated([i64; 2]);
impl Mixer for Simulated {
    fn read(&mut self) -> io::Result<[i64; 2]> {
        Ok(self.0)
    }
    fn write(&mut self, raw: [i64; 2]) -> io::Result<()> {
        self.0 = raw;
        Ok(())
    }
}
struct Lease {
    baseline: [i64; 2],
    ours: [i64; 2],
    until: Instant,
}
struct Engine {
    mixer: Box<dyn Mixer>,
    lease: Option<Lease>,
}
impl Engine {
    fn apply(&mut self, request: &Request, now: Instant) -> io::Result<[i64; 2]> {
        request.validate().map_err(io::Error::other)?;
        let before = self.mixer.read()?;
        let baseline = self
            .lease
            .as_ref()
            .filter(|lease| lease.ours == before)
            .map_or(before, |lease| lease.baseline);
        let raw = (i64::from(request.percent) * 255 + 50) / 100;
        self.mixer.write([raw; 2])?;
        self.lease = request.lease_ms.map(|ms| Lease {
            baseline,
            ours: [raw; 2],
            until: now + Duration::from_millis(u64::from(ms)),
        });
        self.mixer.read()
    }
    fn restore(&mut self) -> io::Result<()> {
        if let Some(lease) = &self.lease
            && self.mixer.read()? == lease.ours
        {
            self.mixer.write(lease.baseline)?;
        }
        self.lease = None;
        Ok(())
    }
    fn expire(&mut self, now: Instant) -> io::Result<()> {
        if self.lease.as_ref().is_some_and(|lease| now >= lease.until) {
            self.restore()?;
        }
        Ok(())
    }
}
struct State {
    verified: bool,
    stopped: bool,
    raw: Option<[i64; 2]>,
    sampled: Option<Instant>,
    temporary_until: Option<Instant>,
    error: Option<String>,
}
impl State {
    fn new(stopped: bool) -> Self {
        Self {
            verified: false,
            stopped,
            raw: None,
            sampled: None,
            temporary_until: None,
            error: None,
        }
    }
    fn sample(&mut self, result: io::Result<[i64; 2]>, until: Option<Instant>) {
        self.temporary_until = until;
        match result {
            Ok(raw) => {
                self.verified = true;
                self.raw = Some(raw);
                self.sampled = Some(Instant::now());
                self.error = None;
            }
            Err(error) => {
                self.raw = None;
                self.sampled = None;
                self.error = Some(error.to_string());
            }
        }
    }
    fn view(&self, now: Instant, simulated: bool) -> Value {
        let age = self.sampled.map(|at| now.saturating_duration_since(at));
        let ready = !self.stopped
            && self.error.is_none()
            && self.raw.is_some()
            && age.is_some_and(|age| age < STALE);
        let raw = if ready { self.raw } else { None };
        let percent = raw.map(|raw| raw.map(|v| v as f64 * 100.0 / 255.0));
        json!({"supported":self.verified,"ready":ready,"simulated":simulated,"raw":raw,"channel_percent":percent,"percent":percent.map(|p|(p[0]+p[1])/2.0),"channels":2,"raw_min":0,"raw_max":255,"age_ms":age.map(|d|d.as_millis() as u64),"temporary":self.temporary_until.is_some(),"lease_remaining_ms":self.temporary_until.map(|at|at.saturating_duration_since(now).as_millis() as u64),"error":if !ready&&self.error.is_none()&&self.sampled.is_some() {Some("stale_reading".to_owned())}else {self.error.clone()}})
    }
}
struct Command {
    request: Request,
    reply: oneshot::Sender<Result<Value, &'static str>>,
    expires: Instant,
}
#[derive(Clone)]
pub struct Volume {
    state: Arc<Mutex<State>>,
    commands: mpsc::SyncSender<Command>,
    stop: Arc<AtomicBool>,
    kind: VolumeBackend,
}
impl Volume {
    pub fn start(kind: VolumeBackend) -> Self {
        let state = Arc::new(Mutex::new(State::new(kind == VolumeBackend::Off)));
        let stop = Arc::new(AtomicBool::new(false));
        let (commands, receive) = mpsc::sync_channel(8);
        let handle = Self {
            state: state.clone(),
            commands,
            stop: stop.clone(),
            kind,
        };
        if kind != VolumeBackend::Off {
            match thread::Builder::new()
                .name("yactl-volume".into())
                .spawn(move || {
                    worker(state.clone(), receive, stop, kind);
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
    pub fn status(&self) -> Value {
        self.state
            .lock()
            .unwrap()
            .view(Instant::now(), self.kind == VolumeBackend::Simulate)
    }
    pub async fn set(&self, request: Request) -> Result<Value, &'static str> {
        request.validate()?;
        if self.kind == VolumeBackend::Off {
            return Err("unsupported_hardware");
        }
        let (reply, receive) = oneshot::channel();
        self.commands
            .try_send(Command {
                request,
                reply,
                expires: Instant::now() + Duration::from_secs(1),
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => "busy",
                mpsc::TrySendError::Disconnected(_) => "provider_unavailable",
            })?;
        tokio::time::timeout(Duration::from_secs(1), receive)
            .await
            .map_err(|_| "provider_unavailable")?
            .map_err(|_| "provider_unavailable")?
    }
    pub fn capability(&self) -> Option<Value> {
        let status = self.status();
        if status["supported"] != true {
            return None;
        }
        Some(
            json!({"id":"audio.volume.v1","supported":true,"ready":status["ready"],"simulated":self.kind==VolumeBackend::Simulate,"channels":2,"percent_min":0,"percent_max":100,"raw_min":0,"raw_max":255,"temporary_lease_ms":[100,5000],"supports_mute":false,"scale":"linear"}),
        )
    }
    pub fn health(&self) -> Option<Value> {
        if self.kind == VolumeBackend::Off {
            return None;
        }
        let status = self.status();
        Some(
            json!({"id":"audio.volume.v1","supported":status["supported"],"ready":status["ready"],"simulated":status["simulated"],"error":status["error"]}),
        )
    }
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
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
fn worker(
    state: Arc<Mutex<State>>,
    receive: mpsc::Receiver<Command>,
    stop: Arc<AtomicBool>,
    kind: VolumeBackend,
) {
    let mixer: Box<dyn Mixer> = match kind {
        VolumeBackend::Simulate => Box::new(Simulated([64; 2])),
        VolumeBackend::Alsa => match alsa::Alsa::open() {
            Ok(alsa) => Box::new(alsa),
            Err(error) => {
                state.lock().unwrap().error = Some(error.to_string());
                return;
            }
        },
        VolumeBackend::Off => return,
    };
    serve(state, receive, stop, kind, Engine { mixer, lease: None });
}
fn serve(
    state: Arc<Mutex<State>>,
    receive: mpsc::Receiver<Command>,
    stop: Arc<AtomicBool>,
    kind: VolumeBackend,
    mut engine: Engine,
) {
    let sample = engine.mixer.read();
    state.lock().unwrap().sample(sample, None);
    let mut last_poll = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        if engine
            .lease
            .as_ref()
            .is_some_and(|lease| Instant::now() >= lease.until)
        {
            let result = engine
                .expire(Instant::now())
                .and_then(|()| engine.mixer.read());
            state
                .lock()
                .unwrap()
                .sample(result, engine.lease.as_ref().map(|l| l.until));
        }
        match receive.recv_timeout(Duration::from_millis(50)) {
            Ok(command) => {
                if command.reply.is_closed()
                    || Instant::now() >= command.expires
                    || stop.load(Ordering::Relaxed)
                {
                    continue;
                }
                let result = engine.apply(&command.request, Instant::now());
                let mut state = state.lock().unwrap();
                state.sample(result, engine.lease.as_ref().map(|lease| lease.until));
                let reply = if state.error.is_some() {
                    Err("provider_unavailable")
                } else {
                    Ok(
                        json!({"requested_percent":command.request.percent,"status":state.view(Instant::now(),kind==VolumeBackend::Simulate)}),
                    )
                };
                let _ = command.reply.send(reply);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if last_poll.elapsed() >= Duration::from_secs(1) {
            let result = engine.mixer.read();
            state
                .lock()
                .unwrap()
                .sample(result, engine.lease.as_ref().map(|lease| lease.until));
            last_poll = Instant::now();
        }
    }
    let result = engine.restore().and_then(|()| engine.mixer.read());
    state.lock().unwrap().sample(result, None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    struct SharedMixer {
        raw: Arc<Mutex<[i64; 2]>>,
        fail_read: Arc<AtomicBool>,
    }
    impl Mixer for SharedMixer {
        fn read(&mut self) -> io::Result<[i64; 2]> {
            if self.fail_read.swap(false, Ordering::Relaxed) {
                Err(io::Error::other("injected read failure"))
            } else {
                Ok(*self.raw.lock().unwrap())
            }
        }
        fn write(&mut self, raw: [i64; 2]) -> io::Result<()> {
            *self.raw.lock().unwrap() = raw;
            Ok(())
        }
    }
    fn engine() -> (Engine, Arc<Mutex<[i64; 2]>>, Arc<AtomicBool>) {
        let raw = Arc::new(Mutex::new([64, 32]));
        let fail = Arc::new(AtomicBool::new(false));
        (
            Engine {
                mixer: Box::new(SharedMixer {
                    raw: raw.clone(),
                    fail_read: fail.clone(),
                }),
                lease: None,
            },
            raw,
            fail,
        )
    }
    #[test]
    fn renewed_leases_restore_original_stereo_and_preserve_newer_firmware() {
        let (mut engine, raw, _) = engine();
        let now = Instant::now();
        engine
            .apply(
                &Request {
                    percent: 20,
                    lease_ms: Some(100),
                },
                now,
            )
            .unwrap();
        engine
            .apply(
                &Request {
                    percent: 10,
                    lease_ms: Some(100),
                },
                now + Duration::from_millis(50),
            )
            .unwrap();
        assert_eq!(*raw.lock().unwrap(), [26; 2]);
        engine.expire(now + Duration::from_millis(200)).unwrap();
        assert_eq!(*raw.lock().unwrap(), [64, 32]);
        engine
            .apply(
                &Request {
                    percent: 20,
                    lease_ms: Some(100),
                },
                now,
            )
            .unwrap();
        *raw.lock().unwrap() = [80, 40];
        engine.expire(now + Duration::from_millis(200)).unwrap();
        assert_eq!(*raw.lock().unwrap(), [80, 40]);
    }
    #[test]
    fn failed_restore_retains_lease_for_retry() {
        let (mut engine, raw, fail) = engine();
        let now = Instant::now();
        engine
            .apply(
                &Request {
                    percent: 20,
                    lease_ms: Some(100),
                },
                now,
            )
            .unwrap();
        fail.store(true, Ordering::Relaxed);
        assert!(engine.expire(now + Duration::from_millis(200)).is_err());
        assert!(
            engine.lease.is_some(),
            "failed restore discarded its saved baseline"
        );
        engine.expire(now + Duration::from_millis(300)).unwrap();
        assert_eq!(*raw.lock().unwrap(), [64, 32]);
    }
    #[test]
    fn persistent_writes_supersede_temporary_restore_and_validate_before_io() {
        let (mut engine, raw, _) = engine();
        let now = Instant::now();
        engine
            .apply(
                &Request {
                    percent: 10,
                    lease_ms: Some(100),
                },
                now,
            )
            .unwrap();
        engine
            .apply(
                &Request {
                    percent: 70,
                    lease_ms: None,
                },
                now,
            )
            .unwrap();
        engine.expire(now + Duration::from_millis(200)).unwrap();
        assert_eq!(*raw.lock().unwrap(), [179; 2]);
        assert!(
            engine
                .apply(
                    &Request {
                        percent: 101,
                        lease_ms: None
                    },
                    now
                )
                .is_err()
        );
        assert_eq!(*raw.lock().unwrap(), [179; 2]);
        engine
            .apply(
                &Request {
                    percent: 0,
                    lease_ms: None,
                },
                now,
            )
            .unwrap();
        assert_eq!(*raw.lock().unwrap(), [0; 2]);
        engine
            .apply(
                &Request {
                    percent: 100,
                    lease_ms: None,
                },
                now,
            )
            .unwrap();
        assert_eq!(*raw.lock().unwrap(), [255; 2]);
    }
    #[test]
    fn stale_or_failed_volume_samples_are_not_reported_as_current() {
        let mut state = State::new(false);
        let now = Instant::now();
        state.sample(Ok([64, 32]), None);
        state.sampled = Some(now - STALE);
        assert_eq!(state.view(now, true)["ready"], false);
        assert!(state.view(now, true)["raw"].is_null());
        state.sample(Err(io::Error::other("unavailable")), None);
        assert!(state.view(now, true)["raw"].is_null());
        state.sample(Ok([0, 255]), None);
        assert_eq!(state.view(Instant::now(), true)["ready"], true);
    }
    struct GateMixer {
        reads: usize,
        raw: [i64; 2],
        writes: Arc<Mutex<Vec<[i64; 2]>>>,
        blocked: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
    }
    impl Mixer for GateMixer {
        fn read(&mut self) -> io::Result<[i64; 2]> {
            self.reads += 1;
            if self.reads == 2 {
                self.blocked.send(()).unwrap();
                self.release.recv_timeout(Duration::from_secs(2)).unwrap();
            }
            Ok(self.raw)
        }
        fn write(&mut self, raw: [i64; 2]) -> io::Result<()> {
            self.writes.lock().unwrap().push(raw);
            self.raw = raw;
            Ok(())
        }
    }
    #[test]
    fn volume_queue_is_bounded_and_skips_canceled_or_expired_changes() {
        let state = Arc::new(Mutex::new(State::new(false)));
        let stop = Arc::new(AtomicBool::new(false));
        let (send, receive) = mpsc::sync_channel(8);
        let (blocked, started) = mpsc::sync_channel(1);
        let (release, wait) = mpsc::sync_channel(1);
        let writes = Arc::new(Mutex::new(vec![]));
        let engine = Engine {
            mixer: Box::new(GateMixer {
                reads: 0,
                raw: [64; 2],
                writes: writes.clone(),
                blocked,
                release: wait,
            }),
            lease: None,
        };
        let worker_state = state.clone();
        let worker_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            serve(
                worker_state,
                receive,
                worker_stop,
                VolumeBackend::Simulate,
                engine,
            )
        });
        let command = |percent, expires| {
            let (reply, result) = oneshot::channel();
            (
                Command {
                    request: Request {
                        percent,
                        lease_ms: None,
                    },
                    reply,
                    expires,
                },
                result,
            )
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let (first, first_reply) = command(10, deadline);
        send.try_send(first).unwrap();
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        let (canceled, canceled_reply) = command(20, deadline);
        drop(canceled_reply);
        send.try_send(canceled).unwrap();
        let (expired, expired_reply) = command(30, Instant::now() - Duration::from_secs(1));
        send.try_send(expired).unwrap();
        let (live, live_reply) = command(40, deadline);
        send.try_send(live).unwrap();
        for _ in 0..5 {
            let (canceled, reply) = command(50, deadline);
            drop(reply);
            send.try_send(canceled).unwrap();
        }
        let (overflow, _) = command(90, deadline);
        assert!(matches!(
            send.try_send(overflow),
            Err(mpsc::TrySendError::Full(_))
        ));
        release.send(()).unwrap();
        assert!(first_reply.blocking_recv().unwrap().is_ok());
        assert!(live_reply.blocking_recv().unwrap().is_ok());
        assert!(expired_reply.blocking_recv().is_err());
        stop.store(true, Ordering::Relaxed);
        thread.join().unwrap();
        assert_eq!(*writes.lock().unwrap(), vec![[26; 2], [102; 2]]);
    }
}
