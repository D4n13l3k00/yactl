//! Versioned station API. Hardware adapters are added separately, never in the supervisor.
use crate::{
    protocol::Request as ServiceRequest,
    settings::{Access, Scope, Settings},
};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path as ApiPath, RawQuery, State},
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    net::SocketAddr,
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UnixListener, UnixStream},
    sync::{RwLock, Semaphore, mpsc, oneshot},
    task::JoinSet,
};

struct Runtime {
    config: Settings,
    keys: Vec<(Vec<u8>, Vec<Scope>)>,
}
impl Runtime {
    fn load(root: &Path) -> io::Result<Self> {
        let config = Settings::load(root)?;
        let keys = config.credentials()?;
        Ok(Self { config, keys })
    }
}
#[derive(Clone, Copy)]
enum Peer {
    Unix,
    Tcp(SocketAddr),
}
struct Reload(oneshot::Sender<Result<(), String>>);
struct App {
    root: PathBuf,
    runtime: RwLock<Runtime>,
    instance_id: String,
    evidence: crate::identity::Evidence,
    reload: mpsc::Sender<Reload>,
    http: RwLock<Option<SocketAddr>>,
    service_slots: Arc<Semaphore>,
    lighting: crate::lighting::Lighting,
    clock: crate::clock::Clock,
    sensors: crate::sensors::Sensors,
    volume: crate::volume::Volume,
    events: crate::events::Feed,
    firmware: crate::firmware::Firmware,
    input: crate::input::Input,
}
fn error(status: StatusCode, code: &str, message: impl ToString) -> Response {
    (
        status,
        Json(json!({"error":{"code":code,"message":message.to_string()}})),
    )
        .into_response()
}
fn normalize_error(response: Response) -> Response {
    let status = response.status();
    if (status.is_client_error() || status.is_server_error())
        && !response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/json"))
    {
        let code = match status {
            StatusCode::METHOD_NOT_ALLOWED => "method_not_allowed",
            StatusCode::PAYLOAD_TOO_LARGE => "request_too_large",
            _ => "invalid_request",
        };
        let mut normalized = error(
            status,
            code,
            status.canonical_reason().unwrap_or("request rejected"),
        );
        for (name, value) in response.headers() {
            if !matches!(
                name.as_str(),
                "content-type"
                    | "content-length"
                    | "transfer-encoding"
                    | "content-encoding"
                    | "etag"
                    | "last-modified"
            ) {
                normalized.headers_mut().append(name.clone(), value.clone());
            }
        }
        return normalized;
    }
    response
}
fn token_equal(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}
async fn authorize(State(app): State<Arc<App>>, request: Request<Body>, next: Next) -> Response {
    let scope = if request.uri().path().starts_with("/v1/services")
        || request.uri().path() == "/v1/config/reload"
    {
        Scope::Services
    } else if (request.uri().path().starts_with("/v1/lighting/")
        || request.uri().path().starts_with("/v1/clock/")
        || request.uri().path() == "/v1/volume")
        && request.method() != axum::http::Method::GET
    {
        Scope::Hardware
    } else {
        Scope::Read
    };
    // No browser-facing API is implemented yet. In particular, reject browser mutations.
    if request.method() != axum::http::Method::GET && request.headers().contains_key("origin") {
        return error(
            StatusCode::FORBIDDEN,
            "origin_rejected",
            "browser mutations are not enabled",
        );
    }
    let runtime = app.runtime.read().await;
    let peer = request.extensions().get::<Peer>().copied();
    let allowed = match peer {
        Some(Peer::Unix) => true, // api.sock is private (0600), separate from LAN access.
        Some(Peer::Tcp(peer)) => {
            if !runtime.config.peer_allowed(peer.ip()) {
                return error(
                    StatusCode::FORBIDDEN,
                    "peer_rejected",
                    "client is outside configured ranges",
                );
            }
            let authorization = request.headers().get_all("authorization");
            let mut headers = authorization.iter();
            let token = headers.next();
            if headers.next().is_some() {
                return error(
                    StatusCode::UNAUTHORIZED,
                    "authentication_required",
                    "duplicate credentials",
                );
            }
            if let Some(token) = token {
                let token = token
                    .to_str()
                    .ok()
                    .and_then(|s| s.strip_prefix("Bearer "))
                    .unwrap_or("");
                if token.len() > 256 {
                    return error(
                        StatusCode::UNAUTHORIZED,
                        "authentication_required",
                        "invalid credentials",
                    );
                }
                let granted = runtime
                    .keys
                    .iter()
                    .find(|(secret, _)| token_equal(secret, token.as_bytes()));
                match granted {
                    Some((_, scopes)) => scopes.contains(&scope),
                    None => {
                        return error(
                            StatusCode::UNAUTHORIZED,
                            "authentication_required",
                            "invalid credentials",
                        );
                    }
                }
            } else {
                runtime.config.http.access == Access::Loopback
                    && peer.ip().is_loopback()
                    && scope == Scope::Read
            }
        }
        _ => false,
    };
    drop(runtime);
    if !allowed {
        return error(
            StatusCode::FORBIDDEN,
            "scope_required",
            "credential does not grant this operation",
        );
    }
    if matches!(
        request.uri().path(),
        "/v1/lighting/stream" | "/v1/clock/stream"
    ) && request.method() == axum::http::Method::POST
    {
        if let Some(streaming) = request.extensions().get::<Arc<AtomicBool>>() {
            streaming.store(true, Ordering::Relaxed);
        }
        return normalize_error(next.run(request).await);
    }
    match tokio::time::timeout(Duration::from_secs(3), next.run(request)).await {
        Ok(response) => normalize_error(response),
        Err(_) => error(
            StatusCode::GATEWAY_TIMEOUT,
            "request_timeout",
            "request deadline exceeded",
        ),
    }
}
async fn health(State(app): State<Arc<App>>) -> Json<Value> {
    let supervisor = tokio::time::timeout(Duration::from_millis(250), async {
        let mut socket = UnixStream::connect(app.root.join("control.sock")).await?;
        socket.write_all(b"{\"action\":\"ping\"}\n").await?;
        let mut bytes = Vec::new();
        let mut chunk = [0; 1024];
        loop {
            let n = socket.read(&mut chunk).await?;
            if n == 0 || bytes.len() + n > 65536 {
                return Err(io::Error::other("invalid supervisor health response"));
            }
            bytes.extend_from_slice(&chunk[..n]);
            if let Some(end) = bytes.iter().position(|b| *b == b'\n') {
                let reply: crate::protocol::Response =
                    serde_json::from_slice(&bytes[..end]).map_err(io::Error::other)?;
                return Ok(reply.ok && reply.data["pid"].as_u64().is_some_and(|pid| pid > 1));
            }
        }
    })
    .await
    .is_ok_and(|result: io::Result<bool>| result.unwrap_or(false));
    Json(
        json!({"ready":true,"api_version":1,"http":*app.http.read().await,"supervisor_socket_present":app.root.join("control.sock").exists(),"supervisor":{"ready":supervisor},"providers":app.lighting.health().into_iter().chain(app.clock.health()).chain(app.sensors.health()).chain(app.volume.health()).chain(app.firmware.health()).chain(app.input.health()).collect::<Vec<_>>()}),
    )
}
async fn device(State(app): State<Arc<App>>) -> Json<Value> {
    Json(
        app.evidence
            .describe(&app.instance_id, &app.runtime.read().await.config.device),
    )
}
async fn capabilities(State(app): State<Arc<App>>) -> Json<Value> {
    Json(json!(
        app.lighting
            .capability()
            .into_iter()
            .chain(app.clock.capability())
            .chain(app.sensors.capability())
            .chain(app.volume.capability())
            .chain(app.firmware.capabilities())
            .chain(app.input.capability())
            .collect::<Vec<_>>()
    ))
}
async fn lighting_status(State(app): State<Arc<App>>) -> Json<Value> {
    Json(app.lighting.status())
}
fn provider_error(code: &str) -> Response {
    let status = match code {
        "ownership_conflict" | "stale_frame" => StatusCode::CONFLICT,
        "busy" => StatusCode::SERVICE_UNAVAILABLE,
        "provider_unavailable" => StatusCode::SERVICE_UNAVAILABLE,
        "unsupported_hardware" => StatusCode::NOT_IMPLEMENTED,
        "lease_expired" => StatusCode::REQUEST_TIMEOUT,
        _ => StatusCode::BAD_REQUEST,
    };
    error(status, code, code.replace('_', " "))
}
async fn lighting_stream(State(app): State<Arc<App>>, request: Request<Body>) -> Response {
    stream_frames(&app.lighting, request, |frame: crate::lighting::Frame| {
        Ok(frame)
    })
    .await
}
async fn volume_get(State(app): State<Arc<App>>) -> Json<Value> {
    Json(app.volume.status())
}
async fn volume_set(
    State(app): State<Arc<App>>,
    payload: Result<Json<crate::volume::Request>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(rejection) => return normalize_error(rejection.into_response()),
    };
    match app.volume.set(request).await {
        Ok(result) => Json(result).into_response(),
        Err(code) => provider_error(code),
    }
}
async fn sensors_list(State(app): State<Arc<App>>) -> Json<Value> {
    Json(json!(app.sensors.list()))
}
async fn sensor_get(State(app): State<Arc<App>>, ApiPath(id): ApiPath<String>) -> Response {
    match app.sensors.get(&id) {
        Some(reading) => Json(reading).into_response(),
        None => error(
            StatusCode::NOT_FOUND,
            "unsupported_sensor",
            "sensor is not in the discovered registry",
        ),
    }
}
async fn clock_status(State(app): State<Arc<App>>) -> Json<Value> {
    Json(app.clock.status())
}
async fn events_get(State(app): State<Arc<App>>, RawQuery(query): RawQuery) -> Response {
    let mut response = match app.events.read(query.as_deref()).await {
        Ok(page) => Json(page).into_response(),
        Err(crate::events::ReadError::InvalidQuery) => error(StatusCode::BAD_REQUEST, "invalid_request", "invalid event query"),
        Err(crate::events::ReadError::FutureCursor) => error(StatusCode::CONFLICT, "invalid_event_cursor", "cursor exceeds current event history"),
        Err(crate::events::ReadError::StreamReset) => (
            StatusCode::CONFLICT,
            Json(json!({"error":{"code":"event_stream_reset","message":"event history changed; start a new read"},"stream_id":app.events.stream_id()})),
        ).into_response(),
    };
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}
async fn input_get(State(app): State<Arc<App>>) -> Json<Value> {
    Json(app.input.status())
}
async fn clock_stream(State(app): State<Arc<App>>, request: Request<Body>) -> Response {
    stream_frames(&app.clock, request, crate::clock::Command::into_frame).await
}
async fn stream_frames<F: crate::resource::ResourceFrame, I: serde::de::DeserializeOwned>(
    resource: &crate::resource::Resource<F>,
    request: Request<Body>,
    convert: impl Fn(I) -> Result<F, &'static str>,
) -> Response {
    use http_body_util::BodyExt;
    if request
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        != Some("application/x-ndjson")
    {
        return provider_error("invalid_content_type");
    }
    let session = match resource.acquire().await {
        Ok(session) => session,
        Err(code) => return provider_error(code),
    };
    let mut body = request.into_body();
    let mut line = Vec::new();
    let mut frame_deadline = tokio::time::Instant::now() + Duration::from_millis(1000);
    loop {
        let chunk = match tokio::time::timeout_at(frame_deadline, body.frame()).await {
            Ok(Some(Ok(chunk))) => chunk,
            Ok(None) => break,
            Ok(Some(Err(_))) => return provider_error("invalid_frame"),
            Err(_) => return provider_error("lease_expired"),
        };
        let Ok(bytes) = chunk.into_data() else {
            continue;
        };
        for byte in bytes {
            if line.len() >= 4096 {
                return provider_error("frame_too_large");
            }
            if byte == b'\n' {
                let frame = match serde_json::from_slice::<I>(&line) {
                    Ok(frame) => frame,
                    Err(_) => return provider_error("invalid_frame"),
                };
                let frame = match convert(frame) {
                    Ok(frame) => frame,
                    Err(code) => return provider_error(code),
                };
                if let Err(code) = session.submit(frame) {
                    return provider_error(code);
                }
                line.clear();
                frame_deadline = tokio::time::Instant::now() + Duration::from_millis(1000);
            } else {
                line.push(byte);
            }
        }
    }
    if !line.is_empty() {
        return provider_error("invalid_frame");
    }
    Json(json!({"released":true})).into_response()
}
async fn services(State(app): State<Arc<App>>, Json(request): Json<ServiceRequest>) -> Response {
    let Ok(permit) = app.service_slots.clone().try_acquire_owned() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "service request limit reached",
        );
    };
    let root = app.root.clone();
    match tokio::task::spawn_blocking(move || {
        // Retain admission until blocking I/O ends, even if the HTTP task is canceled.
        let _permit = permit;
        crate::client::request(&root, request).map_err(|e| e.to_string())
    })
    .await
    {
        Ok(Ok(data)) => Json(data).into_response(),
        Ok(Err(e)) => error(StatusCode::BAD_REQUEST, "service_request_failed", e),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "worker_failed",
            "service request failed",
        ),
    }
}
async fn reload(State(app): State<Arc<App>>) -> Response {
    let (reply, result) = oneshot::channel();
    if app.reload.try_send(Reload(reply)).is_err() {
        return error(StatusCode::SERVICE_UNAVAILABLE, "busy", "reload queue full");
    }
    match result.await {
        Ok(Ok(())) => Json(json!({"reloaded":true})).into_response(),
        Ok(Err(e)) => error(StatusCode::BAD_REQUEST, "invalid_config", e),
        Err(_) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "worker_stopped",
            "API worker stopped",
        ),
    }
}
fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/device", get(device))
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/services/request", post(services))
        .route("/v1/config/reload", post(reload))
        .route("/v1/lighting/status", get(lighting_status))
        .route("/v1/lighting/stream", post(lighting_stream))
        .route("/v1/clock/status", get(clock_status))
        .route("/v1/clock/stream", post(clock_stream))
        .route("/v1/volume", get(volume_get).post(volume_set))
        .route("/v1/sensors", get(sensors_list))
        .route("/v1/sensors/{id}", get(sensor_get))
        .route("/v1/events", get(events_get))
        .route("/v1/input", get(input_get))
        .fallback(|| async { error(StatusCode::NOT_FOUND, "not_found", "unknown API route") })
        .layer(DefaultBodyLimit::max(65536))
        .layer(middleware::from_fn_with_state(app.clone(), authorize))
        .with_state(app)
}
struct Guard {
    path: PathBuf,
    inode: u64,
    _lock: File,
}
impl Drop for Guard {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|m| m.file_type().is_socket() && m.ino() == self.inode)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}
struct Prepared {
    listener: std::os::unix::net::UnixListener,
    guard: Guard,
    instance_id: String,
    evidence: crate::identity::Evidence,
    runtime: Runtime,
    events: crate::events::Feed,
    firmware_sources: crate::firmware::Prepared,
}
fn prepare(root: &Path) -> io::Result<Prepared> {
    let runtime = Runtime::load(root)?;
    crate::config::private_dir(root)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(root.join("api.lock"))?;
    if !lock.metadata()?.is_file()
        || unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0
    {
        return Err(io::Error::other(
            "API worker already running or invalid lock",
        ));
    }
    let path = root.join("api.sock");
    match fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_socket() => {
            match std::os::unix::net::UnixStream::connect(&path) {
                Ok(_) => return Err(io::Error::other("API socket is active")),
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => fs::remove_file(&path)?,
                Err(e) => return Err(e),
            }
        }
        Ok(_) => return Err(io::Error::other("api.sock exists and is not a socket")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = std::os::unix::net::UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let guard = Guard {
        inode: fs::symlink_metadata(&path)?.ino(),
        path,
        _lock: lock,
    };
    let id_path = root.join("instance-id");
    let id = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&id_path)
    {
        Ok(file) => {
            if !file.metadata()?.is_file() {
                return Err(io::Error::other("invalid identity file"));
            }
            let mut text = String::new();
            file.take(34).read_to_string(&mut text)?;
            if text.len() != 32 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(io::Error::other("invalid identity"));
            }
            text
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut random = [0u8; 16];
            File::open("/dev/urandom")?.read_exact(&mut random)?;
            let id: String = random.iter().map(|b| format!("{b:02x}")).collect();
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(id_path)?
                .write_all(id.as_bytes())?;
            id
        }
        Err(e) => return Err(e),
    };
    let evidence = crate::identity::Evidence::load(Path::new("/"));
    let events = crate::events::Feed::open()?;
    let firmware_sources = crate::firmware::Firmware::prepare(&runtime.config.firmware);
    Ok(Prepared {
        listener,
        guard,
        instance_id: id,
        evidence,
        runtime,
        events,
        firmware_sources,
    })
}
async fn connection<S>(
    stream: S,
    router: Router,
    peer: Peer,
    permit: tokio::sync::OwnedSemaphorePermit,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let _permit = permit;
    let streaming = Arc::new(AtomicBool::new(false));
    let service = TowerToHyperService::new(
        router
            .layer(axum::Extension(peer))
            .layer(axum::Extension(streaming.clone())),
    );
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(2))
        .keep_alive(false)
        .max_buf_size(16384);
    let mut connection = Box::pin(builder.serve_connection(TokioIo::new(stream), service));
    if tokio::time::timeout(Duration::from_secs(6), &mut connection)
        .await
        .is_err()
        && streaming.load(Ordering::Relaxed)
    {
        let _ = connection.await;
    }
}
pub async fn run(root: &Path) -> io::Result<()> {
    let path = root.to_owned();
    let Prepared {
        listener,
        guard: _guard,
        instance_id,
        evidence,
        runtime,
        events,
        firmware_sources,
    } = tokio::task::spawn_blocking(move || prepare(&path))
        .await
        .map_err(io::Error::other)??;
    let unix = UnixListener::from_std(listener)?;
    let mut tcp = if runtime.config.http.enabled {
        Some(TcpListener::bind(runtime.config.http.listen).await?)
    } else {
        None
    };
    let address = tcp.as_ref().map(TcpListener::local_addr).transpose()?;
    let (reload_tx, mut reload_rx) = mpsc::channel::<Reload>(4);
    let lighting = crate::lighting::start(runtime.config.lighting.backend);
    let clock = crate::clock::start(runtime.config.clock.backend);
    let sensors = crate::sensors::Sensors::start(runtime.config.sensors.backend);
    let volume = crate::volume::Volume::start(runtime.config.volume.backend);
    let firmware = crate::firmware::Firmware::start(firmware_sources, events.journal.clone());
    let input = crate::input::Input::start(
        &runtime.config.input,
        evidence.is_cucumber(),
        events.journal.clone(),
    );
    let app = Arc::new(App {
        root: root.to_owned(),
        runtime: RwLock::new(runtime),
        instance_id,
        evidence,
        reload: reload_tx,
        http: RwLock::new(address),
        service_slots: Arc::new(Semaphore::new(4)),
        lighting: lighting.clone(),
        clock: clock.clone(),
        sensors: sensors.clone(),
        volume: volume.clone(),
        events,
        firmware,
        input: input.clone(),
    });
    let router = router(app.clone());
    let slots = Arc::new(Semaphore::new(32));
    let reload_slot = Arc::new(Semaphore::new(1));
    let mut tasks = JoinSet::new();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    loop {
        tokio::select! {
            _=term.recv()=>break,
            _=interrupt.recv()=>break,
            Some(_)=tasks.join_next(),if !tasks.is_empty()=>{},
            accepted=unix.accept()=>{
                let (stream,_)=accepted?;
                if tasks.len()<32 && let Ok(permit)=slots.clone().try_acquire_owned(){tasks.spawn(connection(stream,router.clone(),Peer::Unix,permit));}
            },
            accepted=async{tcp.as_ref().unwrap().accept().await},if tcp.is_some()=>{
                let (stream,peer)=accepted?;
                if tasks.len()<32 && let Ok(permit)=slots.clone().try_acquire_owned(){tasks.spawn(connection(stream,router.clone(),Peer::Tcp(peer),permit));}
            },
            Some(Reload(reply))=reload_rx.recv()=>{
                let path=app.root.clone();
                let result=async {
                    let permit=reload_slot.clone().try_acquire_owned().map_err(|_|io::Error::other("previous config read is still pending"))?;
                    let loader=tokio::task::spawn_blocking(move||{let _permit=permit;Runtime::load(&path)});
                    let candidate=tokio::time::timeout(Duration::from_secs(2),loader).await.map_err(|_|io::Error::new(io::ErrorKind::TimedOut,"config read deadline exceeded"))?.map_err(io::Error::other)??;
                    if reply.is_closed(){return Err(io::Error::other("reload request canceled"));}
                    if candidate.config.lighting != app.runtime.read().await.config.lighting {return Err(io::Error::other("lighting backend changes require API-worker restart"));}
                    if candidate.config.clock != app.runtime.read().await.config.clock {return Err(io::Error::other("clock backend changes require API-worker restart"));}
                    if candidate.config.sensors != app.runtime.read().await.config.sensors {return Err(io::Error::other("sensor backend changes require API-worker restart"));}
                    if candidate.config.volume != app.runtime.read().await.config.volume {return Err(io::Error::other("volume backend changes require API-worker restart"));}
                    if candidate.config.firmware != app.runtime.read().await.config.firmware {return Err(io::Error::other("firmware source changes require API-worker restart"));}
                    if candidate.config.input != app.runtime.read().await.config.input {return Err(io::Error::other("input backend changes require API-worker restart"));}
                    let old=app.runtime.read().await.config.http.clone();
                    let changed=old.enabled!=candidate.config.http.enabled || old.listen!=candidate.config.http.listen;
                    let replacement=if changed && candidate.config.http.enabled {Some(TcpListener::bind(candidate.config.http.listen).await?)}else{None};
                    let replacement_address=replacement.as_ref().map(TcpListener::local_addr).transpose()?;
                    // Commit only after config, keys and the replacement listener are ready.
                    if changed {tcp=replacement;*app.http.write().await=replacement_address;}
                    *app.runtime.write().await=candidate;
                    Ok::<_,io::Error>(())
                }.await;
                let _=reply.send(result.map_err(|e|e.to_string()));
            }
        }
    }
    app.firmware.shutdown();
    input.shutdown();
    app.events.journal.close();
    lighting.shutdown();
    clock.shutdown();
    sensors.shutdown();
    volume.shutdown();
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    tokio::join!(
        lighting.wait_stopped(),
        clock.wait_stopped(),
        sensors.wait_stopped(),
        volume.wait_stopped(),
        input.wait_stopped()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    #[test]
    fn framework_error_normalization_preserves_http_headers() {
        let response = (
            StatusCode::METHOD_NOT_ALLOWED,
            [(axum::http::header::ALLOW, "POST")],
            "Method Not Allowed",
        )
            .into_response();
        let normalized = normalize_error(response);
        assert_eq!(normalized.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            normalized.headers().get(axum::http::header::ALLOW).unwrap(),
            "POST"
        );
        assert_eq!(
            normalized
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            "application/json"
        );
    }
    #[tokio::test]
    async fn canceled_forwarding_retains_capacity_until_blocking_io_finishes() {
        let root = std::env::temp_dir().join(format!("yactl-api-cancel-{}", std::process::id()));
        crate::config::private_dir(&root).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(root.join("control.sock")).unwrap();
        let (accepted, started) = oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(socket.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            accepted.send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(2)).unwrap();
            socket
                .write_all(b"{\"ok\":true,\"data\":{\"pid\":123}}\n")
                .unwrap();
        });
        let (reload, _) = mpsc::channel(1);
        let app = Arc::new(App {
            root: root.clone(),
            runtime: RwLock::new(Runtime {
                config: Settings::default(),
                keys: vec![],
            }),
            instance_id: "test".into(),
            evidence: crate::identity::Evidence::load(&root),
            reload,
            http: RwLock::new(None),
            service_slots: Arc::new(Semaphore::new(1)),
            lighting: crate::lighting::start(crate::settings::LightingBackend::Off),
            clock: crate::clock::start(crate::settings::ClockBackend::Off),
            sensors: crate::sensors::Sensors::start(crate::settings::SensorsBackend::Off),
            volume: crate::volume::Volume::start(crate::settings::VolumeBackend::Off),
            events: crate::events::Feed::open().unwrap(),
            firmware: crate::firmware::Firmware::start(
                crate::firmware::Firmware::prepare(&crate::settings::Firmware::default()),
                Arc::new(crate::events::Journal::default()),
            ),
            input: crate::input::Input::start(
                &crate::settings::Input::default(),
                false,
                Arc::new(crate::events::Journal::default()),
            ),
        });
        let pending = tokio::spawn(services(State(app.clone()), Json(ServiceRequest::Ping)));
        tokio::time::timeout(Duration::from_secs(1), started)
            .await
            .unwrap()
            .unwrap();
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        let response = services(State(app.clone()), Json(ServiceRequest::Ping)).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while app.service_slots.available_permits() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
