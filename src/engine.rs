use crate::{
    config::{ConfigStore, RestartPolicy, ServiceConfig, validate_name},
    logs::LogWriter,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read},
    os::{
        fd::AsRawFd,
        unix::process::{CommandExt, ExitStatusExt},
    },
    path::Path,
    process::{Child, ChildStderr, ChildStdout, Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn nonblocking(fd: libc::c_int) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn exited_unreaped(child: &Child) -> io::Result<bool> {
    // WNOWAIT observes an exit while the zombie still reserves its PID/PGID.
    let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
    if unsafe {
        libc::waitid(
            libc::P_PID,
            child.id() as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    } < 0
    {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(error);
    }
    Ok(unsafe { info.si_pid() } != 0)
}
fn signal_group(pid: u32, signal: libc::c_int) -> io::Result<()> {
    // Only IDs held by live Child handles are used for process-group signaling.
    let pid = i32::try_from(pid).map_err(|_| io::Error::other("invalid process group"))?;
    if pid <= 1 {
        return Err(io::Error::other("refusing invalid process group"));
    }
    if unsafe { libc::kill(-pid, signal) } == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(e)
    }
}
fn drain<R: Read>(reader: &mut Option<R>, log: &mut LogWriter) -> io::Result<()> {
    let mut ended = false;
    if let Some(reader) = reader {
        let mut buf = [0u8; 4096];
        for _ in 0..16 {
            match reader.read(&mut buf) {
                Ok(0) => {
                    ended = true;
                    break;
                }
                Ok(n) => log.append(&buf[..n])?,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }
    if ended {
        *reader = None;
    }
    Ok(())
}
struct Service {
    config: ServiceConfig,
    log: LogWriter,
    child: Option<Child>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    wanted: bool,
    stopping: Option<Instant>,
    restart_pending: bool,
    next_start: Option<Instant>,
    started: Option<Instant>,
    ever_started: bool,
    restart_count: u64,
    failures: u32,
    last_exit: Option<i32>,
    completed: bool,
    failed: bool,
    error: Option<String>,
}
impl Service {
    fn new(config: ServiceConfig, root: &Path) -> io::Result<Self> {
        let log = LogWriter::new(
            &root.join("logs").join(format!("{}.log", config.name)),
            config.log_max_bytes,
        )?;
        let enabled = config.enabled;
        Ok(Self {
            config,
            log,
            child: None,
            stdout: None,
            stderr: None,
            wanted: enabled,
            stopping: None,
            restart_pending: false,
            next_start: enabled.then(Instant::now),
            started: None,
            ever_started: false,
            restart_count: 0,
            failures: 0,
            last_exit: None,
            completed: false,
            failed: false,
            error: None,
        })
    }
    fn event(&mut self, text: &str) {
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if let Err(e) = self
            .log
            .append(format!("[yactl {time}] {text}\n").as_bytes())
        {
            self.error = Some(format!("log: {e}"));
        }
    }
    fn spawn(&mut self) -> io::Result<()> {
        let mut command = Command::new(&self.config.command[0]);
        command.args(&self.config.command[1..]).current_dir(self.config.working_dir.as_deref().unwrap_or(Path::new("/")))
            .env_remove("LD_LIBRARY_PATH").env_remove("LD_PRELOAD")
            .env("PATH","/data/opt/local/bin:/data/opt/bin:/opt/bin:/opt/sbin:/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME","/data/opt/home").envs(&self.config.environment)
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
        let parent = std::process::id() as libc::pid_t;
        // SAFETY: the pre-exec hook only makes async-signal-safe syscalls and creates raw OS errors.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    return Err(io::Error::from_raw_os_error(libc::ECHILD));
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        if let Err(e) =
            nonblocking(stdout.as_raw_fd()).and_then(|_| nonblocking(stderr.as_raw_fd()))
        {
            let _ = signal_group(child.id(), libc::SIGKILL);
            let _ = child.wait();
            return Err(e);
        }
        if self.ever_started {
            self.restart_count += 1;
        }
        self.ever_started = true;
        self.started = Some(Instant::now());
        self.next_start = None;
        self.stopping = None;
        self.completed = false;
        self.failed = false;
        self.error = None;
        self.event(&format!("started pid={}", child.id()));
        self.child = Some(child);
        self.stdout = Some(stdout);
        self.stderr = Some(stderr);
        Ok(())
    }
    fn schedule(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        let delay = self
            .config
            .restart_delay_ms
            .saturating_mul(1u64 << self.failures.saturating_sub(1).min(16))
            .min(30000);
        self.next_start = Some(now + Duration::from_millis(delay));
        self.event(&format!("restart in {delay}ms (failure {})", self.failures));
    }
    fn request_start(&mut self) {
        self.wanted = true;
        self.completed = false;
        self.failed = false;
        self.failures = 0;
        if self.stopping.is_some() {
            self.restart_pending = true;
        } else if self.child.is_none() {
            self.next_start = Some(Instant::now());
        }
    }
    fn request_stop(&mut self) {
        self.wanted = false;
        self.next_start = None;
        self.restart_pending = false;
        self.completed = false;
        self.failed = false;
        self.begin_stop();
    }
    fn request_restart(&mut self) {
        self.wanted = true;
        self.failures = 0;
        self.completed = false;
        self.failed = false;
        if self.child.is_some() {
            self.restart_pending = true;
            self.begin_stop();
        } else {
            self.next_start = Some(Instant::now());
        }
    }
    fn begin_stop(&mut self) {
        if self.stopping.is_none()
            && let Some(child) = &self.child
        {
            let pid = child.id();
            if let Err(e) = signal_group(pid, libc::SIGTERM) {
                self.error = Some(e.to_string());
            }
            self.stopping =
                Some(Instant::now() + Duration::from_millis(self.config.stop_timeout_ms));
            self.event(&format!("stopping pid={pid}"));
        }
    }
    fn tick(&mut self) {
        let now = Instant::now();
        if let Err(e) = drain(&mut self.stdout, &mut self.log)
            .and_then(|_| drain(&mut self.stderr, &mut self.log))
        {
            self.error = Some(format!("output: {e}"));
        }
        if let Some(child) = &mut self.child {
            let exited = exited_unreaped(child).and_then(|exited| {
                if exited {
                    let _ = signal_group(child.id(), libc::SIGKILL);
                    child.wait().map(Some)
                } else {
                    Ok(None)
                }
            });
            match exited {
                Ok(Some(exit)) => {
                    // A foreground leader exiting must not leave same-group workers behind.
                    let pid = child.id();
                    let was_stopping = self.stopping.take().is_some();
                    self.last_exit = exit.code().or_else(|| exit.signal().map(|s| -s));
                    let lived = self.started.take().map(|t| t.elapsed()).unwrap_or_default();
                    self.child = None;
                    let _ = drain(&mut self.stdout, &mut self.log);
                    let _ = drain(&mut self.stderr, &mut self.log);
                    self.stdout = None;
                    self.stderr = None;
                    self.event(&format!(
                        "exited pid={pid} status={}",
                        self.last_exit.map_or("unknown".into(), |n| n.to_string())
                    ));
                    if self.restart_pending && self.wanted {
                        self.restart_pending = false;
                        self.next_start = Some(now);
                    } else if !self.wanted {
                        self.next_start = None;
                    } else if !was_stopping
                        && (self.config.restart == RestartPolicy::Never
                            || (self.config.restart == RestartPolicy::OnFailure && exit.success()))
                    {
                        self.wanted = false;
                        self.next_start = None;
                        self.completed = true;
                        self.failed = !exit.success();
                    } else {
                        if lived >= Duration::from_secs(5) {
                            self.failures = 0;
                        }
                        if !exit.success() {
                            self.error =
                                Some(format!("process exited: {}", self.last_exit.unwrap_or(-1)));
                        }
                        self.schedule(now);
                    }
                }
                Ok(None) => {
                    if self.stopping.is_some_and(|t| now >= t) {
                        let pid = child.id();
                        let _ = signal_group(pid, libc::SIGKILL);
                        self.stopping = Some(now + Duration::from_secs(1));
                        self.event(&format!("SIGKILL pid={pid}"));
                    }
                }
                Err(e) => {
                    self.error = Some(format!("wait: {e}"));
                    self.request_stop();
                }
            }
        }
        if self.child.is_none()
            && self.wanted
            && self.next_start.is_some_and(|t| now >= t)
            && let Err(e) = self.spawn()
        {
            self.error = Some(format!("spawn: {e}"));
            self.event(&format!("spawn failed: {e}"));
            if self.config.restart == RestartPolicy::Never {
                self.wanted = false;
                self.next_start = None;
                self.failed = true;
            } else {
                self.schedule(now);
            }
        }
    }
    fn status(&self) -> Value {
        let state = if self.child.is_some() {
            if self.stopping.is_some() {
                "stopping"
            } else {
                "running"
            }
        } else if self.wanted {
            "backoff"
        } else if self.failed {
            "failed"
        } else if self.completed {
            "exited"
        } else {
            "stopped"
        };
        json!({"name":self.config.name,"state":state,"pid":self.child.as_ref().map(Child::id),"enabled":self.config.enabled,"restart_count":self.restart_count,"failures":self.failures,"last_exit":self.last_exit,"next_start_ms":self.next_start.map(|t|t.saturating_duration_since(Instant::now()).as_millis() as u64),"error":self.error,"uptime_secs":self.started.map(|t|t.elapsed().as_secs())})
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = signal_group(child.id(), libc::SIGKILL);
            let _ = child.try_wait();
        }
    }
}

pub struct Supervisor {
    pub store: ConfigStore,
    services: BTreeMap<String, Service>,
    invalid: BTreeMap<String, String>,
    started: Instant,
}
impl Supervisor {
    pub fn new(root: &Path) -> io::Result<Self> {
        let store = ConfigStore::new(root)?;
        let mut services = BTreeMap::new();
        let mut invalid = BTreeMap::new();
        for (name, config) in store.all()? {
            match config.and_then(|c| Service::new(c, root).map_err(|e| e.to_string())) {
                Ok(service) => {
                    services.insert(name, service);
                }
                Err(error) => {
                    invalid.insert(name, error);
                }
            }
        }
        let mut this = Self {
            store,
            services,
            invalid,
            started: Instant::now(),
        };
        this.tick();
        Ok(this)
    }
    pub fn tick(&mut self) {
        for service in self.services.values_mut() {
            service.tick();
        }
    }
    pub fn reap_orphans(&self) {
        let managed: BTreeSet<_> = self
            .services
            .values()
            .filter_map(|s| s.child.as_ref().map(Child::id))
            .collect();
        let p = std::process::id();
        if let Ok(children) = fs::read_to_string(format!("/proc/self/task/{p}/children")) {
            for pid in children
                .split_whitespace()
                .filter_map(|s| s.parse::<u32>().ok())
                .filter(|p| !managed.contains(p))
            {
                if let Ok(pid) = i32::try_from(pid) {
                    unsafe {
                        libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG);
                    }
                }
            }
        }
    }
    pub fn list(&self) -> Value {
        json!({"manager_pid":std::process::id(),"uptime_secs":self.started.elapsed().as_secs(),"services":self.services.values().map(Service::status).collect::<Vec<_>>(),"invalid":self.invalid.iter().map(|(name,error)|json!({"name":name,"error":error})).collect::<Vec<_>>()})
    }
    pub fn ping(&self) -> Value {
        json!({"pid":std::process::id(),"uptime_secs":self.started.elapsed().as_secs()})
    }
    pub fn get(&self, name: &str) -> Result<Value, String> {
        validate_name(name)?;
        let s = self.services.get(name).ok_or_else(|| {
            self.invalid
                .get(name)
                .map_or(format!("unknown service: {name}"), |e| {
                    format!("invalid config for {name}: {e}")
                })
        })?;
        Ok(json!({"config":s.config,"status":s.status()}))
    }
    pub fn add(&mut self, config: ServiceConfig) -> Result<Value, String> {
        config.validate()?;
        let name = config.name.clone();
        if self.services.contains_key(&name) || self.invalid.contains_key(&name) {
            return Err("service already exists; use reload after editing its JSON".into());
        }
        let service = Service::new(config.clone(), &self.store.root).map_err(|e| e.to_string())?;
        self.store.save(&config).map_err(|e| e.to_string())?;
        self.services.insert(name.clone(), service);
        self.services.get_mut(&name).unwrap().tick();
        self.get(&name)
    }
    fn service(&mut self, name: &str) -> Result<&mut Service, String> {
        validate_name(name)?;
        self.services
            .get_mut(name)
            .ok_or(format!("unknown service: {name}"))
    }
    pub fn start(&mut self, name: &str) -> Result<Value, String> {
        let s = self.service(name)?;
        s.request_start();
        s.tick();
        self.get(name)
    }
    pub fn stop(&mut self, name: &str) -> Result<Value, String> {
        let s = self.service(name)?;
        s.request_stop();
        s.tick();
        self.get(name)
    }
    pub fn restart(&mut self, name: &str) -> Result<Value, String> {
        let s = self.service(name)?;
        s.request_restart();
        s.tick();
        self.get(name)
    }
    pub fn enable(&mut self, name: &str, enabled: bool) -> Result<Value, String> {
        let mut c = self.service(name)?.config.clone();
        c.enabled = enabled;
        self.store.save(&c).map_err(|e| e.to_string())?;
        self.service(name)?.config = c;
        self.get(name)
    }
    pub fn remove(&mut self, name: &str) -> Result<Value, String> {
        validate_name(name)?;
        if self
            .services
            .get(name)
            .is_some_and(|s| s.child.is_some() || s.wanted)
        {
            return Err("stop the service before removing it".into());
        }
        if !self.services.contains_key(name) && !self.invalid.contains_key(name) {
            return Err(format!("unknown service: {name}"));
        }
        self.store.remove(name).map_err(|e| e.to_string())?;
        self.services.remove(name);
        self.invalid.remove(name);
        Ok(json!({"name":name,"removed":true}))
    }
    pub fn reload(&mut self, name: &str) -> Result<Value, String> {
        let c = self.store.load(name).map_err(|e| e.to_string())?;
        if let Some(s) = self.services.get_mut(name) {
            let log = LogWriter::new(
                &self.store.root.join("logs").join(format!("{name}.log")),
                c.log_max_bytes,
            )
            .map_err(|e| e.to_string())?;
            if s.wanted {
                s.request_restart();
            }
            s.config = c;
            s.log = log;
            s.tick();
        } else {
            self.services.insert(
                name.into(),
                Service::new(c, &self.store.root).map_err(|e| e.to_string())?,
            );
            self.services.get_mut(name).unwrap().tick();
        }
        self.invalid.remove(name);
        self.get(name)
    }
    pub fn shutdown(&mut self) {
        for s in self.services.values_mut() {
            s.request_stop();
        }
        let deadline = Instant::now() + Duration::from_secs(35);
        while self.services.values().any(|s| s.child.is_some()) && Instant::now() < deadline {
            self.tick();
            self.reap_orphans();
            std::thread::sleep(Duration::from_millis(20));
        }
        self.reap_orphans();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    fn root(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("yactl-engine-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }
    fn wait(s: &mut Supervisor, name: &str, state: &str) {
        let end = Instant::now() + Duration::from_secs(3);
        while Instant::now() < end {
            s.tick();
            if s.get(name).unwrap()["status"]["state"] == state {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("state: {}", s.get(name).unwrap());
    }
    #[test]
    fn literal_arguments_environment_logs_and_stop() {
        let root = root("literal");
        let mut s = Supervisor::new(&root).unwrap();
        let mut c = ServiceConfig::new(
            "hello".into(),
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf '%s|%s|%s\\n' \"$PWD\" \"$HELLO\" \"$1\"; echo stderr >&2; exec sleep 30"
                    .into(),
                "service".into(),
                "x; touch SHOULD_NOT_EXIST".into(),
            ],
        );
        c.working_dir = Some(root.clone());
        c.environment.insert("HELLO".into(), "world".into());
        c.restart_delay_ms = 100;
        s.add(c).unwrap();
        wait(&mut s, "hello", "running");
        for _ in 0..10 {
            s.tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        let log = fs::read_to_string(root.join("logs/hello.log")).unwrap();
        assert!(log.contains("world|x; touch SHOULD_NOT_EXIST"));
        assert!(log.contains("stderr"));
        assert!(!root.join("SHOULD_NOT_EXIST").exists());
        s.stop("hello").unwrap();
        wait(&mut s, "hello", "stopped");
        for _ in 0..15 {
            s.tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(s.get("hello").unwrap()["status"]["pid"].is_null());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn crash_backoff_does_not_spin_and_stop_cancels_retry() {
        let root = root("backoff");
        let mut s = Supervisor::new(&root).unwrap();
        let mut c = ServiceConfig::new(
            "crash".into(),
            vec!["/bin/sh".into(), "-c".into(), "exit 7".into()],
        );
        c.restart_delay_ms = 100;
        s.add(c).unwrap();
        wait(&mut s, "crash", "backoff");
        assert_eq!(s.get("crash").unwrap()["status"]["last_exit"], 7);
        let first = s.get("crash").unwrap()["status"]["next_start_ms"]
            .as_u64()
            .unwrap();
        assert!(first <= 100);
        std::thread::sleep(Duration::from_millis(120));
        s.tick();
        wait(&mut s, "crash", "backoff");
        assert!(
            s.get("crash").unwrap()["status"]["failures"]
                .as_u64()
                .unwrap()
                >= 2
        );
        s.stop("crash").unwrap();
        wait(&mut s, "crash", "stopped");
        assert!(s.get("crash").unwrap()["status"]["next_start_ms"].is_null());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn never_restart_and_persistent_enable() {
        let root = root("enable");
        let mut s = Supervisor::new(&root).unwrap();
        let mut c = ServiceConfig::new("once".into(), vec!["/bin/true".into()]);
        c.restart = RestartPolicy::Never;
        s.add(c).unwrap();
        wait(&mut s, "once", "exited");
        s.enable("once", false).unwrap();
        drop(s);
        let s = Supervisor::new(&root).unwrap();
        assert_eq!(s.get("once").unwrap()["status"]["enabled"], false);
        assert!(s.get("once").unwrap()["status"]["pid"].is_null());
        drop(s);
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;
    #[test]
    fn reload_preserves_stopped_runtime_intent() {
        let root = std::env::temp_dir().join(format!("midi-reload-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mut s = Supervisor::new(&root).unwrap();
        let mut c = ServiceConfig::new("disabled".into(), vec!["/bin/sleep".into(), "30".into()]);
        c.enabled = false;
        s.add(c).unwrap();
        s.reload("disabled").unwrap();
        assert!(s.get("disabled").unwrap()["status"]["pid"].is_null());
        s.start("disabled").unwrap();
        s.stop("disabled").unwrap();
        let end = Instant::now() + Duration::from_secs(2);
        while Instant::now() < end && s.get("disabled").unwrap()["status"]["pid"].is_number() {
            s.tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        s.reload("disabled").unwrap();
        assert!(s.get("disabled").unwrap()["status"]["pid"].is_null());
        drop(s);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn removing_a_backoff_service_requires_stop() {
        let root = std::env::temp_dir().join(format!("midi-remove-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mut s = Supervisor::new(&root).unwrap();
        let c = ServiceConfig::new("crash".into(), vec!["/bin/false".into()]);
        s.add(c).unwrap();
        let end = Instant::now() + Duration::from_secs(2);
        while Instant::now() < end && s.get("crash").unwrap()["status"]["state"] != "backoff" {
            s.tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(s.remove("crash").is_err());
        s.stop("crash").unwrap();
        assert!(s.remove("crash").is_ok());
        fs::remove_dir_all(root).unwrap();
    }
}
