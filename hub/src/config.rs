//! Configuration: the v1 hub's `HUB_*` environment variables, unchanged, plus
//! the database the v2 hub keeps its records in.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::wire::{py_int, py_strip};

pub const DEFAULT_PORT: u16 = 7370;
/// The API-only public listener (FR-10). Fixed in v1; `HUB_PUBLIC_PORT`
/// may move it, for hosts and tests that run several hubs.
pub const DEFAULT_PUBLIC_PORT: u16 = 7371;
pub const DEFAULT_MAX_FILE_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Clone)]
pub struct Config {
    pub hub_name: String,
    pub port: u16,
    pub bind: String,
    pub public: bool,
    pub public_bind: String,
    pub public_port: u16,
    /// `HUB_PUBLIC_ADVERTISE`: where clients reach the door when that is not
    /// where it listens (Docker's port mapping, a tunnel); /healthz shows it
    /// beside the listener's own address (v2.0.2)
    pub public_advertise: Option<String>,
    pub data_dir: PathBuf,
    /// days mail and files are kept (`HUB_RETENTION_DAYS`); None, the v2
    /// default, keeps them until their owners delete them (G4)
    pub retention_days: Option<i64>,
    /// days a silent address stays on the roster (`HUB_ORG_RETENTION_DAYS`);
    /// None, the v2 default, keeps it until it unregisters or the operator
    /// removes it (G9)
    pub org_retention_days: Option<i64>,
    /// the startup default for one attachment upload (`HUB_MAX_FILE_BYTES`)
    pub max_file_bytes: u64,
    /// the embedding host's live override (`HUB_RUNTIME_CONFIG_FILE`)
    pub runtime_config_file: Option<PathBuf>,
    /// PostgreSQL connection string (URL or key=value). May hold a
    /// password: never logged, never serialized.
    database_url: String,
    /// `HUB_DATABASE_PASSWORD`: the password kept out of the URL (no
    /// percent-encoding, and a host can pass it apart from the address)
    database_password: Option<String>,
    pub pool_size: usize,
    pub verbose: bool,
    /// import `<data>/hub.sqlite3` on startup when the database is empty
    pub import_sqlite: bool,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("hub_name", &self.hub_name)
            .field("port", &self.port)
            .field("bind", &self.bind)
            .field("public", &self.public)
            .field("public_bind", &self.public_bind)
            .field("public_port", &self.public_port)
            .field("public_advertise", &self.public_advertise)
            .field("data_dir", &self.data_dir)
            .field("retention_days", &self.retention_days)
            .field("org_retention_days", &self.org_retention_days)
            .field("max_file_bytes", &self.max_file_bytes)
            .field("runtime_config_file", &self.runtime_config_file)
            .field("database_url", &"*****")
            .field("pool_size", &self.pool_size)
            .field("verbose", &self.verbose)
            .field("import_sqlite", &self.import_sqlite)
            .finish()
    }
}

impl Config {
    pub fn from_env() -> anyhow::Result<Config> {
        Self::from_vars(&std::env::vars().collect())
    }

