//! The config file (design.md, Configuration).

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// What apps reach the server at; informational (URLs in responses are relative).
    #[serde(default)]
    pub public_url: Option<String>,
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Index, preview cache, secret. Put it on an SSD.
    #[serde(default = "default_data")]
    pub data: PathBuf,
    #[serde(default)]
    pub tls: Option<Tls>,
    #[serde(default, rename = "library")]
    pub libraries: Vec<LibraryConfig>,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub decode: DecodeConfig,
    #[serde(default)]
    pub log: LogConfig,
    #[serde(default)]
    pub background: BackgroundConfig,
}

/// Background work: reading files, making previews, hashing.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackgroundConfig {
    /// Requests come first: background work pauses while the API is in use and resumes once it has had no
    /// request for this many seconds. 0: never pause.
    #[serde(default = "default_yield_seconds")]
    pub yield_seconds: u64,
}

impl Default for BackgroundConfig {
    fn default() -> Self {
        BackgroundConfig {
            yield_seconds: default_yield_seconds(),
        }
    }
}

fn default_yield_seconds() -> u64 {
    10
}

/// Logging and the audit trail.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    /// Also write the log to `<data>/logs/wirephoto.log.<date>`, a file a day (it survives a recreated
    /// container, which the container's own log does not).
    #[serde(default = "yes")]
    pub file: bool,
    /// Days of log files kept.
    #[serde(default = "default_keep_days")]
    pub keep_days: usize,
    /// One line per API request: method, path, status, time, size, key, client. With `file`, in a file of its
    /// own, `<data>/logs/access.log.<date>` (and not in the other logs); without, in the console log.
    #[serde(default = "yes")]
    pub access: bool,
    /// Days the audit trail (changes, refused requests, key changes; `wirephoto audit`) is kept in the index.
    #[serde(default = "default_audit_days")]
    pub audit_days: u32,
}

impl Default for LogConfig {
    fn default() -> Self {
        LogConfig {
            file: true,
            keep_days: default_keep_days(),
            access: true,
            audit_days: default_audit_days(),
        }
    }
}

fn default_keep_days() -> usize {
    14
}

fn default_audit_days() -> u32 {
    365
}

/// The decode workers (design.md, Architecture).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecodeConfig {
    /// Previews rendered at once; 0: one per CPU.
    #[serde(default)]
    pub workers: usize,
    /// ffmpeg for video posters (ffprobe is expected next to it). Default: found on `PATH`; without it videos
    /// have no posters or durations.
    #[serde(default)]
    pub ffmpeg: Option<PathBuf>,
    /// The `wirephoto` binary the workers run (`<program> decode`). Default: the running one.
    #[serde(default)]
    pub program: Option<PathBuf>,
    /// Decoding on the GPU: `auto` uses a VA-API GPU (Intel Quick Sync, AMD) on Linux when found working at
    /// start; `on` also VideoToolbox on macOS (for development: there the CPU path is faster); `off` never.
    #[serde(default)]
    pub gpu: GpuMode,
    /// The GPU's render node (VA-API). Default: /dev/dri/renderD128.
    #[serde(default)]
    pub gpu_device: Option<String>,
    /// HEIC decoding: `auto` through ffmpeg (on the GPU when it takes the file, else on the CPU, several times
    /// faster than libheif), `libheif` only through libheif. libheif decodes whatever ffmpeg cannot either way.
    #[serde(default)]
    pub heic: HeicMode,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GpuMode {
    #[default]
    Auto,
    On,
    Off,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HeicMode {
    #[default]
    Auto,
    Libheif,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    pub cert: PathBuf,
    pub key: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryConfig {
    /// Short name; the library's API ID is `lib_<id>`.
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub read_only: bool,
    /// Days entries stay in the trash; 0 makes deletes permanent.
    #[serde(default = "default_trash_days")]
    pub trash_days: u32,
    #[serde(default)]
    pub watch: WatchMode,
    #[serde(default = "default_poll_seconds")]
    pub poll_seconds: u64,
    /// Names skipped while indexing (exact names, or `*` prefix/suffix wildcards; `.*` skips dotfiles).
    #[serde(default = "default_ignore")]
    pub ignore: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WatchMode {
    #[default]
    Auto,
    Poll,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    /// Preview cache budget on disk, all sizes together, e.g. "50GB".
    #[serde(default = "default_cache_size", deserialize_with = "de_size")]
    pub max_size: u64,
    /// Preview cache in memory, in front of the disk cache, e.g. "256MB".
    #[serde(default = "default_memory_cache", deserialize_with = "de_size")]
    pub memory: u64,
    /// Pre-generate the 640 px preview of every photo.
    #[serde(default = "yes")]
    pub pregenerate: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_size: default_cache_size(),
            memory: default_memory_cache(),
            pregenerate: true,
        }
    }
}

fn default_memory_cache() -> u64 {
    256 << 20
}
fn default_listen() -> String {
    "0.0.0.0:8443".into()
}
fn default_data() -> PathBuf {
    "/data".into()
}
fn default_trash_days() -> u32 {
    30
}
fn default_poll_seconds() -> u64 {
    60
}
fn default_cache_size() -> u64 {
    50 << 30
}
fn yes() -> bool {
    true
}
fn default_ignore() -> Vec<String> {
    // Dotfiles (and with them wirephoto's own trash and temp files), Synology's thumbnail and recycle folders,
    // Windows and macOS clutter.
    [
        ".*",
        "@eaDir",
        "#recycle",
        "#snapshot",
        "Thumbs.db",
        "desktop.ini",
    ]
    .map(String::from)
    .to_vec()
}

fn de_size<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        N(u64),
        S(String),
    }
    match Raw::deserialize(d)? {
        Raw::N(n) => Ok(n),
        Raw::S(s) => parse_size(&s).map_err(serde::de::Error::custom),
    }
}

