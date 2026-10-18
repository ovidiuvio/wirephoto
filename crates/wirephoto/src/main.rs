use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use wirephoto::auth::{Keys, normalize_origin};
use wirephoto::config::Config;
use wirephoto_api::Scope;

#[derive(Parser)]
#[command(
    name = "wirephoto",
    version,
    about = "Serves the photos on a machine's own disks to apps over HTTP"
)]
struct Cli {
    /// The config file.
    #[arg(
        long,
        short,
        env = "WIREPHOTO_CONFIG",
        default_value = "/etc/wirephoto.toml",
        global = true
    )]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Runs the server.
    Serve,
    /// Exits 0 when the server accepts connections (for container health checks).
    Health,
    /// A decode worker: started by `serve`, not by hand.
    #[command(hide = true)]
    Decode,
    /// Manages API keys.
    Keys {
        #[command(subcommand)]
        command: KeysCommand,
    },
    /// Looks inside the preview cache, for debugging. Works while the server runs (reads a copy).
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
}

#[derive(Subcommand)]
enum CacheCommand {
    /// Counts and bytes per preview size, and how many are stale or orphaned.
    Stats,
    /// One line per cached preview: size, bytes, last used, state, path.
    List {
        #[command(flatten)]
        filter: CacheFilter,
    },
    /// Writes the cached previews as JPEG files under DIR, mirroring the libraries' folders
    /// (`<library>/<path>.<size>.jpg`, under the first path for copies sharing one; stale ones end
    /// `.stale.jpg`, orphans go to `_orphans/`).
    Extract {
        dir: PathBuf,
        #[command(flatten)]
        filter: CacheFilter,
    },
}

#[derive(clap::Args)]
struct CacheFilter {
    /// Only this entry (ent_…).
    #[arg(long)]
    entry: Option<String>,
    /// Only this preview size (160, 320, 480, 640, 1280, 2560).
    #[arg(long)]
    size: Option<u32>,
    /// Only previews whose file is under this path in its library (e.g. /2026/Lisbon).
    #[arg(long)]
    path: Option<String>,
}

#[derive(Subcommand)]
enum KeysCommand {
    /// Creates a key and prints it (once).
    Create {
        /// A name to recognise the key by (e.g. "lith-laptop").
        #[arg(long)]
        name: String,
        /// read, or write (includes deleting).
        #[arg(long, default_value = "read")]
        scope: String,
        /// The browser origin allowed to use the key (e.g. https://lith.example.com). Without it the key is
        /// for scripts only.
        #[arg(long)]
        origin: Option<String>,
        /// Limit the key to a library (by its config id); repeatable. Default: all libraries.
        #[arg(long = "library")]
        libraries: Vec<String>,
    },
    /// Lists keys.
    List,
    /// Revokes a key by name or ID.
    Revoke { name: String },
}

fn open_db(config: &Config) -> anyhow::Result<wirephoto::db::Db> {
    std::fs::create_dir_all(&config.data)?;
    wirephoto::db::Db::open(&config.data.join("index.db"))
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if let Command::Decode = cli.command {
        return wirephoto::decode::worker_main();
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("WIREPHOTO_LOG")
                .unwrap_or_else(|_| "wirephoto=info,tower_http=warn".into()),
        )
        .init();
    let config = Config::load(&cli.config)?;
    match cli.command {
        Command::Serve => tokio::runtime::Runtime::new()?.block_on(serve(config)),
        Command::Keys { command } => keys(config, command),
        Command::Health => health(&config),
        Command::Cache { command } => cache(config, command),
        Command::Decode => unreachable!(),
    }
}

async fn serve(config: Config) -> anyhow::Result<()> {
    let listen: std::net::SocketAddr = config
        .listen
        .parse()
        .with_context(|| format!("listen = {:?}", config.listen))?;
    let tls = config.tls.clone();
    let (app, router) = wirephoto::build(config)?;
    wirephoto::start(&app);
    let service = router.into_make_service();
    match tls {
        Some(tls) => {
            let rustls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&tls.cert, &tls.key)
                .await
                .with_context(|| {
                    format!("loading {} / {}", tls.cert.display(), tls.key.display())
                })?;
            tracing::info!(%listen, "serving HTTPS (HTTP/2 and HTTP/1.1)");
            axum_server::bind_rustls(listen, rustls)
                .serve(service)
                .await?;
        }
        None => {
            tracing::info!(%listen, "serving plain HTTP: put a TLS-terminating proxy in front for browser apps");
            axum_server::bind(listen).serve(service).await?;
        }
    }
    Ok(())
}

