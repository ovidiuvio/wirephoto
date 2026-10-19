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
    /// Pauses background work (reading files, making previews, hashing) until `resume`, also across
    /// restarts. Requests and changes on disk are still served and indexed.
    Pause,
    /// Resumes background work paused with `pause`.
    Resume,
    /// What the running server is doing, in detail: its processes' CPU and memory, storage, each library's
    /// contents and background work, requests by kind, previews by size and how long they take, caches, the
    /// GPU, keys, uploads, recent warnings. Run it where the server runs (`docker exec wirephoto wirephoto
    /// status`)
    Status {
        /// The same as JSON, for scripts.
        #[arg(long)]
        json: bool,
    },
    /// Lists the audit trail: changes made through the API, refused requests, key changes, what the server
    /// deleted itself. Newest last.
    Audit {
        /// How far back: `30m`, `24h`, `7d` (default 24h).
        #[arg(long, default_value = "24h")]
        since: String,
        /// Only this key (its name).
        #[arg(long)]
        key: Option<String>,
        /// Only this operation (upload, create-folder, copy, move, delete, restore, denied, key-create,
        /// key-revoke, purge).
        #[arg(long)]
        op: Option<String>,
        /// At most this many (the newest).
        #[arg(long, default_value_t = 200)]
        limit: usize,
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
    let config = Config::load(&cli.config)?;
    // Kept to the end of `main`: dropping it writes out what the log file has buffered.
    let _log = init_logging(&config, matches!(cli.command, Command::Serve));
    match cli.command {
        Command::Serve => tokio::runtime::Runtime::new()?.block_on(serve(config)),
        Command::Keys { command } => keys(config, command),
        Command::Health => health(&config),
        Command::Cache { command } => cache(config, command),
        Command::Status { json } => status(&config, json),
        Command::Pause | Command::Resume => {
            let pause = matches!(cli.command, Command::Pause);
            let db = open_db(&config)?;
            wirephoto::indexer::set_paused(&db, pause)?;
            wirephoto::audit::record_in(
                &db,
                wirephoto::audit::Event {
                    client: Some("cli".into()),
                    ..wirephoto::audit::Event::new(
                        if pause { "pause" } else { "resume" },
                        if pause {
                            "background work paused"
                        } else {
                            "background work resumed"
                        },
                    )
                },
            );
            println!(
                "{} (a running server notices within 5 seconds).",
                if pause {
                    "Paused reading files, previews and hashing"
                } else {
                    "Resumed"
                }
            );
            Ok(())
        }
        Command::Audit {
            since,
            key,
            op,
            limit,
        } => audit(&config, &since, key.as_deref(), op.as_deref(), limit),
        Command::Decode => unreachable!(),
    }
}

async fn serve(config: Config) -> anyhow::Result<()> {
    let listen: std::net::SocketAddr = config
        .listen
        .parse()
        .with_context(|| format!("listen = {:?}", config.listen))?;
    let tls = config.tls.clone();
    tracing::info!("wirephoto {} starting", wirephoto::version());
    let (app, router) = wirephoto::build(config)?;
    wirephoto::start(&app);
    // With the client's address, for the access log and the audit trail.
    let service = router.into_make_service_with_connect_info::<std::net::SocketAddr>();
    // `docker stop` (SIGTERM) or Ctrl-C: stop taking requests, let the ones in flight finish, then make the
    // preview cache durable so nothing made is lost and the next start opens it at once.
    let handle = axum_server::Handle::new();
    {
        let handle = handle.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            tracing::info!("stopping");
            handle.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
        });
    }
    match tls {
        Some(tls) => {
            let rustls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&tls.cert, &tls.key)
                .await
                .with_context(|| {
                    format!("loading {} / {}", tls.cert.display(), tls.key.display())
                })?;
            tracing::info!(%listen, "serving HTTPS (HTTP/2 and HTTP/1.1)");
            axum_server::bind_rustls(listen, rustls)
                .handle(handle)
                .serve(service)
                .await?;
        }
        None => {
            tracing::info!(%listen, "serving plain HTTP: put a TLS-terminating proxy in front for browser apps");
            axum_server::bind(listen)
                .handle(handle)
                .serve(service)
                .await?;
        }
    }
    let disk = app.previews.disk().clone();
    tokio::task::spawn_blocking(move || disk.flush()).await??;
    tracing::info!("stopped; preview cache saved");
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
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
            wirephoto::audit::record_in(
                &db,
                wirephoto::audit::Event {
                    client: Some("cli".into()),
                    ..wirephoto::audit::Event::new(
                        "key-create",
                        format!(
                            "created key {} ({}, {})",
                            k.name,
                            if k.scope == Scope::Write {
                                "write"
                            } else {
                                "read"
                            },
                            k.origin.as_deref().unwrap_or("scripts only")
                        ),
                    )
                },
            );
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
                wirephoto::audit::record_in(
                    &db,
                    wirephoto::audit::Event {
                        client: Some("cli".into()),
                        ..wirephoto::audit::Event::new("key-revoke", format!("revoked key {name}"))
                    },
                );
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
        b if b >= 1 << 40 => format!("{:.1} TB", b as f64 / (1u64 << 40) as f64),
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

