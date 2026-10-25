use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Always,
    OnFailure,
    Never,
}
fn enabled_default() -> bool {
    true
}
fn restart_default() -> RestartPolicy {
    RestartPolicy::Always
}
fn delay_default() -> u64 {
    1000
}
fn timeout_default() -> u64 {
    3000
}
fn log_default() -> u64 {
    262144
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub name: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
    #[serde(default = "restart_default")]
    pub restart: RestartPolicy,
    #[serde(default = "delay_default")]
    pub restart_delay_ms: u64,
    #[serde(default = "timeout_default")]
    pub stop_timeout_ms: u64,
    #[serde(default = "log_default")]
    pub log_max_bytes: u64,
}
pub fn validate_name(name: &str) -> Result<(), String> {
    let b = name.as_bytes();
    if b.is_empty()
        || b.len() > 64
        || !b[0].is_ascii_alphanumeric()
        || name.contains("..")
        || !b
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || b"_-.".contains(c))
    {
        return Err("invalid service name".into());
    }
    Ok(())
}
impl ServiceConfig {
    pub fn new(name: String, command: Vec<String>) -> Self {
        Self {
            name,
            command,
            working_dir: None,
            environment: BTreeMap::new(),
            enabled: true,
            restart: RestartPolicy::Always,
            restart_delay_ms: 1000,
            stop_timeout_ms: 3000,
            log_max_bytes: 262144,
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        validate_name(&self.name)?;
        if self.command.is_empty()
            || self.command.len() > 256
            || !Path::new(&self.command[0]).is_absolute()
            || self
                .command
                .iter()
                .any(|s| s.len() > 4096 || s.contains('\0'))
        {
            return Err("command requires absolute executable and at most 256 NUL-free arguments of at most 4096 bytes".into());
        }
        if self
            .working_dir
            .as_ref()
            .is_some_and(|p| !p.is_absolute() || p.as_os_str().as_encoded_bytes().contains(&0))
        {
            return Err("working directory must be absolute and NUL-free".into());
        }
        if self
            .environment
            .iter()
            .any(|(k, v)| k.is_empty() || k.contains(['=', '\0']) || v.contains('\0'))
        {
            return Err("invalid environment entry".into());
        }
        if !(100..=30000).contains(&self.restart_delay_ms)
            || !(100..=30000).contains(&self.stop_timeout_ms)
            || !(4096..=16 * 1024 * 1024).contains(&self.log_max_bytes)
        {
            return Err("service tuning value outside allowed range".into());
        }
        Ok(())
    }
}
fn invalid(e: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}
pub(crate) fn private_dir(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(invalid("state directory is not a real directory")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut b = fs::DirBuilder::new();
            b.recursive(true).mode(0o700).create(path)
        }
        Err(e) => Err(e),
    }
}
#[derive(Clone)]
pub struct ConfigStore {
    pub root: PathBuf,
}
impl ConfigStore {
    pub fn new(root: &Path) -> io::Result<Self> {
        private_dir(root)?;
        private_dir(&root.join("services"))?;
        private_dir(&root.join("logs"))?;
        Ok(Self { root: root.into() })
    }
    fn path(&self, name: &str) -> io::Result<PathBuf> {
        validate_name(name).map_err(invalid)?;
        private_dir(&self.root)?;
        private_dir(&self.root.join("services"))?;
        Ok(self.root.join("services").join(format!("{name}.json")))
    }
    pub fn save(&self, c: &ServiceConfig) -> io::Result<()> {
        c.validate().map_err(invalid)?;
        let path = self.path(&c.name)?;
        if fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file() || m.file_type().is_symlink()) {
            return Err(invalid("config is not a regular file"));
        }
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let tmp = path.with_extension(format!(
            "tmp-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(c).map_err(invalid)?)?;
            f.write_all(b"\n")?;
            f.sync_all()?;
            fs::rename(&tmp, &path)?;
            fs::File::open(self.root.join("services"))?.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(tmp);
        }
        result
    }
    pub fn load(&self, name: &str) -> io::Result<ServiceConfig> {
        let p = self.path(name)?;
        let f = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(p)?;
        if !f.metadata()?.is_file() || f.metadata()?.len() > 2 * 1024 * 1024 {
            return Err(invalid("invalid or oversized config file"));
        }
        let mut bytes = Vec::new();
        f.take(2 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 2 * 1024 * 1024 {
            return Err(invalid("oversized config file"));
        }
        let c: ServiceConfig = serde_json::from_slice(&bytes).map_err(invalid)?;
        c.validate().map_err(invalid)?;
        if c.name != name {
            return Err(invalid("config name differs from filename"));
        }
        Ok(c)
    }
    pub fn all(&self) -> io::Result<Vec<(String, Result<ServiceConfig, String>)>> {
        private_dir(&self.root.join("services"))?;
        let mut result = Vec::new();
        for e in fs::read_dir(self.root.join("services"))? {
            let e = e?;
            let p = e.path();
            if p.extension().is_some_and(|x| x == "json") {
                let name = p.file_stem().unwrap().to_string_lossy().into_owned();
                let c = self.load(&name).map_err(|e| e.to_string());
                result.push((name, c));
            }
        }
        result.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(result)
    }
    pub fn remove(&self, name: &str) -> io::Result<()> {
        fs::remove_file(self.path(name)?)?;
        fs::File::open(self.root.join("services"))?.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_and_limits() {
        for name in ["", ".", "..", "a/b", "_bad", "a..b", "é"] {
            assert!(validate_name(name).is_err(), "{name}");
        }
        let mut c = ServiceConfig::new("ok-1.x".into(), vec!["/bin/true".into()]);
        assert!(c.validate().is_ok());
        c.command[0] = "true".into();
        assert!(c.validate().is_err());
    }
    #[test]
    fn rejects_invalid_runtime_inputs() {
        let base = ServiceConfig::new("svc".into(), vec!["/bin/true".into()]);
        let mut c = base.clone();
        c.restart_delay_ms = 99;
        assert!(c.validate().is_err());
        let mut c = base.clone();
        c.stop_timeout_ms = 30001;
        assert!(c.validate().is_err());
        let mut c = base.clone();
        c.log_max_bytes = 4095;
        assert!(c.validate().is_err());
        let mut c = base.clone();
        c.environment.insert("A=B".into(), "x".into());
        assert!(c.validate().is_err());
        let mut c = base.clone();
        c.working_dir = Some("relative".into());
        assert!(c.validate().is_err());
        let mut c = base;
        c.command.push("x\0y".into());
        assert!(c.validate().is_err());
    }
    #[test]
    fn defaults_and_unknown_fields() {
        let c: ServiceConfig =
            serde_json::from_str(r#"{"name":"a","command":["/bin/true"]}"#).unwrap();
        assert!(c.enabled);
        assert_eq!(c.log_max_bytes, 262144);
        assert!(
            serde_json::from_str::<ServiceConfig>(
                r#"{"name":"a","command":["/bin/true"],"oops":1}"#
            )
            .is_err()
        );
    }
    #[test]
    fn store_round_trip_and_bad_config_isolation() {
        let root = std::env::temp_dir().join(format!("yactl-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let s = ConfigStore::new(&root).unwrap();
        let c = ServiceConfig::new("good".into(), vec!["/bin/true".into()]);
        s.save(&c).unwrap();
        assert_eq!(s.load("good").unwrap().command, c.command);
        std::fs::write(root.join("services/bad.json"), b"{}").unwrap();
        let all = s.all().unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|(n, v)| n == "bad" && v.is_err()));
        std::os::unix::fs::symlink("good.json", root.join("services/link.json")).unwrap();
        assert!(s.load("link").is_err());
        s.remove("good").unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