fn health(config: &Config) -> anyhow::Result<()> {
    let addr: std::net::SocketAddr = config.listen.parse()?;
    // A wildcard listen address is reached on loopback.
    let addr = if addr.ip().is_unspecified() {
        std::net::SocketAddr::new([127, 0, 0, 1].into(), addr.port())
    } else {
        addr
    };
    std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(3))
        .with_context(|| format!("nothing listening on {addr}"))?;
    Ok(())
}

fn keys(config: Config, command: KeysCommand) -> anyhow::Result<()> {
    let db = open_db(&config)?;
    match command {
        KeysCommand::Create {
            name,
            scope,
            origin,
            libraries,
        } => {
            let scope = match scope.as_str() {
                "read" => Scope::Read,
                "write" => Scope::Write,
                s => anyhow::bail!("scope is read or write, not {s:?}"),
            };
            let origin = match origin {
                Some(o) => Some(normalize_origin(&o).with_context(|| {
                    format!("not an origin: {o:?} (like https://lith.example.com)")
                })?),
                None => None,
            };
            for l in &libraries {
                if !config.libraries.iter().any(|c| c.id == *l) {
                    anyhow::bail!("no library {l:?} in the config");
                }
            }
            let libs = (!libraries.is_empty())
                .then(|| libraries.iter().map(|l| format!("lib_{l}")).collect());
            let (k, token) = Keys::create(&db, &name, scope, libs, origin)?;
            println!("{token}");
            eprintln!(
                "Created key {:?} ({}). This is the only time it is shown.",
                k.name, k.id
            );
        }
        KeysCommand::List => {
            println!(
                "{:<20} {:<6} {:<32} {:<16} {:<20} LAST USED",
                "NAME", "SCOPE", "ORIGIN", "LIBRARIES", "CREATED"
            );
            let fmt = |t: i64| {
                chrono::DateTime::from_timestamp(t, 0)
                    .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_default()
            };
            for k in Keys::list(&db)? {
                println!(
                    "{:<20} {:<6} {:<32} {:<16} {:<20} {}",
                    k.name,
                    if k.scope == Scope::Write {
                        "write"
                    } else {
                        "read"
                    },
                    k.origin.as_deref().unwrap_or("(scripts)"),
                    k.libraries
                        .map(|l| l.join(","))
                        .unwrap_or_else(|| "all".into()),
                    fmt(k.created),
                    k.last_used.map(fmt).unwrap_or_else(|| "never".into()),
                );
            }
        }
        KeysCommand::Revoke { name } => {
            if Keys::revoke(&db, &name)? {
                eprintln!("Revoked {name:?}. Its signed URLs stop working too.");
            } else {
                anyhow::bail!("no key named {name:?}");
            }
        }
    }
    Ok(())
}

/// A cached preview and what the index says about it now.
struct CacheItem {
    c: wirephoto::preview::inspect::Cached,
    /// (library, path) of a file it is for, if any is still indexed.
    file: Option<(String, String)>,
    /// Entries it serves (several for copies sharing a content-keyed preview).
    entries: Vec<String>,
    /// An entry-keyed preview the file no longer uses: it changed since, or its previews are content-keyed now.
    stale: bool,
}

impl CacheItem {
    fn state(&self) -> &'static str {
        match (&self.file, self.stale) {
            (None, _) => "orphan",
            (Some(_), true) => "stale",
            _ => "current",
        }
    }
}