    /// The same rules as v1's module-level reads (`int(...)`, `.strip()`,
    /// `or default`), applied to an explicit variable map.
    pub fn from_vars(vars: &HashMap<String, String>) -> anyhow::Result<Config> {
        let get = |k: &str| vars.get(k).map(String::as_str);
        let int = |k: &str, default: i128| -> anyhow::Result<i128> {
            match get(k) {
                None => Ok(default),
                Some(v) => py_int(v).ok_or_else(|| anyhow::anyhow!("{k} must be an integer, got {v:?}")),
            }
        };
        let stripped_or = |k: &str, default: &str| -> String {
            let v = py_strip(get(k).unwrap_or("")).to_string();
            if v.is_empty() {
                default.to_string()
            } else {
                v
            }
        };
        let port = int("HUB_PORT", DEFAULT_PORT as i128)?;
        let port = u16::try_from(port).map_err(|_| anyhow::anyhow!("HUB_PORT must be a port number, got {port}"))?;
        let public_port = int("HUB_PUBLIC_PORT", DEFAULT_PUBLIC_PORT as i128)?;
        let public_port =
            u16::try_from(public_port).map_err(|_| anyhow::anyhow!("HUB_PUBLIC_PORT must be a port number, got {public_port}"))?;
        let max_file_bytes = int("HUB_MAX_FILE_BYTES", DEFAULT_MAX_FILE_BYTES as i128)?;
        if max_file_bytes <= 0 {
            anyhow::bail!("HUB_MAX_FILE_BYTES must be a positive integer");
        }
        let clamp_days = |d: i128| d.clamp(-3_000_000, 3_000_000) as i64;
        let data_dir = match get("HUB_DATA") {
            None => PathBuf::from("/data"),
            Some("") => anyhow::bail!("HUB_DATA is set but empty"),
            Some(d) => PathBuf::from(d),
        };
        let database_url = get("HUB_DATABASE_URL").map(str::trim).unwrap_or("").to_string();
        if database_url.is_empty() {
            anyhow::bail!("HUB_DATABASE_URL is required (a PostgreSQL connection string)");
        }
        let pool_size = int("HUB_DB_POOL", 32)?.clamp(2, 1024) as usize;
        let flag = |k: &str, default: bool| match get(k).map(|v| py_strip(v).to_ascii_lowercase()) {
            None => default,
            Some(v) if v.is_empty() => default,
            Some(v) => !matches!(v.as_str(), "0" | "false" | "no" | "off"),
        };
        Ok(Config {
            hub_name: {
                let n = py_strip(get("HUB_NAME").unwrap_or("")).to_string();
                if n.is_empty() {
                    hostname()
                } else {
                    n
                }
            },
            port,
            bind: stripped_or("HUB_BIND", "0.0.0.0"),
            public: !py_strip(get("HUB_PUBLIC").unwrap_or("")).is_empty(),
            public_bind: stripped_or("HUB_PUBLIC_BIND", "0.0.0.0"),
            public_port,
            public_advertise: {
                let a = py_strip(get("HUB_PUBLIC_ADVERTISE").unwrap_or(""));
                if a.len() > 255 || !a.bytes().all(|b| b.is_ascii_graphic()) {
                    anyhow::bail!("HUB_PUBLIC_ADVERTISE must be one address clients can use, such as 100.64.1.2:7378 or https://hub.example.com (no spaces, at most 255 characters)");
                }
                (!a.is_empty()).then(|| a.to_string())
            },
            data_dir,
            retention_days: match get("HUB_RETENTION_DAYS").map(py_strip) {
                None | Some("") => None,
                Some(_) => Some(clamp_days(int("HUB_RETENTION_DAYS", 0)?)),
            },
            org_retention_days: match get("HUB_ORG_RETENTION_DAYS").map(py_strip) {
                None | Some("") => None,
                Some(_) => Some(clamp_days(int("HUB_ORG_RETENTION_DAYS", 0)?)),
            },
            max_file_bytes: u64::try_from(max_file_bytes).unwrap_or(u64::MAX),
            runtime_config_file: get("HUB_RUNTIME_CONFIG_FILE").filter(|v| !v.is_empty()).map(PathBuf::from),
            database_url,
            database_password: get("HUB_DATABASE_PASSWORD").filter(|p| !p.is_empty()).map(str::to_string),
            pool_size,
            verbose: flag("HUB_LOG_VERBOSE", false),
            import_sqlite: flag("HUB_IMPORT_SQLITE", true),
        })
    }

    #[doc(hidden)]
    pub fn database_url(&self) -> &str {
        &self.database_url
    }

    #[doc(hidden)]
    pub fn database_password(&self) -> Option<&str> {
        self.database_password.as_deref()
    }

    pub fn blob_dir(&self) -> PathBuf {
        self.data_dir.join("blobs")
    }

    /// Resumable uploads in progress (kept across restarts, unlike the
    /// partial files of whole-file uploads in `blobs/`).
    pub fn uploads_dir(&self) -> PathBuf {
        self.data_dir.join("uploads")
    }

