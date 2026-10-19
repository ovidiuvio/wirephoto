//! `wirephoto status`: what the server is doing, from the snapshot it saves in the index ([`Snapshot`]) and
//! what this command reads itself ([`Live`]: CPU and memory, disks, files, keys), as a report or as JSON.

use std::path::Path;

use serde::Serialize;

use crate::indexer::{LibraryStatus, Snapshot, Stages};
use crate::resources::{self, Proc, Usage};

/// What `wirephoto status` reads itself rather than from the snapshot: what moves all the time.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Live {
    /// Unix seconds.
    pub now: i64,
    /// The server answers (it is listening).
    pub running: bool,
    pub paused: bool,
    pub listen: String,
    pub tls: bool,
    /// CPU (% of one core, over a moment) and memory of the server and what it started, by role.
    pub processes: Option<Vec<Group>>,
    pub data_dir: String,
    pub data_disk: Option<(u64, u64)>,
    pub index_bytes: u64,
    pub wal_bytes: u64,
    pub cache_file_bytes: u64,
    pub logs_bytes: u64,
    /// Bytes of uploads not finished yet (in the libraries' `.wp-uploads`).
    pub unfinished_upload_bytes: u64,
    /// Free and total bytes of each library's disk, in the snapshot's order.
    pub library_disks: Vec<Option<(u64, u64)>>,
    pub keys: Vec<KeyLine>,
}

/// Processes of one role.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Group {
    /// server, request decoders, pre-generation decoders, ffmpeg and ffprobe
    pub role: String,
    pub processes: usize,
    /// Over the moment measured; 100 is one core.
    pub cpu_percent: f64,
    /// Since each started.
    pub cpu_secs: f64,
    pub rss: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct KeyLine {
    pub name: String,
    pub scope: String,
    pub origin: Option<String>,
    pub libraries: Option<Vec<String>>,
    pub last_used: Option<i64>,
}

/// The role of a process the server started (or the server itself).
fn role(p: &Proc, server: u32) -> &'static str {
    if p.pid == server {
        "server"
    } else if p.comm.starts_with("ffmpeg") || p.comm.starts_with("ffprobe") {
        "ffmpeg and ffprobe"
    } else if p.nice > 0 {
        "pre-generation decoders"
    } else {
        "request decoders"
    }
}

/// CPU over `gap` and memory of the server's processes, grouped by role.
pub fn processes(pid: u32, gap: std::time::Duration) -> Option<Vec<Group>> {
    let before = resources::family(pid)?;
    std::thread::sleep(gap);
    let after = resources::family(pid)?;
    let mut groups: Vec<Group> = [
        "server",
        "request decoders",
        "pre-generation decoders",
        "ffmpeg and ffprobe",
    ]
    .iter()
    .map(|r| Group {
        role: r.to_string(),
        ..Group::default()
    })
    .collect();
    for p in &after {
        let was = before
            .iter()
            .find(|b| b.pid == p.pid)
            .map_or(Usage::default(), |b| b.usage);
        let g = groups
            .iter_mut()
            .find(|g| g.role == role(p, pid))
            .expect("every role is listed");
        g.processes += 1;
        g.cpu_percent += (p.usage.cpu_secs - was.cpu_secs).max(0.0) / gap.as_secs_f64() * 100.0;
        g.cpu_secs += p.usage.cpu_secs;
        g.rss += p.usage.rss;
    }
    Some(groups)
}

fn size_of(p: &Path) -> u64 {
    std::fs::metadata(p).map_or(0, |m| m.len())
}

