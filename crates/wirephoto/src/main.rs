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
