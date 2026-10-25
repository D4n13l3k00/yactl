use serde_json::Value;
use std::{
    error::Error,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use yactl::{
    cli::{Action, Cli},
    client,
    config::ConfigStore,
    protocol::Request,
};
type Result<T> = std::result::Result<T, Box<dyn Error>>;
struct Guard {
    socket: PathBuf,
    inode: u64,
    pid: PathBuf,
    _lock: File,
}
impl Drop for Guard {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.socket)
            .is_ok_and(|m| m.file_type().is_socket() && m.ino() == self.inode)
        {
            let _ = fs::remove_file(&self.socket);
        }
        if fs::read_to_string(&self.pid).is_ok_and(|s| s.trim() == std::process::id().to_string()) {
            let _ = fs::remove_file(&self.pid);
        }
    }
}
fn manager_lock(root: &Path) -> Result<File> {
    ConfigStore::new(root)?;
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(root.join("manager.lock"))?;
    if !f.metadata()?.is_file() {
        return Err("manager.lock must be a regular file".into());
    }
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("yactl is already running for this directory".into());
    }
    Ok(f)
}
fn bind_socket(root: &Path) -> Result<UnixListener> {
    let path = root.join("control.sock");
    match fs::symlink_metadata(&path) {
        Ok(m) => {
            if !m.file_type().is_socket() {
                return Err("control.sock exists and is not a socket".into());
            }
            match UnixStream::connect(&path) {
                Ok(_) => return Err("control socket is already active".into()),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) =>
                {
                    fs::remove_file(&path)?
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}
fn foreground(root: &Path) -> Result<()> {
    let settings = yactl::settings::Settings::load(root)?;
    settings.credentials()?;
    let lock = manager_lock(root)?;
    let listener = bind_socket(root)?;
    let socket = root.join("control.sock");
    let inode = fs::symlink_metadata(&socket)?.ino();
    let pid = root.join("manager.pid");
    let mut f = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&pid)?;
    if !f.metadata()?.is_file() {
        return Err("manager.pid must be a regular file".into());
    }
    writeln!(f, "{}", std::process::id())?;
    let _guard = Guard {
        socket,
        inode,
        pid,
        _lock: lock,
    };
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    println!("yactl pid={} root={}", std::process::id(), root.display());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(yactl::daemon::serve(root, listener))?;
    Ok(())
}
fn detach(root: &Path) -> Result<Value> {
    if let Ok(ping) = client::request(root, Request::Ping) {
        return Ok(ping);
    }
    ConfigStore::new(root)?;
    let log_path = root.join("manager.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(log_path)?;
    if !log.metadata()?.is_file() {
        return Err("manager.log must be a regular file".into());
    }
    // Manager output only records startup/fatal diagnostics, not service output.
    if log.metadata()?.len() > 262144 {
        log.set_len(0)?;
    }
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--root")
        .arg(root)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(ping) = client::request(root, Request::Ping) {
            return Ok(ping);
        }
        if let Some(exit) = child.try_wait()? {
            return Err(format!(
                "yactl exited with {exit}; see {}/manager.log",
                root.display()
            )
            .into());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "manager startup timed out; see {}/manager.log",
                root.display()
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
fn run() -> Result<()> {
    let mut cli = Cli::parse_entry();
    if !cli.root.is_absolute() {
        cli.root = std::env::current_dir()?.join(&cli.root);
    }
    if matches!(&cli.command, Action::ApiWorker) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(6)
            .build()?;
        let result = runtime.block_on(yactl::api::run(&cli.root));
        // Blocking syscalls cannot be canceled. Bound the wait at process exit;
        // remaining blocking threads end when this dedicated worker exits.
        runtime.shutdown_timeout(Duration::from_secs(1));
        result?;
        Ok(())
    } else if let Action::Daemon { detach: background } = &cli.command {
        if *background {
            let ping = detach(&cli.root)?;
            if cli.json {
                println!("{ping}");
            } else {
                println!("yactl running · PID {}", ping["pid"]);
            }
            Ok(())
        } else {
            foreground(&cli.root)
        }
    } else {
        cli.execute()
    }
}
fn main() {
    if let Err(e) = run() {
        eprintln!("yactl: {e}");
        std::process::exit(1);
    }
}
