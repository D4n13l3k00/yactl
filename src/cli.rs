#[path = "terminal.rs"]
mod terminal;
use crate::{
    client::{request, request_with_timeout},
    config::{RestartPolicy, ServiceConfig, validate_name},
    protocol::Request,
};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use serde_json::Value;
use std::{
    error::Error,
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

#[derive(Parser, Debug)]
#[command(
    name = "yactl",
    version,
    about = "Manage background services. Programs must run in the foreground.",
    after_help = "Service commands also accept the 'services' prefix: yactl services status"
)]
pub struct Cli {
    #[arg(
        long,
        global = true,
        default_value = "/data/.yactl",
        help = "Directory for service configs, socket and logs"
    )]
    pub root: PathBuf,
    #[arg(long, global = true, help = "Print the response as JSON")]
    pub json: bool,
    #[command(subcommand)]
    pub command: Action,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Restart {
    Always,
    OnFailure,
    Never,
}
#[derive(Debug, Subcommand)]
pub enum Action {
    #[command(about = "Run the isolated station API worker")]
    ApiWorker,
    #[command(about = "Validate settings and token files without changing state")]
    Check,
    #[command(about = "Validate and atomically reload API settings")]
    DaemonReload,
    #[command(about = "Show station API health")]
    Health,
    #[command(about = "Show verified hardware capabilities")]
    Capabilities,
    #[command(about = "Inspect or change the station master volume")]
    Volume {
        #[command(subcommand)]
        command: VolumeAction,
    },
    #[command(about = "Inspect the station")]
    Device {
        #[command(subcommand)]
        command: DeviceAction,
    },
    #[command(about = "Start the service manager")]
    Daemon {
        #[arg(long, help = "Run in the background")]
        detach: bool,
    },
    #[command(about = "Add and start a service: add NAME -- /path/program args")]
    Add {
        name: String,
        #[arg(long, help = "Add without starting or enabling at boot")]
        disabled: bool,
        #[arg(long, help = "Working directory")]
        cwd: Option<PathBuf>,
        #[arg(long="env", value_name="KEY=VALUE", value_parser=parse_env)]
        environment: Vec<(String, String)>,
        #[arg(long, value_enum, default_value = "always")]
        restart: Restart,
        #[arg(long, default_value_t = 1000)]
        restart_delay_ms: u64,
        #[arg(long, default_value_t = 3000)]
        stop_timeout_ms: u64,
        #[arg(long, default_value_t = 256)]
        log_max_kib: u64,
        #[arg(last=true, required=true, num_args=1..)]
        command: Vec<String>,
    },
    #[command(about = "Start a service")]
    Start { name: String },
    #[command(about = "Stop a service and wait for it to exit")]
    Stop { name: String },
    #[command(about = "Restart a service")]
    Restart { name: String },
    #[command(about = "Show one or all services")]
    Status { name: Option<String> },
    #[command(about = "List services")]
    List,
    #[command(about = "Show service configuration and status")]
    Show { name: String },
    #[command(about = "Enable startup with the manager")]
    Enable {
        name: String,
        #[arg(long, help = "Also start now")]
        now: bool,
    },
    #[command(about = "Disable startup with the manager")]
    Disable {
        name: String,
        #[arg(long, help = "Also stop now")]
        now: bool,
    },
    #[command(about = "Remove a stopped service; keep its logs")]
    Remove { name: String },
    #[command(about = "Reload a service configuration")]
    Reload { name: String },
    #[command(about = "Show service logs")]
    Logs {
        name: String,
        #[arg(short = 'n', default_value_t = 50)]
        lines: usize,
        #[arg(short = 'f', long = "follow")]
        follow: bool,
    },
    #[command(about = "Stop all services and shut down the manager")]
    Shutdown,
    #[command(about = "Generate fish completions")]
    Completions {
        #[arg(value_enum)]
        shell: Shell,
    },
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Shell {
    Fish,
}
#[derive(Debug, Subcommand)]
pub enum DeviceAction {
    Info,
}
#[derive(Debug, Subcommand)]
pub enum VolumeAction {
    #[command(about = "Show current volume and provider readiness")]
    Status,
    #[command(about = "Set the master level, optionally for a bounded lease")]
    Set {
        #[arg(value_parser=clap::value_parser!(u8).range(0..=100))]
        percent: u8,
        #[arg(long, value_parser=clap::value_parser!(u16).range(100..=5000))]
        lease_ms: Option<u16>,
    },
}
fn parse_env(s: &str) -> Result<(String, String), String> {
    let (k, v) = s.split_once('=').ok_or("expected KEY=VALUE")?;
    if k.is_empty() || k.contains('\0') || v.contains('\0') {
        return Err("invalid environment variable".into());
    }
    Ok((k.into(), v.into()))
}
fn executable(s: &str) -> io::Result<String> {
    let candidate = if s.contains('/') {
        std::path::absolute(s)?
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|p| p.join(s))
            .find(|p| {
                p.metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
            .ok_or_else(|| io::Error::other(format!("program not found in PATH: {s}")))?
    };
    // Preserve the executable basename: BusyBox applets select behavior via argv[0].
    let path = std::path::absolute(candidate)?;
    let m = path.metadata()?;
    if !m.is_file() || m.permissions().mode() & 0o111 == 0 {
        return Err(io::Error::other("program is not an executable file"));
    }
    path.into_os_string()
        .into_string()
        .map_err(|_| io::Error::other("program path is not valid UTF-8"))
}
impl Cli {
    pub fn parse_entry() -> Self {
        let mut args: Vec<_> = std::env::args_os().collect();
        // Normalize only the command position, never a service name or program argv.
        let mut i = 1;
        while i < args.len() {
            if args[i] == "--root" {
                i += 2;
            } else if args[i] == "--json"
                || args[i].to_str().is_some_and(|s| s.starts_with("--root="))
            {
                i += 1;
            } else {
                if args[i] == "services" {
                    args.remove(i);
                }
                break;
            }
        }
        let matches = Self::command().get_matches_from(args);
        <Self as clap::FromArgMatches>::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())
    }

    pub fn execute(&self) -> Result<(), Box<dyn Error>> {
        let data = match &self.command {
            Action::Daemon { .. } | Action::ApiWorker => {
                return Err(io::Error::other("daemon must be handled by the entry point").into());
            }
            Action::Check => {
                let settings = crate::settings::Settings::load(&self.root)?;
                settings.credentials()?;
                serde_json::json!({"valid":true,"version":settings.version})
            }
            Action::DaemonReload => {
                crate::api_client::request(&self.root, "POST", "/v1/config/reload", Value::Null)?
            }
            Action::Health => {
                crate::api_client::request(&self.root, "GET", "/v1/health", Value::Null)?
            }
            Action::Capabilities => {
                crate::api_client::request(&self.root, "GET", "/v1/capabilities", Value::Null)?
            }
            Action::Volume { command } => match command {
                VolumeAction::Status => {
                    crate::api_client::request(&self.root, "GET", "/v1/volume", Value::Null)?
                }
                VolumeAction::Set { percent, lease_ms } => crate::api_client::request(
                    &self.root,
                    "POST",
                    "/v1/volume",
                    serde_json::json!({"percent":percent,"lease_ms":lease_ms}),
                )?,
            },
            Action::Device { .. } => {
                crate::api_client::request(&self.root, "GET", "/v1/device", Value::Null)?
            }
            Action::Completions { .. } => {
                clap_complete::generate(
                    clap_complete::Shell::Fish,
                    &mut Self::command(),
                    "yactl",
                    &mut io::stdout(),
                );
                return Ok(());
            }
            Action::Logs {
                name,
                lines,
                follow,
            } => {
                validate_name(name)?;
                return self.logs(name, *lines, *follow);
            }
            Action::Add {
                name,
                disabled,
                cwd,
                environment,
                restart,
                restart_delay_ms,
                stop_timeout_ms,
                log_max_kib,
                command,
            } => {
                let mut argv = command.clone();
                argv[0] = executable(&argv[0])?;
                let mut config = ServiceConfig::new(name.clone(), argv);
                config.enabled = !*disabled;
                config.working_dir = cwd.as_ref().map(fs::canonicalize).transpose()?;
                config.environment = environment.iter().cloned().collect();
                config.restart = match restart {
                    Restart::Always => RestartPolicy::Always,
                    Restart::OnFailure => RestartPolicy::OnFailure,
                    Restart::Never => RestartPolicy::Never,
                };
                config.restart_delay_ms = *restart_delay_ms;
                config.stop_timeout_ms = *stop_timeout_ms;
                config.log_max_bytes = log_max_kib
                    .checked_mul(1024)
                    .ok_or("log size is too large")?;
                config.validate()?;
                request(&self.root, Request::Add { config })?
            }
            Action::Start { name } => request(&self.root, Request::Start { name: name.clone() })?,
            Action::Stop { name } => {
                request(&self.root, Request::Stop { name: name.clone() })?;
                self.wait_service(name, None)?
            }
            Action::Restart { name } => {
                let old =
                    request(&self.root, Request::Get { name: name.clone() })?["status"]["pid"]
                        .as_u64();
                request(&self.root, Request::Restart { name: name.clone() })?;
                self.wait_service(name, Some(old))?
            }
            Action::Status { name: Some(name) } | Action::Show { name } => {
                request(&self.root, Request::Get { name: name.clone() })?
            }
            Action::Status { name: None } | Action::List => request(&self.root, Request::List)?,
            Action::Enable { name, now } | Action::Disable { name, now } => {
                let enabled = matches!(self.command, Action::Enable { .. });
                let result = request(
                    &self.root,
                    Request::Enable {
                        name: name.clone(),
                        enabled,
                    },
                )?;
                if *now {
                    if enabled {
                        request(&self.root, Request::Start { name: name.clone() })?
                    } else {
                        request(&self.root, Request::Stop { name: name.clone() })?;
                        self.wait_service(name, None)?
                    }
                } else {
                    result
                }
            }
            Action::Remove { name } => request(&self.root, Request::Remove { name: name.clone() })?,
            Action::Reload { name } => request(&self.root, Request::Reload { name: name.clone() })?,
            Action::Shutdown => {
                let result = request(&self.root, Request::Shutdown)?;
                let until = Instant::now() + Duration::from_secs(35);
                while self.root.join("control.sock").exists() {
                    if Instant::now() >= until {
                        return Err("manager shutdown timed out".into());
                    }
                    std::thread::sleep(
                        Duration::from_millis(100)
                            .min(until.saturating_duration_since(Instant::now())),
                    );
                }
                result
            }
        };
        if self.json {
            println!("{}", serde_json::to_string_pretty(&data)?);
        } else if matches!(self.command, Action::Volume { .. }) {
            let current = data.get("status").unwrap_or(&data);
            terminal::heading(
                "Volume",
                if current["ready"] == true {
                    "ready"
                } else {
                    "unavailable"
                },
            );
            if let Some(percent) = current["percent"].as_f64() {
                terminal::field("Level", format!("{percent:.1}%"));
            }
            if current["temporary"] == true {
                terminal::field(
                    "Lease",
                    format!("{} ms remaining", current["lease_remaining_ms"]),
                );
            }
            if let Some(error) = current["error"].as_str() {
                terminal::field("Error", error);
            }
        } else {
            print_human(&data, matches!(self.command, Action::Show { .. }));
        }
        Ok(())
    }
    fn wait_service(
        &self,
        name: &str,
        restart: Option<Option<u64>>,
    ) -> Result<Value, Box<dyn Error>> {
        let until = Instant::now() + Duration::from_secs(35);
        loop {
            let remaining = until
                .checked_duration_since(Instant::now())
                .ok_or("timed out waiting for the service")?;
            let data = request_with_timeout(
                &self.root,
                Request::Get { name: name.into() },
                remaining.min(Duration::from_secs(2)),
            )?;
            let s = &data["status"];
            let state = s["state"].as_str().unwrap_or("");
            let done = match restart {
                None => matches!(state, "stopped" | "exited" | "failed"),
                Some(old) => {
                    (state == "running" && s["pid"].as_u64() != old)
                        || matches!(state, "failed" | "exited" | "backoff")
                }
            };
            if done {
                return Ok(data);
            }
            if Instant::now() >= until {
                return Err("timed out waiting for the service".into());
            }
            std::thread::sleep(
                Duration::from_millis(100).min(until.saturating_duration_since(Instant::now())),
            );
        }
    }
    fn logs(&self, name: &str, lines: usize, follow: bool) -> Result<(), Box<dyn Error>> {
        let path = self.root.join("logs").join(format!("{name}.log"));
        let mut file = open_follow(&path)?;
        let mut offset = file
            .as_ref()
            .map(|f| f.metadata().map(|m| m.len()))
            .transpose()?
            .unwrap_or(0);
        print!("{}", crate::logs::tail(&path, lines)?);
        io::stdout().flush()?;
        if !follow {
            return Ok(());
        }
        crate::install_signals();
        while !crate::STOP.load(Ordering::Relaxed) {
            if let Some(f) = file.as_mut() {
                if f.metadata()?.len() < offset {
                    offset = 0;
                }
                f.seek(SeekFrom::Start(offset))?;
                let n = io::copy(&mut f.take(16 * 1024 * 1024), &mut io::stdout())?;
                offset += n;
                io::stdout().flush()?;
            }
            if let Some(next) = open_follow(&path)? {
                let changed = match file.as_ref() {
                    Some(f) => {
                        let a = f.metadata()?;
                        let b = next.metadata()?;
                        a.ino() != b.ino() || a.dev() != b.dev()
                    }
                    None => true,
                };
                if changed {
                    file = Some(next);
                    offset = 0;
                    continue;
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Ok(())
    }
}
fn open_follow(path: &Path) -> io::Result<Option<fs::File>> {
    match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(f) if f.metadata()?.is_file() => Ok(Some(f)),
        Ok(_) => Err(io::Error::other("log is not a regular file")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
pub fn duration(seconds: u64) -> String {
    if seconds >= 86400 {
        format!("{}d {}h", seconds / 86400, seconds / 3600 % 24)
    } else if seconds >= 3600 {
        format!("{}h {}m", seconds / 3600, seconds / 60 % 60)
    } else if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

fn print_human(data: &Value, show_config: bool) {
    use terminal::{field, heading, paint, state};
    fn text<'a>(v: &'a Value, key: &str) -> &'a str {
        v[key].as_str().unwrap_or("-")
    }
    fn details(s: &Value) {
        heading(text(s, "name"), &state(text(s, "state")));
        field(
            "PID",
            s["pid"]
                .as_u64()
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".into()),
        );
        field(
            "Uptime",
            s["uptime_secs"]
                .as_u64()
                .map(duration)
                .unwrap_or_else(|| "-".into()),
        );
        field(
            "Autostart",
            if s["enabled"] == true {
                "enabled"
            } else {
                "disabled"
            },
        );
        field("Restarts", &s["restart_count"]);
        if let Some(ms) = s["next_start_ms"].as_u64() {
            field("Retry in", format!("{:.1}s", ms as f64 / 1000.0));
        }
        if let Some(code) = s["last_exit"].as_i64() {
            field(
                "Last exit",
                if code < 0 {
                    format!("signal {}", -code)
                } else {
                    code.to_string()
                },
            );
        }
        if let Some(e) = s["error"].as_str() {
            field("Error", paint(e, "31"));
        }
    }
    if let Some(services) = data["services"].as_array() {
        heading(
            "yactl",
            &format!(
                "{} service{} · PID {} · up {}",
                services.len(),
                if services.len() == 1 { "" } else { "s" },
                data["manager_pid"],
                duration(data["uptime_secs"].as_u64().unwrap_or(0))
            ),
        );
        if services.is_empty() {
            println!("  No services. Add one with: yactl services add NAME -- /path/program");
        } else {
            let width = services
                .iter()
                .filter_map(|s| s["name"].as_str())
                .map(str::len)
                .max()
                .unwrap_or(7)
                .max(7);
            println!(
                "  {}",
                paint(
                    &format!(
                        "{:<width$}  {:<10}  {:>7}  {:<9}  {:>8}  {}",
                        "SERVICE", "STATE", "PID", "AUTOSTART", "RESTARTS", "UPTIME"
                    ),
                    "2"
                )
            );
            for s in services {
                let status = format!("{:<10}", text(s, "state"));
                println!(
                    "  {:<width$}  {}  {:>7}  {:<9}  {:>8}  {}",
                    text(s, "name"),
                    state(status.trim()).replace(status.trim(), &status),
                    s["pid"]
                        .as_u64()
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "-".into()),
                    if s["enabled"] == true { "yes" } else { "no" },
                    s["restart_count"].to_string(),
                    s["uptime_secs"]
                        .as_u64()
                        .map(duration)
                        .unwrap_or_else(|| "-".into())
                );
                if let Some(e) = s["error"].as_str() {
                    println!("    {}", paint(e, "31"));
                }
            }
        }
        if let Some(invalid) = data["invalid"].as_array() {
            for i in invalid {
                println!(
                    "  {}  {}: {}",
                    state("invalid"),
                    text(i, "name"),
                    text(i, "error")
                );
            }
        }
    } else if let Some(s) = data.get("status") {
        details(s);
        if show_config && let Some(c) = data.get("config") {
            field(
                "Command",
                c["command"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|v| format!("{:?}", v.as_str().unwrap_or("")))
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default(),
            );
            field("Directory", c["working_dir"].as_str().unwrap_or("/"));
            field(
                "Restart",
                format!(
                    "{} · delay {} ms",
                    text(c, "restart"),
                    c["restart_delay_ms"]
                ),
            );
            field("Stop timeout", format!("{} ms", c["stop_timeout_ms"]));
            field(
                "Log limit",
                format!(
                    "{} KiB × 2",
                    c["log_max_bytes"].as_u64().unwrap_or(0) / 1024
                ),
            );
            if let Some(env) = c["environment"].as_object() {
                for (k, v) in env {
                    field("Environment", format!("{k}={}", v.as_str().unwrap_or("")));
                }
            }
        }
    } else if data["removed"] == true {
        println!(
            "{} Removed {}. Logs retained.",
            paint("OK", "32"),
            text(data, "name")
        );
    } else if data["shutdown"] == true {
        println!("{} Manager stopped.", paint("OK", "32"));
    } else {
        println!("{data}");
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn executable_preserves_applet_symlink() {
        let dir = std::env::temp_dir().join(format!("yactl-applet-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let link = dir.join("sleep");
        std::os::unix::fs::symlink("/bin/sh", &link).unwrap();
        assert_eq!(
            executable(link.to_str().unwrap()).unwrap(),
            link.to_str().unwrap()
        );
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn cli_argv_and_env() {
        assert_eq!(
            Cli::try_parse_from(["yactl", "--version"])
                .unwrap_err()
                .kind(),
            clap::error::ErrorKind::DisplayVersion
        );
        let cli = Cli::try_parse_from([
            "yactl",
            "add",
            "sample",
            "--env",
            "X=a=b",
            "--restart",
            "on-failure",
            "--",
            "/bin/echo",
            "$HOME",
            "a b",
        ])
        .unwrap();
        match cli.command {
            Action::Add {
                command,
                environment,
                restart,
                ..
            } => {
                assert_eq!(command, ["/bin/echo", "$HOME", "a b"]);
                assert_eq!(environment, [("X".into(), "a=b".into())]);
                assert!(matches!(restart, Restart::OnFailure));
            }
            _ => panic!(),
        }
        assert!(
            Cli::try_parse_from(["yactl", "add", "a", "--env", "bad", "--", "/bin/true"]).is_err()
        );
    }
}
