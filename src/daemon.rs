//! Async control transport; one dedicated actor owns the blocking process engine.
use crate::{
    config::ServiceConfig,
    engine::Supervisor,
    protocol::{Request, Response},
};
use serde_json::json;
use std::{
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot},
    task::JoinSet,
};

const LIMIT: usize = 65536;
enum Command {
    Tick,
    Request {
        request: Request,
        reply: oneshot::Sender<Response>,
        expires: Instant,
    },
    Stop,
}
fn execute(manager: &mut Supervisor, request: Request) -> (Response, bool) {
    let result = match request {
        Request::Ping => Ok(manager.ping()),
        Request::List => Ok(manager.list()),
        Request::Get { name } => manager.get(&name),
        Request::Add { config } => manager.add(config),
        Request::Start { name } => manager.start(&name),
        Request::Stop { name } => manager.stop(&name),
        Request::Restart { name } => manager.restart(&name),
        Request::Enable { name, enabled } => manager.enable(&name, enabled),
        Request::Remove { name } => manager.remove(&name),
        Request::Reload { name } => manager.reload(&name),
        Request::Shutdown => return (Response::success(json!({"shutdown":true})), true),
    };
    (
        match result {
            Ok(data) => Response::success(data),
            Err(e) => Response::error(e),
        },
        false,
    )
}
fn actor(
    root: PathBuf,
    mut commands: mpsc::Receiver<Command>,
    ready: oneshot::Sender<Result<(), String>>,
) {
    let mut manager = match Supervisor::new(&root) {
        Ok(manager) => manager,
        Err(e) => {
            let _ = ready.send(Err(e.to_string()));
            return;
        }
    };
    let api_record = root.join("services/yactl-api.json");
    if !api_record.try_exists().unwrap_or(true) {
        let result = std::env::current_exe()
            .map_err(|e| e.to_string())
            .and_then(|binary| {
                let config = ServiceConfig::new(
                    "yactl-api".into(),
                    vec![
                        binary.to_string_lossy().into_owned(),
                        "--root".into(),
                        root.to_string_lossy().into_owned(),
                        "api-worker".into(),
                    ],
                );
                manager.add(config)
            });
        if let Err(error) = result {
            let _ = ready.send(Err(error));
            return;
        }
    }
    let _ = ready.send(Ok(()));
    let mut reap = Instant::now();
    while let Some(command) = commands.blocking_recv() {
        // Requests also advance supervision: a full queue cannot starve ticks.
        manager.tick();
        if reap.elapsed() >= Duration::from_millis(250) {
            manager.reap_orphans();
            reap = Instant::now();
        }
        match command {
            Command::Tick => {}
            Command::Stop => break,
            Command::Request {
                request,
                reply,
                expires,
            } => {
                if reply.is_closed() || Instant::now() >= expires {
                    continue;
                }
                let (response, stop) = execute(&mut manager, request);
                let _ = reply.send(response);
                if stop {
                    break;
                }
            }
        }
    }
    manager.shutdown();
}
async fn read_request(stream: &mut UnixStream) -> io::Result<Request> {
    tokio::time::timeout(Duration::from_millis(250), async {
        let mut data = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                return Err(io::Error::other("incomplete control request"));
            }
            data.extend_from_slice(&chunk[..n]);
            if data.len() > LIMIT {
                return Err(io::Error::other("control request exceeds 64 KiB"));
            }
            if let Some(end) = data.iter().position(|b| *b == b'\n') {
                return serde_json::from_slice(&data[..end]).map_err(io::Error::other);
            }
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "control request deadline exceeded"))?
}
async fn connection(mut stream: UnixStream, sender: mpsc::Sender<Command>) {
    let response = match read_request(&mut stream).await {
        Err(e) => Response::error(e),
        Ok(request) => {
            let (reply, receive) = oneshot::channel();
            let command = Command::Request {
                request,
                reply,
                expires: Instant::now() + Duration::from_secs(1),
            };
            if sender.try_send(command).is_err() {
                Response::error("manager queue is full")
            } else {
                match tokio::time::timeout(Duration::from_secs(1), receive).await {
                    Ok(Ok(response)) => response,
                    _ => Response::error("manager response deadline exceeded"),
                }
            }
        }
    };
    let Ok(mut bytes) = serde_json::to_vec(&response) else {
        return;
    };
    if bytes.len() >= LIMIT {
        bytes =
            serde_json::to_vec(&Response::error("response too large; query one service")).unwrap();
    }
    bytes.push(b'\n');
    let _ = tokio::time::timeout(Duration::from_millis(500), stream.write_all(&bytes)).await;
}
pub async fn serve(root: &Path, listener: std::os::unix::net::UnixListener) -> io::Result<()> {
    let listener = UnixListener::from_std(listener)?;
    let (sender, receiver) = mpsc::channel(16);
    let (ready, started) = oneshot::channel();
    let (done, finished) = oneshot::channel();
    let root = root.to_owned();
    let actor = std::thread::Builder::new()
        .name("yactl-supervisor".into())
        .spawn(move || {
            actor(root, receiver, ready);
            let _ = done.send(());
        })?;
    started
        .await
        .map_err(|_| io::Error::other("supervisor startup failed"))?
        .map_err(io::Error::other)?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut finished = Box::pin(finished);
    let mut tasks = JoinSet::new();
    let mut actor_done = false;
    loop {
        tokio::select! {
            _ = &mut finished => { actor_done = true; break; },
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
            _ = tick.tick() => { let _ = sender.try_send(Command::Tick); },
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                if tasks.len() < 8 { tasks.spawn(connection(stream, sender.clone())); }
            }
        }
    }
    drop(listener);
    if !actor_done {
        // Single owner processes stop after at most the bounded command backlog.
        let _ = tokio::time::timeout(Duration::from_secs(2), sender.send(Command::Stop)).await;
        tokio::time::timeout(Duration::from_secs(35), &mut finished)
            .await
            .map_err(|_| io::Error::other("supervisor shutdown deadline exceeded"))?
            .map_err(|_| io::Error::other("supervisor actor failed"))?;
    }
    // Let a shutdown caller receive its reply before aborting stalled connections.
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    tasks.abort_all();
    actor
        .join()
        .map_err(|_| io::Error::other("supervisor actor panicked"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn full_queue_rejects_without_waiting_and_recovers() {
        let (sender, mut receiver) = mpsc::channel(1);
        sender.try_send(Command::Tick).unwrap();
        let (server, mut client) = UnixStream::pair().unwrap();
        let task = tokio::spawn(connection(server, sender.clone()));
        client.write_all(b"{\"action\":\"ping\"}\n").await.unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_millis(200), client.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        let response: Response = serde_json::from_slice(&bytes).unwrap();
        assert!(!response.ok);
        assert_eq!(response.error.as_deref(), Some("manager queue is full"));
        task.await.unwrap();
        assert!(matches!(receiver.recv().await, Some(Command::Tick)));
        let (server, mut client) = UnixStream::pair().unwrap();
        let task = tokio::spawn(connection(server, sender));
        client.write_all(b"{\"action\":\"ping\"}\n").await.unwrap();
        let Some(Command::Request { reply, .. }) = receiver.recv().await else {
            panic!("request not queued after capacity recovered");
        };
        reply.send(Response::success(json!({"pid":123}))).unwrap();
        bytes.clear();
        client.read_to_end(&mut bytes).await.unwrap();
        let response: Response = serde_json::from_slice(&bytes).unwrap();
        assert!(response.ok);
        assert_eq!(response.data["pid"], 123);
        task.await.unwrap();
    }
    #[test]
    fn actor_skips_canceled_and_expired_mutations() {
        let root = std::env::temp_dir().join(format!("yactl-actor-cancel-{}", std::process::id()));
        let store = crate::config::ConfigStore::new(&root).unwrap();
        let disabled = |name: &str| {
            let mut config = ServiceConfig::new(name.into(), vec!["/bin/true".into()]);
            config.enabled = false;
            config
        };
        store.save(&disabled("yactl-api")).unwrap();
        let (sender, receiver) = mpsc::channel(16);
        let (canceled, canceled_reply) = oneshot::channel();
        drop(canceled_reply);
        sender
            .try_send(Command::Request {
                request: Request::Add {
                    config: disabled("canceled"),
                },
                reply: canceled,
                expires: Instant::now() + Duration::from_secs(10),
            })
            .unwrap();
        let (expired, expired_reply) = oneshot::channel();
        sender
            .try_send(Command::Request {
                request: Request::Add {
                    config: disabled("expired"),
                },
                reply: expired,
                expires: Instant::now() - Duration::from_secs(1),
            })
            .unwrap();
        let (live, live_reply) = oneshot::channel();
        sender
            .try_send(Command::Request {
                request: Request::Add {
                    config: disabled("live"),
                },
                reply: live,
                expires: Instant::now() + Duration::from_secs(10),
            })
            .unwrap();
        sender.try_send(Command::Stop).unwrap();
        let (ready, started) = oneshot::channel();
        let actor_root = root.clone();
        let thread = std::thread::spawn(move || actor(actor_root, receiver, ready));
        started.blocking_recv().unwrap().unwrap();
        assert!(live_reply.blocking_recv().unwrap().ok);
        assert!(expired_reply.blocking_recv().is_err());
        thread.join().unwrap();
        assert!(!root.join("services/canceled.json").exists());
        assert!(!root.join("services/expired.json").exists());
        assert!(root.join("services/live.json").is_file());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn trickled_request_has_one_total_deadline() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        let task = tokio::spawn(async move {
            for _ in 0..4 {
                if writer.write_all(b" ").await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        });
        let start = Instant::now();
        assert!(read_request(&mut reader).await.is_err());
        assert!(start.elapsed() < Duration::from_millis(450));
        task.abort();
    }
}