fn dir_size(p: &Path) -> u64 {
    std::fs::read_dir(p)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

/// Reads what [`Live`] holds.
pub fn gather(
    config: &crate::config::Config,
    db: &crate::db::Db,
    s: &Snapshot,
    running: bool,
) -> Live {
    let processes = if running {
        processes(s.pid, std::time::Duration::from_millis(500))
    } else {
        None
    };
    let keys = crate::auth::Keys::list(db)
        .unwrap_or_default()
        .into_iter()
        .map(|k| KeyLine {
            name: k.name,
            scope: format!("{:?}", k.scope).to_lowercase(),
            origin: k.origin,
            libraries: k.libraries,
            last_used: k.last_used,
        })
        .collect();
    let unfinished_upload_bytes = db
        .read(|c| {
            Ok(c.query_row(
                "SELECT COALESCE(SUM(received), 0) FROM tus WHERE entry IS NULL",
                [],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .unwrap_or(0) as u64;
    Live {
        now: crate::util::now_secs(),
        running,
        paused: crate::indexer::paused_in(db),
        listen: config.listen.clone(),
        tls: config.tls.is_some(),
        processes,
        data_dir: config.data.display().to_string(),
        data_disk: resources::disk(&config.data),
        index_bytes: size_of(&config.data.join("index.db")),
        wal_bytes: size_of(&config.data.join("index.db-wal")),
        cache_file_bytes: size_of(&config.data.join("previews.redb")),
        logs_bytes: dir_size(&config.data.join("logs")),
        unfinished_upload_bytes,
        library_disks: s
            .libraries
            .iter()
            .map(|l| resources::disk(Path::new(&l.path)))
            .collect(),
        keys,
    }
}

// ---- formatting ----

pub fn human(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 40 => format!("{:.1} TB", b as f64 / (1u64 << 40) as f64),
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

/// 1234567 -> "1,234,567".
pub fn n(v: u64) -> String {
    let s = v.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// A duration in words: "45 s", "3 min 12 s", "2 h 05 min", "3 days 4 h".
pub fn dur(secs: f64) -> String {
    let s = secs.max(0.0).round() as i64;
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{} min {:02} s", s / 60, s % 60),
        3600..=172_799 => format!("{} h {:02} min", s / 3600, s % 3600 / 60),
        _ => format!("{} days {} h", s / 86400, s % 86400 / 3600),
    }
}

/// Milliseconds: "12 ms", "1.23 s", "1 min 05 s".
pub fn ms(v: u64) -> String {
    match v {
        0..=999 => format!("{v} ms"),
        1000..=59_999 => format!("{:.2} s", v as f64 / 1000.0),
        _ => dur(v as f64 / 1000.0),
    }
}

fn clock(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_default()
}

fn ago(now: i64, t: i64) -> String {
    format!("{} ago", dur((now - t) as f64))
}

fn pct(v: f64) -> String {
    format!("{v:.1}%")
}

/// A table: the first `left` columns aligned left, the others right; indented two spaces.
fn table(head: &[&str], rows: &[Vec<String>], left: usize) -> String {
    let cols = head.len();
    let mut width: Vec<usize> = head.iter().map(|h| h.chars().count()).collect();
    for r in rows {
        for (i, c) in r.iter().enumerate().take(cols) {
            width[i] = width[i].max(c.chars().count());
        }
    }
    let line = |cells: Vec<String>| {
        let mut out = String::from("  ");
        for (i, c) in cells.iter().enumerate() {
            let pad = width[i].saturating_sub(c.chars().count());
            if i > 0 {
                out.push_str("  ");
            }
            if i < left {
                out.push_str(c);
                out.push_str(&" ".repeat(pad));
            } else {
                out.push_str(&" ".repeat(pad));
                out.push_str(c);
            }
        }
        out.trim_end().to_string() + "\n"
    };
    let mut out = line(head.iter().map(|h| h.to_string()).collect());
    for r in rows {
        out.push_str(&line(r.clone()));
    }
    out
}

/// A label and its value, the values lined up.
fn kv(out: &mut String, indent: usize, label: &str, value: impl AsRef<str>) {
    let pad = 26usize.saturating_sub(indent + label.chars().count());
    out.push_str(&format!(
        "{}{label}{}{}\n",
        " ".repeat(indent),
        " ".repeat(pad.max(1)),
        value.as_ref()
    ));
}

/// A stage's progress: "12,000 of 25,024 done · 3.1/s · about 1 h 10 min left".
fn progress(done: u64, total: u64, rate: f64) -> String {
    let left = total.saturating_sub(done);
    let mut s = format!("{} of {} done", n(done), n(total));
    if left > 0 {
        if rate > 0.0 {
            s.push_str(&format!(
                " · {rate:.1}/s · about {} left",
                dur(left as f64 / rate)
            ));
        } else {
            s.push_str(" · waiting");
        }
    }
    s
}

fn library(out: &mut String, s: &Snapshot, l: &LibraryStatus, disk: Option<(u64, u64)>, now: i64) {
    out.push_str(&format!(
        "\nLibrary {} \"{}\" at {} ({})\n",
        l.id,
        l.name,
        l.path,
        if l.read_only {
            "read-only"
        } else {
            "read-write"
        }
    ));
    kv(
        out,
        2,
        "Disk",
        disk.map(|(f, t)| format!("{} free of {}", human(f), human(t)))
            .unwrap_or_else(|| "free space unknown".into()),
    );
    if let Some(c) = &l.contents {
        let files = c.images + c.raws + c.videos + c.others;
        kv(
            out,
            2,
            "Holds",
            format!(
                "{} files, {}, in {} folders",
                n(files),
                human(c.bytes),
                n(c.folders)
            ),
        );
        kv(
            out,
            2,
            "",
            format!(
                "{} photos · {} RAW · {} videos · {} other",
                n(c.images),
                n(c.raws),
                n(c.videos),
                n(c.others)
            ),
        );
        kv(
            out,
            2,
            "Trash",
            format!("{} files, {}", n(c.trash_files), human(c.trash_bytes)),
        );
        kv(
            out,
            2,
            "Duplicates",
            if c.duplicate_sets == 0 {
                "none found (among files hashed so far)".to_string()
            } else {
                format!(
                    "{} sets of identical files: {} extra copies, {}",
                    n(c.duplicate_sets),
                    n(c.duplicate_copies),
                    human(c.duplicate_bytes)
                )
            },
        );
    }
    let watch = if l.watching {
        let mut w = "watched".to_string();
        if !l.watch_note.is_empty() {
            w.push_str(&format!(": {}", l.watch_note));
        }
        if l.watched_folders > 0 {
            w.push_str(&format!(" ({} folders)", n(l.watched_folders)));
        }
        w
    } else {
        let mut w = format!(
            "polled: folder times compared every {} s",
            l.poll_seconds.max(5)
        );
        if !l.watch_note.is_empty() {
            w.push_str(&format!(", because {}", l.watch_note));
        }
        w
    };
    kv(out, 2, "Changes on disk", watch);
    if let Some(sc) = &l.last_scan {
        kv(
            out,
            2,
            "Last full scan",
            format!(
                "{} ({}), {} entries in {}",
                clock(sc.at),
                ago(now, sc.at),
                n(sc.entries),
                dur(sc.secs)
            ),
        );
    }
    match &l.stages {
        Some(st) if st.state == "scanning" => {
            kv(
                out,
                2,
                "Background work",
                format!(
                    "scanning: {} entries listed ({:.0}/s); the rest waits for it",
                    n(st.listed),
                    st.list_rate
                ),
            );
        }
        Some(st) => stages(out, s, st),
        None => kv(out, 2, "Background work", l.progress.clone()),
    }
}

fn stages(out: &mut String, s: &Snapshot, st: &Stages) {
    let state = match st.state.as_str() {
        "paused" => "PAUSED (`wirephoto resume` continues it)".to_string(),
        "working" if s.yielding => format!(
            "working, slowed: the API had requests in the last {} s, and requests come first",
            s.yield_seconds
        ),
        other => other.to_string(),
    };
    kv(out, 2, "Background work", state);
    kv(
        out,
        4,
        "reading facts",
        progress(st.files - st.facts_pending, st.files, st.facts_rate),
    );
    if st.pregenerate {
        let made = st.previews_done + st.previews_pending;
        let mut line = progress(st.previews_done, made, st.previews_rate);
        if st.previews_failed > 0 {
            line.push_str(&format!(
                " · {} failed (tried again after a restart)",
                n(st.previews_failed)
            ));
        }
        if st.not_previewable > 0 {
            line.push_str(&format!(" · {} cannot have one", n(st.not_previewable)));
        }
        kv(out, 4, "640 px previews", line);
    } else {
        kv(
            out,
            4,
            "640 px previews",
            "made when first asked for ([cache] pregenerate = false)",
        );
    }
    let mut line = progress(st.files - st.hashes_pending, st.files, st.hashes_rate);
    if st.hash_failed > 0 {
        line.push_str(&format!(
            " · {} could not be read (tried again after a restart)",
            n(st.hash_failed)
        ));
    }
    kv(out, 4, "hashes", line);
}

/// The report.
pub fn render(s: &Snapshot, live: &Live) -> String {
    let mut out = String::new();
    let now = live.now;
    out.push_str(&format!("wirephoto {}\n", s.version));
    if live.running {
        kv(
            &mut out,
            2,
            "Running since",
            format!(
                "{} (up {}); figures saved {}",
                clock(s.started),
                dur((now - s.started) as f64),
                ago(now, s.time)
            ),
        );
    } else {
        kv(
            &mut out,
            2,
            "NOT RUNNING",
            format!(
                "nothing answers on {}; the figures below are from {} ({})",
                live.listen,
                clock(s.time),
                ago(now, s.time)
            ),
        );
    }
    kv(
        &mut out,
        2,
        "Listening on",
        format!(
            "{} ({})",
            live.listen,
            if live.tls {
                "HTTPS"
            } else {
                "plain HTTP: browser apps need HTTPS from a proxy in front"
            }
        ),
    );
    if live.paused {
        kv(
            &mut out,
            2,
            "Background work",
            "PAUSED for every library (`wirephoto resume` continues it)",
        );
    }

    // ---- server ----
    out.push_str("\nServer\n");
    match &live.processes {
        Some(groups) => {
            out.push_str("  CPU now (over half a second; 100% is one core), memory, CPU time used since start:\n");
            let rows: Vec<Vec<String>> = groups
                .iter()
                .filter(|g| g.processes > 0 || g.role == "server")
                .map(|g| {
                    vec![
                        g.role.clone(),
                        n(g.processes as u64),
                        pct(g.cpu_percent),
                        human(g.rss),
                        dur(g.cpu_secs),
                    ]
                })
                .collect();
            out.push_str(&table(
                &["processes", "count", "CPU now", "memory", "CPU used"],
                &rows,
                1,
            ));
        }
        None if live.running => {
            out.push_str("  (CPU and memory: only on Linux, where /proc is)\n");
        }
        None => {}
    }
    kv(
        &mut out,
        2,
        "Renderings at once",
        format!(
            "up to {} for requests, and up to {} for pre-generation (its own workers, at low CPU priority)",
            s.workers, s.background
        ),
    );
    kv(
        &mut out,
        2,
        "Worker processes",
        format!(
            "{} started since start, {} lost (crashed or hung; each costs one preview)",
            n(s.workers_started),
            n(s.workers_lost)
        ),
    );
    kv(
        &mut out,
        2,
        "GPU",
        if s.gpu.is_empty() { "-" } else { &s.gpu },
    );
    if s.gpu_decoded + s.gpu_fell_back > 0 {
        kv(
            &mut out,
            4,
            "",
            format!(
                "{} images decoded on it; {} it failed on, decoded on the CPU instead",
                n(s.gpu_decoded),
                n(s.gpu_fell_back)
            ),
        );
    }
    kv(
        &mut out,
        2,
        "ffmpeg",
        if s.ffmpeg.is_empty() { "-" } else { &s.ffmpeg },
    );
    kv(
        &mut out,
        2,
        "HEIC decoded with",
        if s.heic_route.is_empty() {
            "-"
        } else {
            &s.heic_route
        },
    );

    // ---- storage ----
    out.push_str("\nStorage\n");
    kv(
        &mut out,
        2,
        "Data folder",
        format!(
            "{}: {}",
            live.data_dir,
            live.data_disk
                .map(|(f, t)| format!("{} free of {}", human(f), human(t)))
                .unwrap_or_else(|| "free space unknown".into())
        ),
    );
    kv(
        &mut out,
        4,
        "index",
        format!(
            "{} (index.db {} + write-ahead log {})",
            human(live.index_bytes + live.wal_bytes),
            human(live.index_bytes),
            human(live.wal_bytes)
        ),
    );
    kv(
        &mut out,
        4,
        "preview cache",
        format!(
            "{} of previews in a {} file (previews.redb; it keeps freed space for new ones) · budget {}",
            human(s.cache_bytes),
            human(live.cache_file_bytes),
            human(s.cache_budget)
        ),
    );
    kv(&mut out, 4, "logs", human(live.logs_bytes));
    kv(
        &mut out,
        4,
        "unfinished uploads",
        human(live.unfinished_upload_bytes),
    );

    // ---- libraries ----
    for (i, l) in s.libraries.iter().enumerate() {
        library(
            &mut out,
            s,
            l,
            live.library_disks.get(i).copied().flatten(),
            now,
        );
    }
    let w = &s.disk_watch;
    out.push_str(&format!(
        "\nChanges on disk since start (made outside wirephoto): {} filesystem events, {} folders listed again for\n  \
         them; the index last changed for one {}\n",
        n(w.events),
        n(w.folders_relisted),
        if w.last_change > 0 {
            ago(now, w.last_change)
        } else {
            "never".into()
        }
    ));

    // ---- requests ----
    out.push_str(
        "\nRequests since start (time until the answer starts; a download or a stream goes on after)\n",
    );
    if s.requests.is_empty() {
        out.push_str("  none yet\n");
    } else {
        let rows: Vec<Vec<String>> = s
            .requests
            .iter()
            .map(|r| {
                vec![
                    r.kind.clone(),
                    n(r.count),
                    n(r.count - r.client_errors - r.server_errors - r.not_cached),
                    n(r.client_errors),
                    n(r.server_errors),
                    n(r.not_cached),
                    human(r.bytes),
                    human(r.bytes_in),
                    ms(r.p50_ms),
                    ms(r.p95_ms),
                    ms(r.max_ms),
                ]
            })
            .collect();
        out.push_str(&table(
            &[
                "kind",
                "count",
                "ok",
                "4xx",
                "5xx",
                "not cached",
                "sent",
                "received",
                "median",
                "95%",
                "slowest",
            ],
            &rows,
            1,
        ));
        out.push_str(
            "  4xx: refused (bad request, no key, not found); 5xx: failed on the server; not cached: asked\n  \
             only if already made (lith's stand-ins), and it was not; that is expected, not an error.\n",
        );
    }
    out.push_str(&format!(
        "  Live change feeds open now: {} (apps following changes)\n",
        s.change_feeds
    ));

    // ---- previews ----
    out.push_str("\nPreviews asked for since start, by size, and where each came from\n");
    if s.previews_by_size.is_empty() {
        out.push_str("  none yet\n");
    } else {
        let rows: Vec<Vec<String>> = s
            .previews_by_size
            .iter()
            .map(|p| {
                vec![
                    format!("{} px", p.size),
                    n(p.memory + p.disk + p.from_cached + p.rendered + p.not_cached),
                    n(p.memory),
                    n(p.disk),
                    n(p.from_cached),
                    n(p.rendered),
                    n(p.not_cached),
                ]
            })
            .collect();
        out.push_str(&table(
            &[
                "size",
                "asked",
                "memory",
                "disk cache",
                "from the 640",
                "from the original",
                "not cached",
            ],
            &rows,
            1,
        ));
        out.push_str(
            "  from the 640: shrunk from the cached 640 px (about a millisecond); from the original: decoded\n  \
             from the photo itself (below).\n",
        );
    }
    if let Some(t) = s.times.iter().find(|t| t.kind == "from the 640") {
        out.push_str(&format!(
            "  Shrinking from the 640: {} made, median {}, 95% {}, slowest {}\n",
            n(t.count),
            ms(t.p50_ms),
            ms(t.p95_ms),
            ms(t.max_ms)
        ));
    }

    out.push_str("\nPreviews made from originals since start\n");
    if s.renders.is_empty() {
        out.push_str("  none yet\n");
    } else {
        let rows: Vec<Vec<String>> = s
            .renders
            .iter()
            .map(|r| {
                vec![
                    if r.purpose == "request" {
                        "a request".into()
                    } else {
                        "ahead".into()
                    },
                    r.kind.clone(),
                    r.decoder.clone(),
                    format!("{} px", r.size),
                    n(r.count),
                    n(r.failed),
                    ms(r.p50_ms),
                    ms(r.p95_ms),
                    ms(r.max_ms),
                    ms(r.mean_ms),
                    ms(r.wait_ms),
                    ms(r.decode_ms),
                    ms(r.finish_ms),
                    ms(r.other_ms),
                ]
            })
            .collect();
        out.push_str(&table(
            &[
                "for",
                "type",
                "decoder",
                "size",
                "made",
                "failed",
                "median",
                "95%",
                "slowest",
                "mean",
                "= waiting",
                "+ decoding",
                "+ resize..encode",
                "+ other",
            ],
            &rows,
            4,
        ));
        out.push_str(
            "  for: a request (someone waited for it) or ahead (made before anyone asked: pre-generation, uploads).\n  \
             waiting: for a free worker; decoding: reading the file and decoding it (on the GPU too); resize..encode:\n  \
             resizing, colour, orientation and the JPEG; other: starting the job, sending the file, storing the result.\n",
        );
        let fell: u64 = s.renders.iter().map(|r| r.fell_back).sum();
        if fell > 0 {
            out.push_str(&format!(
                "  {} of these were decoded on the CPU after the GPU failed on them.\n",
                n(fell)
            ));
        }
    }

    // ---- caches ----
    out.push_str("\nCaches\n");
    kv(
        &mut out,
        2,
        "Disk cache",
        format!(
            "{} of a {} budget; {} evicted since start to stay within it",
            human(s.cache_bytes),
            human(s.cache_budget),
            human(s.evicted_bytes)
        ),
    );
    for c in &s.cache_sizes {
        kv(
            &mut out,
            4,
            &format!("{} px", c.size),
            format!(
                "{} previews, {} ({} each on average)",
                n(c.count),
                human(c.bytes),
                human(c.bytes / c.count.max(1))
            ),
        );
    }
    let (m_n, m_b, m_cap) = s.memory_cache;
    kv(
        &mut out,
        2,
        "Memory cache",
        format!(
            "{} previews, {} of {} (kept 10 minutes after their last use)",
            n(m_n),
            human(m_b),
            human(m_cap)
        ),
    );

    // ---- apps ----
    out.push_str("\nApps\n");
    if live.keys.is_empty() {
        kv(
            &mut out,
            2,
            "API keys",
            "none (`wirephoto keys create` makes one)",
        );
    }
    for k in &live.keys {
        let mut what = format!("{} key", k.scope);
        match &k.origin {
            Some(o) => what.push_str(&format!(", for the app at {o}")),
            None => what.push_str(", for scripts (no browser origin)"),
        }
        if let Some(libs) = &k.libraries {
            what.push_str(&format!(", libraries {}", libs.join(", ")));
        }
        what.push_str(&match k.last_used {
            Some(t) => format!("; last used {}", ago(now, t)),
            None => "; never used".into(),
        });
        kv(&mut out, 2, &format!("key {}", k.name), what);
    }
    let u = &s.uploads;
    kv(
        &mut out,
        2,
        "Uploads (tus)",
        format!(
            "{} unfinished now; since start {} created, {} stored, {} abandoned, {} expired; {} received",
            n(u.open),
            n(u.opened),
            n(u.completed),
            n(u.abandoned),
            n(u.expired),
            human(u.bytes)
        ),
    );

    // ---- problems ----
    out.push_str(
        "\nRecent warnings and errors (the last 20, newest last; all of them are in the log)\n",
    );
    if s.problems.is_empty() {
        out.push_str("  none since start\n");
    } else {
        for p in &s.problems {
            let repeats = if p.repeats > 1 {
                format!(" (×{})", p.repeats)
            } else {
                String::new()
            };
            let target = p.target.trim_start_matches("wirephoto::");
            out.push_str(&format!(
                "  {}  {:<5} {}: {}{repeats}\n",
                chrono::DateTime::from_timestamp(p.time, 0)
                    .map(|d| d.format("%m-%d %H:%M:%S").to_string())
                    .unwrap_or_default(),
                p.level,
                target,
                p.message
            ));
        }
    }
    out
}

/// The JSON form (`wirephoto status --json`): the snapshot and the live readings.
pub fn json(s: &Snapshot, live: &Live) -> String {
    serde_json::to_string_pretty(&serde_json::json!({ "server": s, "live": live }))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_and_tables() {
        assert_eq!(dur(45.0), "45 s");
        assert_eq!(dur(192.0), "3 min 12 s");
        assert_eq!(dur(7500.0), "2 h 05 min");
        assert_eq!(ms(1234), "1.23 s");
        assert_eq!(n(1234567), "1,234,567");
        let t = table(
            &["kind", "count"],
            &[vec!["previews 320".into(), "32".into()]],
            1,
        );
        assert_eq!(t, "  kind          count\n  previews 320     32\n");
    }

    #[test]
    fn a_report_says_what_is_what() {
        let s: Snapshot = serde_json::from_value(serde_json::json!({
            "version": "0.1.0 (test)", "started": 1000, "time": 1600,
            "libraries": [{
                "id": "lib_archive", "name": "Archive", "path": "/archive", "watching": true, "poll_seconds": 60,
                "progress": "", "watch_note": "inotify, a watch per folder", "watched_folders": 35,
                "contents": {"folders": 35, "images": 21839, "raws": 3185, "videos": 0, "others": 0, "bytes": 245_000_000_000u64,
                             "trash_files": 0, "trash_bytes": 0, "duplicate_sets": 71, "duplicate_copies": 71, "duplicate_bytes": 126_000_000},
                "stages": {"state": "working", "listed": 0, "list_rate": 0.0, "files": 25024, "facts_pending": 0, "facts_rate": 0.0,
                           "previews_done": 25000, "previews_pending": 23, "previews_failed": 0, "not_previewable": 1,
                           "previews_rate": 0.5, "pregenerate": true, "hashes_pending": 0, "hash_failed": 0, "hashes_rate": 0.0}
            }],
            "times": [{"kind": "from the 640", "count": 32, "mean_ms": 3, "p50_ms": 2, "p95_ms": 13, "max_ms": 29}],
            "served_memory": 0, "served_disk": 0, "rendered": 37, "cache_bytes": 890_000_000u64, "cache_budget": 107_374_182_400u64,
            "workers": 4, "background": 2, "workers_started": 6, "workers_lost": 0,
            "gpu": "VA-API /dev/dri/renderD128: HEVC, HEVC 10-bit, JPEG", "gpu_decoded": 37, "gpu_fell_back": 0, "pid": 1,
            "requests": [{"kind": "previews 320", "count": 32, "client_errors": 0, "server_errors": 0, "not_cached": 2,
                          "bytes": 431_800, "bytes_in": 0, "p50_ms": 5700, "p95_ms": 10670, "max_ms": 10670}],
            "previews_by_size": [{"size": 320, "memory": 0, "disk": 0, "from_cached": 30, "rendered": 0, "not_cached": 2}],
            "renders": [{"purpose": "ahead", "kind": "heic", "decoder": "GPU", "size": 640, "count": 37, "failed": 0,
                         "fell_back": 0, "p50_ms": 8180, "p95_ms": 12350, "max_ms": 14040, "mean_ms": 7850,
                         "wait_ms": 2100, "decode_ms": 5200, "finish_ms": 30, "other_ms": 520}],
            "problems": [{"time": 1500, "level": "WARN", "target": "wirephoto::indexer", "message": "watch limit reached", "repeats": 3}],
            "ffmpeg": "ffmpeg version 7.1.1", "heic_route": "ffmpeg", "change_feeds": 1
        }))
        .unwrap();
        let live = Live {
            now: 1612,
            running: true,
            listen: "0.0.0.0:8443".into(),
            ..Live::default()
        };
        let r = render(&s, &live);
        for want in [
            "Running since",
            "figures saved 12 s ago",
            "25,000 of 25,023 done · 0.5/s · about 46 s left",
            "71 sets of identical files",
            "watched: inotify, a watch per folder (35 folders)",
            "previews 320",
            "not cached",
            "ahead",
            "8.18 s",
            "= waiting",
            "2.10 s",
            "5.20 s",
            "watch limit reached (×3)",
            "Live change feeds open now: 1",
        ] {
            assert!(r.contains(want), "missing {want:?} in:\n{r}");
        }
    }
}
