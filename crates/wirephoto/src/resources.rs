//! What the server uses, read by `wirephoto status` itself (not saved by the server, which would then write
//! every time a number moved): CPU time and memory of the server and its decode workers (Linux, from /proc),
//! and the free space of a folder's disk.

use std::path::Path;

/// One process's use so far.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    /// User and system CPU time, seconds.
    pub cpu_secs: f64,
    /// Resident memory, bytes.
    pub rss: u64,
}

impl std::ops::Add for Usage {
    type Output = Usage;
    fn add(self, o: Usage) -> Usage {
        Usage {
            cpu_secs: self.cpu_secs + o.cpu_secs,
            rss: self.rss + o.rss,
        }
    }
}

/// The server's use, and that of everything it started (decode workers and their ffmpeg), with how many those
/// are. `None` where /proc is not there (macOS) or the process is gone.
#[cfg(target_os = "linux")]
pub fn of_server(pid: u32) -> Option<(Usage, Usage, usize)> {
    let tick = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
    // (pid, parent, usage) of every process we may read.
    let all: Vec<(u32, u32, Usage)> = std::fs::read_dir("/proc")
        .ok()?
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|p| {
            let stat = std::fs::read_to_string(format!("/proc/{p}/stat")).ok()?;
            let (ppid, usage) = parse_stat(&stat, tick)?;
            let rss = std::fs::read_to_string(format!("/proc/{p}/status"))
                .ok()
                .and_then(|s| vm_rss(&s))
                .unwrap_or(0);
            Some((p, ppid, Usage { rss, ..usage }))
        })
        .collect();
    let server = all.iter().find(|(p, ..)| *p == pid)?.2;
    let mut family = vec![pid];
    let mut children = Usage::default();
    let mut n = 0;
    while let Some(parent) = family.pop() {
        for (p, pp, u) in &all {
            if *pp == parent {
                family.push(*p);
                children = children + *u;
                n += 1;
            }
        }
    }
    Some((server, children, n))
}

#[cfg(not(target_os = "linux"))]
pub fn of_server(_pid: u32) -> Option<(Usage, Usage, usize)> {
    None
}

/// `/proc/<pid>/stat` -> (parent, CPU time). The command name may hold spaces and brackets: fields are
/// counted from the last `)`.
pub fn parse_stat(stat: &str, ticks_per_sec: f64) -> Option<(u32, Usage)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ppid = f.get(1)?.parse().ok()?;
    let utime: f64 = f.get(11)?.parse().ok()?;
    let stime: f64 = f.get(12)?.parse().ok()?;
    Some((
        ppid,
        Usage {
            cpu_secs: (utime + stime) / ticks_per_sec,
            rss: 0,
        },
    ))
}

/// `VmRSS:  123456 kB` from `/proc/<pid>/status`, in bytes.
pub fn vm_rss(status: &str) -> Option<u64> {
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

/// Free (for an unprivileged user) and total bytes of the filesystem holding `path`.
pub fn disk(path: &Path) -> Option<(u64, u64)> {
    let s = rustix::fs::statvfs(path).ok()?;
    Some((s.f_bavail * s.f_frsize, s.f_blocks * s.f_frsize))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_files() {
        let stat = "4242 (wirephoto (de)code) S 4200 4242 4242 0 -1 4194560 2000 0 0 0 250 50 0 0 30 10 \
                    4 0 12345 1000000 2000 18446744073709551615";
        let (ppid, u) = parse_stat(stat, 100.0).unwrap();
        assert_eq!(ppid, 4200);
        assert!((u.cpu_secs - 3.0).abs() < 1e-9);
        assert_eq!(vm_rss("Name:\tx\nVmRSS:\t  2048 kB\n"), Some(2 << 20));
        assert!(disk(Path::new(".")).is_some_and(|(free, total)| free <= total && total > 0));
    }
}