fn cache(config: Config, command: CacheCommand) -> anyhow::Result<()> {
    use wirephoto::preview::KeyOf;
    use wirephoto::preview::inspect::CacheReader;
    let reader = CacheReader::open(&config.data.join("previews.redb"))?;
    if reader.snapshot() {
        eprintln!(
            "(the server has the cache open: reading a copy, as of its last flush up to 30 s ago)"
        );
    }
    let db = open_db(&config)?;
    let filter = match &command {
        CacheCommand::Stats => None,
        CacheCommand::List { filter } | CacheCommand::Extract { filter, .. } => Some(filter),
    };
    let mut items = Vec::new();
    for c in reader.list()? {
        if filter.is_some_and(|f| f.size.is_some_and(|s| s != c.size)) {
            continue;
        }
        let (rows, stale) = match &c.of {
            KeyOf::Entry(id, etag, _) => {
                let row = db.read(|conn| Ok(wirephoto::db::entry_by_id(conn, id)?))?;
                let stale = row
                    .as_ref()
                    .is_some_and(|r| r.etag() != *etag || r.sha256.is_some());
                (row.into_iter().collect::<Vec<_>>(), stale)
            }
            KeyOf::Content(hash, _) => {
                let rows = db.read(|conn| {
                    let mut st = conn.prepare(&format!(
                        "SELECT {} FROM {} WHERE e.sha256 = ? ORDER BY e.path",
                        wirephoto::db::ENTRY_COLS,
                        wirephoto::db::ENTRY_FROM
                    ))?;
                    let rows = st
                        .query_map([&hash[..]], wirephoto::db::EntryRow::from_row)?
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(rows)
                })?;
                (rows, false)
            }
        };
        let entries: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
        if let Some(e) = filter.and_then(|f| f.entry.as_ref())
            && !entries.contains(e)
        {
            continue;
        }
        let mut files: Vec<(String, String)> =
            rows.into_iter().map(|r| (r.library, r.path)).collect();
        if let Some(prefix) = filter.and_then(|f| f.path.as_deref()) {
            // Stored paths have no leading slash.
            let prefix = prefix.trim_matches('/');
            files.retain(|(_, p)| {
                prefix.is_empty() || p == prefix || p.starts_with(&format!("{prefix}/"))
            });
            if files.is_empty() {
                continue;
            }
        }
        let file = files.into_iter().next();
        items.push(CacheItem {
            c,
            file,
            entries,
            stale,
        });
    }
    items.sort_by(|a, b| (&a.file, a.c.size).cmp(&(&b.file, b.c.size)));

    match command {
        CacheCommand::Stats => {
            let mut sizes: std::collections::BTreeMap<u32, (u64, u64)> = Default::default();
            for i in &items {
                let s = sizes.entry(i.c.size).or_default();
                s.0 += 1;
                s.1 += i.c.bytes;
            }
            println!(
                "{:>6}  {:>8}  {:>10}  {:>8}",
                "size", "previews", "bytes", "average"
            );
            for (size, (n, bytes)) in &sizes {
                println!(
                    "{size:>6}  {n:>8}  {:>10}  {:>8}",
                    human(*bytes),
                    human(bytes / n.max(&1))
                );
            }
            let total: u64 = items.iter().map(|i| i.c.bytes).sum();
            println!("{:>6}  {:>8}  {:>10}", "all", items.len(), human(total));
            let stale = items.iter().filter(|i| i.state() == "stale").count();
            let orphans = items.iter().filter(|i| i.state() == "orphan").count();
            let shared = items.iter().filter(|i| i.entries.len() > 1).count();
            println!(
                "\nshared by copies: {shared}   stale (superseded): {stale}   orphans (no file): {orphans}"
            );
            println!("budget: {} (cache.max_size)", human(config.cache.max_size));
        }
        CacheCommand::List { .. } => {
            for i in &items {
                let used = chrono::DateTime::from_timestamp(i.c.last_used as i64, 0)
                    .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_default();
                let what = match &i.file {
                    Some((lib, path)) if i.entries.len() > 1 => {
                        format!("{lib}:{path} (shared by {} copies)", i.entries.len())
                    }
                    Some((lib, path)) => format!("{lib}:{path}"),
                    None => i.c.key.clone(),
                };
                println!(
                    "{:>5}  {:>8}  {used}  {:<7}  {what}",
                    i.c.size,
                    human(i.c.bytes),
                    i.state()
                );
            }
            eprintln!("{} previews", items.len());
        }
        CacheCommand::Extract { dir, .. } => {
            let mut written = 0usize;
            for i in &items {
                let Some(jpeg) = reader.get(&i.c.key)? else {
                    continue;
                };
                let rel = match &i.file {
                    Some((lib, path)) if !i.stale => format!("{lib}/{path}.{}.jpg", i.c.size),
                    Some((lib, path)) => format!("{lib}/{path}.{}.stale.jpg", i.c.size),
                    None => format!("_orphans/{}.jpg", i.c.key.replace(':', ".")),
                };
                // Paths come from the index: keep them below DIR whatever they contain.
                let rel: PathBuf = std::path::Path::new(&rel)
                    .components()
                    .filter(|c| matches!(c, std::path::Component::Normal(_)))
                    .collect();
                let out = dir.join(rel);
                if let Some(parent) = out.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&out, jpeg).with_context(|| format!("writing {}", out.display()))?;
                written += 1;
            }
            println!("wrote {written} previews to {}", dir.display());
        }
    }
    Ok(())
}

fn human(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}