/// "50GB", "512 MB", "1TB", "1048576" -> bytes (binary units).
pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().map_err(|_| format!("not a size: {s:?}"))?;
    let mul: u64 = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" | "KIB" => 1 << 10,
        "M" | "MB" | "MIB" => 1 << 20,
        "G" | "GB" | "GIB" => 1 << 30,
        "T" | "TB" | "TIB" => 1 << 40,
        u => return Err(format!("unknown size unit {u:?}")),
    };
    Ok((n * mul as f64) as u64)
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let mut seen = std::collections::HashSet::new();
        for lib in &self.libraries {
            if lib.id.is_empty()
                || !lib
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                bail!("library id {:?}: use letters, digits, '-' and '_'", lib.id);
            }
            if !seen.insert(&lib.id) {
                bail!("library id {:?} is used twice", lib.id);
            }
        }
        Ok(())
    }
}

impl LibraryConfig {
    pub fn api_id(&self) -> String {
        format!("lib_{}", self.id)
    }

    pub fn ignores(&self, name: &str) -> bool {
        self.ignore
            .iter()
            .any(|p| match (p.strip_prefix('*'), p.strip_suffix('*')) {
                _ if p == name => true,
                (Some(suffix), _) if !suffix.is_empty() && !suffix.ends_with('*') => {
                    name.ends_with(suffix)
                }
                (_, Some(prefix)) if !prefix.is_empty() => name.starts_with(prefix),
                _ => false,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("50GB").unwrap(), 50 << 30);
        assert_eq!(parse_size("512 MB").unwrap(), 512 << 20);
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert!(parse_size("5 parsecs").is_err());
    }

    #[test]
    fn cache_sizes() {
        let c: CacheConfig = toml::from_str("max_size = '20GB'\nmemory = '1GB'").unwrap();
        assert_eq!((c.max_size, c.memory), (20 << 30, 1 << 30));
        let c: CacheConfig = toml::from_str("").unwrap();
        assert_eq!(c.memory, 256 << 20);
    }

    #[test]
    fn ignore_patterns() {
        let lib: LibraryConfig = toml::from_str("id='a'\nname='A'\npath='/x'").unwrap();
        assert!(lib.ignores(".DS_Store"));
        assert!(lib.ignores("@eaDir"));
        assert!(!lib.ignores("DSC01234.ARW"));
        let lib: LibraryConfig =
            toml::from_str("id='a'\nname='A'\npath='/x'\nignore=['*.tmp']").unwrap();
        assert!(lib.ignores("x.tmp"));
        assert!(!lib.ignores(".DS_Store"));
    }
}