/// The log: the console (`docker logs`) and, for the server, also a file a day in `<data>/logs` (`[log] file`),
/// kept `keep_days`, with the access log (one line per request) in a file of its own there, `access.log.<date>`,
/// and out of the others. `WIREPHOTO_LOG` filters them (default `wirephoto=info`).
fn init_logging(
    config: &Config,
    serving: bool,
) -> Vec<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::filter::filter_fn;
    use tracing_subscriber::prelude::*;
    const ACCESS: &str = "wirephoto::access";
    // The other commands print their own results: only warnings from the log, unless asked.
    let filter = || {
        EnvFilter::try_from_env("WIREPHOTO_LOG").unwrap_or_else(|_| {
            if serving {
                "wirephoto=info,tower_http=warn"
            } else {
                "wirephoto=warn"
            }
            .into()
        })
    };
    let mut guards = Vec::new();
    let dir = config.data.join("logs");
    let mut daily = |prefix: &str| {
        std::fs::create_dir_all(&dir)
            .map_err(anyhow::Error::from)
            .and_then(|()| {
                Ok(tracing_appender::rolling::RollingFileAppender::builder()
                    .rotation(tracing_appender::rolling::Rotation::DAILY)
                    .filename_prefix(prefix)
                    .max_log_files(config.log.keep_days.max(1))
                    .build(&dir)?)
            })
            .map_err(|e| eprintln!("no log file in {}: {e:#}", dir.display()))
            .ok()
            .map(|appender| {
                let (writer, g) = tracing_appender::non_blocking(appender);
                guards.push(g);
                writer
            })
    };
    let files = serving && config.log.file;
    let file = files.then(|| daily("wirephoto.log")).flatten();
    let access = (files && config.log.access)
        .then(|| daily("access.log"))
        .flatten();
    // Requests go to their own file when there is one, and only there.
    let apart = access.is_some();
    let not_access = move |m: &tracing::Metadata| !(apart && m.target() == ACCESS);
    let console = tracing_subscriber::fmt::layer()
        // Colour codes only for a terminal: a container log viewer shows them as junk.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .with_filter(filter())
        .with_filter(filter_fn(not_access));
    let file = file.map(|writer| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(writer)
            .with_filter(filter())
            .with_filter(filter_fn(not_access))
    });
    let access = access.map(|writer| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_target(false)
            .with_writer(writer)
            .with_filter(filter())
            .with_filter(filter_fn(|m| m.target() == ACCESS))
    });
    tracing_subscriber::registry()
        .with(console)
        .with(file)
        .with(access)
        // The last warnings and errors, for `wirephoto status`.
        .with(serving.then_some(wirephoto::problems::Layer))
        .init();
    guards
}

/// `30m`, `24h`, `7d` as seconds.
fn parse_age(s: &str) -> anyhow::Result<i64> {
    let (n, unit) = s.split_at(s.trim_end_matches(|c: char| c.is_ascii_alphabetic()).len());
    let n: i64 = n
        .parse()
        .with_context(|| format!("not an age: {s:?} (like 30m, 24h, 7d)"))?;
    Ok(n * match unit {
        "s" => 1,
        "m" | "min" => 60,
        "h" | "" => 3600,
        "d" => 86400,
        _ => anyhow::bail!("not an age: {s:?} (like 30m, 24h, 7d)"),
    })
}

fn audit(
    config: &Config,
    since: &str,
    key: Option<&str>,
    op: Option<&str>,
    limit: usize,
) -> anyhow::Result<()> {
    let db = open_db(config)?;
    let since = wirephoto::util::now_secs() - parse_age(since)?;
    let rows = wirephoto::audit::list(&db, since, key, op, limit)?;
    for r in &rows {
        let time = chrono::DateTime::from_timestamp(r.time, 0)
            .map(|t| {
                t.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            })
            .unwrap_or_default();
        println!(
            "{time}  {:<13} {:>3} {:>6} ms  key={} client={}  {}{}",
            r.op,
            r.status,
            r.ms,
            r.key.as_deref().unwrap_or("-"),
            r.client.as_deref().unwrap_or("-"),
            r.path
                .as_deref()
                .map(|p| format!("/{p}  "))
                .unwrap_or_default(),
            r.detail
        );
    }
    eprintln!("{} events", rows.len());
    Ok(())
}

fn status(config: &Config, json: bool) -> anyhow::Result<()> {
    let db = open_db(config)?;
    let Some(raw) = db.meta_get("status")? else {
        println!("No status yet: the server is not running, or started less than 30 seconds ago.");
        return Ok(());
    };
    let s: wirephoto::indexer::Snapshot = serde_json::from_slice(&raw)?;
    // The status is saved when it changes, not on a clock: whether the server runs is asked of the server.
    let running = health(config).is_ok();
    let live = wirephoto::status::gather(config, &db, &s, running);
    if json {
        println!("{}", wirephoto::status::json(&s, &live));
    } else {
        print!("{}", wirephoto::status::render(&s, &live));
    }
    Ok(())
}
