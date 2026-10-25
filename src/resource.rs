//! A bounded mailbox and expiring ownership for one blocking hardware adapter.
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    io,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};
use tokio::sync::oneshot;

pub const LEASE: Duration = Duration::from_millis(1000);
const STALE: Duration = Duration::from_millis(250);
pub trait ResourceFrame: Clone + Serialize + Send + 'static {
    fn sequence(&self) -> u64;
    fn validate(&self, _details: &Value) -> Result<(), &'static str> {
        Ok(())
    }
}
pub trait Adapter<F: ResourceFrame>: Send {
    fn details(&self) -> Value {
        json!({})
    }
    fn capture(&mut self) -> io::Result<()>;
    fn write(&mut self, frame: &F) -> io::Result<()>;
    fn restore(&mut self) -> io::Result<()>;
    fn current(&self) -> F;
}
#[derive(Clone)]
pub struct Config {
    pub enabled: bool,
    pub simulated: bool,
    pub id: &'static str,
    pub thread: &'static str,
    pub details: Value,
}
struct Owner {
    id: u64,
    deadline: Instant,
    sequence: u64,
    release: bool,
}
struct State<F> {
    details: Value,
    ready: bool,
    verified: bool,
    stopped: bool,
    stopping: bool,
    error: Option<String>,
    owner: Option<Owner>,
    pending: Option<(u64, Instant, F)>,
    frame: F,
    next_id: u64,
}
impl<F> State<F> {
    fn new(frame: F) -> Self {
        Self {
            details: json!({}),
            ready: false,
            verified: false,
            stopped: false,
            stopping: false,
            error: None,
            owner: None,
            pending: None,
            frame,
            next_id: 1,
        }
    }
    fn next_frame(&mut self, now: Instant) -> Option<F> {
        self.pending
            .take()
            .filter(|(id, at, _)| {
                now.saturating_duration_since(*at) < STALE
                    && self
                        .owner
                        .as_ref()
                        .is_some_and(|o| o.id == *id && !o.release && o.deadline > now)
            })
            .map(|(_, _, frame)| frame)
    }
}
enum Control {
    Acquire(oneshot::Sender<Result<u64, &'static str>>),
}
#[derive(Clone)]
pub struct Resource<F: ResourceFrame> {
    state: Arc<Mutex<State<F>>>,
    controls: mpsc::SyncSender<Control>,
    config: Config,
}
pub struct Session<F: ResourceFrame> {
    resource: Resource<F>,
    id: u64,
}
impl<F: ResourceFrame> Resource<F> {
    pub fn start(
        config: Config,
        blank: F,
        open: impl FnOnce() -> io::Result<Box<dyn Adapter<F>>> + Send + 'static,
    ) -> Self {
        let state = Arc::new(Mutex::new(State::new(blank)));
        let (controls, receive) = mpsc::sync_channel(8);
        let handle = Self {
            state: state.clone(),
            controls,
            config,
        };
        if handle.config.enabled {
            match thread::Builder::new()
                .name(handle.config.thread.into())
                .spawn(move || {
                    worker(state.clone(), receive, open);
                    state.lock().unwrap().stopped = true;
                }) {
                Ok(_) => {}
                Err(e) => {
                    let mut state = handle.state.lock().unwrap();
                    state.error = Some(e.to_string());
                    state.stopped = true;
                }
            }
        } else {
            handle.state.lock().unwrap().stopped = true;
        }
        handle
    }
    pub async fn acquire(&self) -> Result<Session<F>, &'static str> {
        if !self.config.enabled {
            return Err("unsupported_hardware");
        }
        let (reply, receive) = oneshot::channel();
        self.controls
            .try_send(Control::Acquire(reply))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => "busy",
                mpsc::TrySendError::Disconnected(_) => "provider_unavailable",
            })?;
        let id = tokio::time::timeout(LEASE, receive)
            .await
            .map_err(|_| "provider_unavailable")?
            .map_err(|_| "provider_unavailable")??;
        Ok(Session {
            resource: self.clone(),
            id,
        })
    }
    pub fn status(&self) -> Value {
        let state = self.state.lock().unwrap();
        let mut value = serde_json::to_value(&state.frame).unwrap();
        let fields = json!({"supported":state.verified,"ready":state.ready,"simulated":self.config.simulated,"owned":state.owner.is_some(),"owner_id":state.owner.as_ref().map(|o|o.id),"error":state.error});
        value
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        value
    }
    pub fn capability(&self) -> Option<Value> {
        let state = self.state.lock().unwrap();
        if !self.config.enabled || (!state.verified && !self.config.simulated) {
            return None;
        }
        let mut details = self.config.details.clone();
        details
            .as_object_mut()
            .unwrap()
            .extend(state.details.as_object().unwrap().clone());
        details.as_object_mut().unwrap().extend(json!({"id":self.config.id,"supported":true,"ready":state.ready,"simulated":self.config.simulated,"lease_ms":1000,"pending_frames":1}).as_object().unwrap().clone());
        Some(details)
    }
    pub fn health(&self) -> Option<Value> {
        if !self.config.enabled {
            return None;
        }
        let state = self.state.lock().unwrap();
        Some(
            json!({"id":self.config.id,"supported":state.verified,"ready":state.ready,"simulated":self.config.simulated,"error":state.error}),
        )
    }
    pub fn shutdown(&self) {
        let mut state = self.state.lock().unwrap();
        state.stopping = true;
        state.pending = None;
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
impl<F: ResourceFrame> Session<F> {
    pub fn submit(&self, frame: F) -> Result<(), &'static str> {
        let now = Instant::now();
        let mut state = self.resource.state.lock().unwrap();
        frame.validate(&state.details)?;
        if state.stopping {
            return Err("provider_unavailable");
        }
        let Some(owner) = state
            .owner
            .as_mut()
            .filter(|o| o.id == self.id && !o.release && o.deadline > now)
        else {
            return Err("lease_expired");
        };
        if frame.sequence() <= owner.sequence {
            return Err("stale_frame");
        }
        owner.sequence = frame.sequence();
        owner.deadline = now + LEASE;
        state.pending = Some((self.id, now, frame));
        Ok(())
    }
}
impl<F: ResourceFrame> Drop for Session<F> {
    fn drop(&mut self) {
        let mut state = self.resource.state.lock().unwrap();
        if let Some(owner) = state.owner.as_mut().filter(|o| o.id == self.id) {
            owner.release = true;
            state.pending = None;
        }
    }
}
fn worker<F: ResourceFrame>(
    state: Arc<Mutex<State<F>>>,
    receive: mpsc::Receiver<Control>,
    open: impl FnOnce() -> io::Result<Box<dyn Adapter<F>>>,
) {
    let mut backend = match open() {
        Ok(backend) => backend,
        Err(e) => {
            state.lock().unwrap().error = Some(e.to_string());
            return;
        }
    };
    {
        let details = backend.details();
        let current = backend.current();
        let mut state = state.lock().unwrap();
        state.ready = true;
        state.verified = true;
        state.details = details;
        state.frame = current;
    }
    loop {
        let (release, stop, idle) = {
            let state = state.lock().unwrap();
            (
                state
                    .owner
                    .as_ref()
                    .is_some_and(|o| o.release || o.deadline <= Instant::now())
                    || state.stopping,
                state.stopping,
                state.owner.is_none(),
            )
        };
        if release {
            state.lock().unwrap().pending = None;
            let result = backend.restore();
            let current = backend.current();
            let mut state = state.lock().unwrap();
            state.owner = None;
            state.frame = current;
            if let Err(e) = result {
                state.ready = false;
                state.error = Some(e.to_string());
            }
        }
        if stop {
            break;
        }
        match receive.recv_timeout(if idle {
            Duration::from_millis(50)
        } else {
            Duration::from_millis(5)
        }) {
            Ok(Control::Acquire(reply)) => {
                if reply.is_closed() {
                    continue;
                }
                let result = {
                    let state = state.lock().unwrap();
                    if !state.ready || state.stopping {
                        Err("provider_unavailable")
                    } else if state.owner.is_some() {
                        Err("ownership_conflict")
                    } else {
                        Ok(())
                    }
                };
                if let Err(code) = result {
                    let _ = reply.send(Err(code));
                    continue;
                }
                if let Err(e) = backend.capture() {
                    let mut state = state.lock().unwrap();
                    state.ready = false;
                    state.error = Some(e.to_string());
                    let _ = reply.send(Err("provider_unavailable"));
                    continue;
                }
                let captured = backend.current();
                let id = {
                    let mut state = state.lock().unwrap();
                    state.frame = captured;
                    let id = state.next_id;
                    state.next_id += 1;
                    state.owner = Some(Owner {
                        id,
                        deadline: Instant::now() + LEASE,
                        sequence: 0,
                        release: false,
                    });
                    id
                };
                if reply.send(Ok(id)).is_err() {
                    state.lock().unwrap().owner.as_mut().unwrap().release = true;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = backend.restore();
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let pending = state.lock().unwrap().next_frame(Instant::now());
        if let Some(frame) = pending {
            let result = backend.write(&frame);
            let mut state = state.lock().unwrap();
            match result {
                Ok(()) => state.frame = frame,
                Err(e) => {
                    state.error = Some(e.to_string());
                    state.ready = false;
                    if let Some(owner) = state.owner.as_mut() {
                        owner.release = true;
                    }
                }
            }
        }
    }
    state.lock().unwrap().ready = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Clone, Serialize)]
    struct Frame {
        sequence: u64,
        level: u8,
    }
    impl Frame {
        fn blank() -> Self {
            Self {
                sequence: 0,
                level: 0,
            }
        }
    }
    impl ResourceFrame for Frame {
        fn sequence(&self) -> u64 {
            self.sequence
        }
    }
    struct TestAdapter(Frame);
    impl Adapter<Frame> for TestAdapter {
        fn capture(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn write(&mut self, frame: &Frame) -> io::Result<()> {
            self.0 = frame.clone();
            Ok(())
        }
        fn restore(&mut self) -> io::Result<()> {
            self.0 = Frame::blank();
            Ok(())
        }
        fn current(&self) -> Frame {
            self.0.clone()
        }
    }
    fn config() -> Config {
        Config {
            enabled: true,
            simulated: true,
            id: "test",
            thread: "yactl-test",
            details: json!({}),
        }
    }
    #[tokio::test]
    async fn acquisition_publishes_the_fresh_capture_before_accepting_any_frame() {
        use std::sync::atomic::{AtomicU8, Ordering};
        struct ExternalAdapter {
            external: Arc<AtomicU8>,
            frame: Frame,
        }
        impl Adapter<Frame> for ExternalAdapter {
            fn capture(&mut self) -> io::Result<()> {
                self.frame.level = self.external.load(Ordering::Relaxed);
                Ok(())
            }
            fn write(&mut self, frame: &Frame) -> io::Result<()> {
                self.frame = frame.clone();
                Ok(())
            }
            fn restore(&mut self) -> io::Result<()> {
                Ok(())
            }
            fn current(&self) -> Frame {
                self.frame.clone()
            }
        }
        let external = Arc::new(AtomicU8::new(7));
        let adapter_external = external.clone();
        let resource = Resource::start(config(), Frame::blank(), move || {
            Ok(Box::new(ExternalAdapter {
                external: adapter_external,
                frame: Frame {
                    sequence: 0,
                    level: 7,
                },
            }) as Box<dyn Adapter<Frame>>)
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while resource.status()["ready"] != true {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(resource.status()["level"], 7);
        external.store(51, Ordering::Relaxed);
        let session = resource.acquire().await.unwrap();
        let captured = resource.status();
        assert_eq!(captured["owned"], true);
        assert_eq!(
            captured["level"], 51,
            "status kept the old snapshot after acquisition"
        );
        assert_eq!(captured["sequence"], 0);
        drop(session);
        resource.shutdown();
        resource.wait_stopped().await;
    }
    #[test]
    fn latest_frame_replaces_pending_work_and_release_takes_priority() {
        let now = Instant::now();
        let mut state = State::new(Frame::blank());
        state.owner = Some(Owner {
            id: 1,
            deadline: now + LEASE,
            sequence: 0,
            release: false,
        });
        let (controls, _receive) = mpsc::sync_channel(8);
        let lighting = Resource {
            state: Arc::new(Mutex::new(state)),
            controls,
            config: config(),
        };
        let session = Session {
            resource: lighting.clone(),
            id: 1,
        };
        for sequence in 1..=100 {
            let mut frame = Frame::blank();
            frame.sequence = sequence;
            session.submit(frame).unwrap();
        }
        assert_eq!(
            lighting
                .state
                .lock()
                .unwrap()
                .next_frame(Instant::now())
                .unwrap()
                .sequence,
            100
        );
        let mut frame = Frame::blank();
        frame.sequence = 101;
        session.submit(frame).unwrap();
        drop(session);
        assert!(
            lighting
                .state
                .lock()
                .unwrap()
                .next_frame(Instant::now())
                .is_none()
        );
    }
    #[test]
    fn expired_frames_and_leases_never_reach_the_adapter() {
        let now = Instant::now();
        let mut state = State::new(Frame::blank());
        state.owner = Some(Owner {
            id: 1,
            deadline: now + LEASE,
            sequence: 1,
            release: false,
        });
        state.pending = Some((1, now - STALE, Frame::blank()));
        assert!(state.next_frame(now).is_none());
        state.owner.as_mut().unwrap().deadline = now;
        state.pending = Some((1, now, Frame::blank()));
        assert!(state.next_frame(now).is_none());
    }
    #[tokio::test]
    async fn old_session_cannot_release_a_newer_owner() {
        let lighting = Resource::start(config(), Frame::blank(), || {
            Ok(Box::new(TestAdapter(Frame::blank())))
        });
        let first = lighting.acquire().await.unwrap();
        let first_id = first.id;
        {
            let mut state = lighting.state.lock().unwrap();
            state.owner.as_mut().unwrap().deadline = Instant::now() - Duration::from_millis(1);
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while lighting.status()["owned"] == true {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let second = lighting.acquire().await.unwrap();
        assert_ne!(first_id, second.id);
        drop(first);
        assert_eq!(lighting.status()["owned"], true);
        let mut frame = Frame::blank();
        frame.sequence = 1;
        second.submit(frame).unwrap();
        drop(second);
        lighting.shutdown();
        lighting.wait_stopped().await;
        assert_eq!(lighting.status()["owned"], false);
    }
}