    pub fn sqlite_path(&self) -> PathBuf {
        self.data_dir.join("hub.sqlite3")
    }
}

/// `socket.gethostname()`: the hub's name when `HUB_NAME` is unset.
pub fn hostname() -> String {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{ComputerNamePhysicalDnsHostname, GetComputerNameExW};
        let mut len: u32 = 0;
        // first call reports the size needed
        unsafe { GetComputerNameExW(ComputerNamePhysicalDnsHostname, std::ptr::null_mut(), &mut len) };
        if len > 0 {
            let mut buf = vec![0u16; len as usize];
            if unsafe { GetComputerNameExW(ComputerNamePhysicalDnsHostname, buf.as_mut_ptr(), &mut len) } != 0 {
                return String::from_utf16_lossy(&buf[..len as usize]);
            }
        }
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "localhost".into())
    }
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        if rc == 0 {
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            return String::from_utf8_lossy(&buf[..end]).into_owned();
        }
        "localhost".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        let mut m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        m.entry("HUB_DATABASE_URL".into()).or_insert_with(|| "postgres://x@localhost/y".into());
        m
    }

    #[test]
    fn defaults() {
        let c = Config::from_vars(&vars(&[])).unwrap();
        assert_eq!(c.port, 7370);
        assert_eq!(c.bind, "0.0.0.0");
        assert!(!c.public);
        assert_eq!(c.public_port, 7371);
        assert_eq!(c.data_dir, PathBuf::from("/data"));
        assert_eq!(c.retention_days, None, "v2 keeps mail until it is deleted (G4)");
        assert_eq!(c.org_retention_days, None, "v2 lists an address until it leaves (G9)");
        assert_eq!(c.max_file_bytes, 1024 * 1024 * 1024);
        assert!(c.runtime_config_file.is_none());
        assert!(!c.hub_name.is_empty());
    }

    #[test]
    fn overrides_and_refusals() {
        let c = Config::from_vars(&vars(&[("HUB_MAX_FILE_BYTES", "12345"), ("HUB_NAME", "  office "), ("HUB_PUBLIC", " 1 "), ("HUB_BIND", " ")]))
            .unwrap();
        assert_eq!(c.max_file_bytes, 12345);
        assert_eq!(c.hub_name, "office");
        assert!(c.public);
        assert_eq!(c.bind, "0.0.0.0");
        assert_eq!(c.public_advertise, None);
        let a = Config::from_vars(&vars(&[("HUB_PUBLIC_ADVERTISE", " 100.64.1.2:7378 ")])).unwrap();
        assert_eq!(a.public_advertise.as_deref(), Some("100.64.1.2:7378"));
        assert_eq!(Config::from_vars(&vars(&[("HUB_PUBLIC_ADVERTISE", "  ")])).unwrap().public_advertise, None);
        assert!(Config::from_vars(&vars(&[("HUB_PUBLIC_ADVERTISE", "two words")])).is_err());
        assert!(Config::from_vars(&vars(&[("HUB_PUBLIC_ADVERTISE", &"h".repeat(256))])).is_err());
        assert!(Config::from_vars(&vars(&[("HUB_MAX_FILE_BYTES", "0")])).is_err());
        assert!(Config::from_vars(&vars(&[("HUB_MAX_FILE_BYTES", "-1")])).is_err());
        assert!(Config::from_vars(&vars(&[("HUB_RETENTION_DAYS", "abc")])).is_err());
        assert_eq!(Config::from_vars(&vars(&[("HUB_RETENTION_DAYS", " 30 ")])).unwrap().retention_days, Some(30));
        assert_eq!(Config::from_vars(&vars(&[("HUB_RETENTION_DAYS", "  ")])).unwrap().retention_days, None);
        let mut no_db = vars(&[]);
        no_db.remove("HUB_DATABASE_URL");
        assert!(Config::from_vars(&no_db).is_err());
        assert!(!format!("{:?}", Config::from_vars(&vars(&[("HUB_DATABASE_URL", "postgres://u:hunter2@h/d")])).unwrap())
            .contains("hunter2"));
    }
}
