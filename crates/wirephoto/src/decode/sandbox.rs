//! The worker's sandbox. On Linux: no privileges (root drops to `nobody`, nothing can regain any) and a seccomp
//! filter that refuses the network, creating or changing files, and the kernel interfaces a decoder never needs.
//! Reading stays allowed: the shared libraries, and ffmpeg (which inherits the filter) reading its input. The
//! filter is a deny list, so decoders and the allocator keep working across libc and kernel versions.
//! Elsewhere (macOS, for development) the worker is only a separate process.

#[cfg(target_os = "linux")]
pub fn enter() -> anyhow::Result<()> {
    use std::collections::BTreeMap;

    use anyhow::{Context, bail};
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen as Len, SeccompCmpOp as Cmp,
        SeccompCondition as Cond, SeccompFilter, SeccompRule,
    };

    // SAFETY: plain syscalls on this process, before any other thread exists.
    unsafe {
        // Die with the server.
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
        if libc::geteuid() == 0 || libc::getegid() == 0 {
            const NOBODY: u32 = 65534;
            if libc::setgroups(0, std::ptr::null()) != 0
                || libc::setgid(NOBODY) != 0
                || libc::setuid(NOBODY) != 0
            {
                bail!("dropping root: {}", std::io::Error::last_os_error());
            }
        }
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            bail!("no_new_privs: {}", std::io::Error::last_os_error());
        }
        // Not ptrace-able or core-dumped by anyone.
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
    }

    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    let deny = [
        // Network.
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_connect,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_accept,
        libc::SYS_accept4,
        // New or changed files.
        libc::SYS_mknodat,
        libc::SYS_mkdirat,
        libc::SYS_unlinkat,
        libc::SYS_renameat2,
        libc::SYS_linkat,
        libc::SYS_symlinkat,
        libc::SYS_truncate,
        libc::SYS_fchmod,
        libc::SYS_fchmodat,
        libc::SYS_fchownat,
        libc::SYS_fchown,
        libc::SYS_utimensat,
        libc::SYS_setxattr,
        libc::SYS_lsetxattr,
        libc::SYS_fsetxattr,
        libc::SYS_removexattr,
        libc::SYS_lremovexattr,
        libc::SYS_fremovexattr,
        libc::SYS_memfd_create,
        libc::SYS_open_by_handle_at,
        libc::SYS_name_to_handle_at,
        // openat2 hides its flags behind a pointer; glibc falls back to openat.
        libc::SYS_openat2,
        // io_uring would do all of the above without passing this filter.
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        // Other processes, privileges, the kernel.
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_setuid,
        libc::SYS_setgid,
        libc::SYS_setreuid,
        libc::SYS_setregid,
        libc::SYS_setresuid,
        libc::SYS_setresgid,
        libc::SYS_setgroups,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_kexec_load,
        libc::SYS_reboot,
        libc::SYS_swapon,
        libc::SYS_swapoff,
    ];
    for s in deny {
        rules.insert(s, vec![]);
    }
    #[cfg(target_arch = "x86_64")]
    for s in [
        libc::SYS_mknod,
        libc::SYS_mkdir,
        libc::SYS_rmdir,
        libc::SYS_unlink,
        libc::SYS_rename,
        libc::SYS_renameat,
        libc::SYS_link,
        libc::SYS_symlink,
        libc::SYS_chmod,
        libc::SYS_chown,
        libc::SYS_lchown,
        libc::SYS_utime,
        libc::SYS_utimes,
        libc::SYS_futimesat,
        libc::SYS_creat,
    ] {
        rules.insert(s, vec![]);
    }

    // Opening for anything but reading.
    let writing = |arg: u8| -> anyhow::Result<Vec<SeccompRule>> {
        [libc::O_WRONLY, libc::O_RDWR, libc::O_CREAT, libc::O_TRUNC]
            .into_iter()
            .map(|flag| {
                Ok(SeccompRule::new(vec![Cond::new(
                    arg,
                    Len::Dword,
                    Cmp::MaskedEq(flag as u64),
                    flag as u64,
                )?])?)
            })
            .collect()
    };
    rules.insert(libc::SYS_openat, writing(2)?);
    #[cfg(target_arch = "x86_64")]
    rules.insert(libc::SYS_open, writing(1)?);

    let arch = std::env::consts::ARCH
        .try_into()
        .context("seccomp on this architecture")?;
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )?;
    let program: BpfProgram = filter.try_into()?;
    // The worker has one thread here, so the filter needs no thread sync. The `seccomp` system call (Linux
    // 3.17) is tried first; older kernels, like those of many NAS systems, have the same filter through
    // `prctl`. A kernel built without seccomp filters at all gets none: the worker is still its own process,
    // without privileges and unable to gain any, and says so on stderr.
    match seccompiler::apply_filter(&program) {
        Ok(()) => Ok(()),
        Err(seccompiler::Error::Seccomp(e)) if e.raw_os_error() == Some(libc::ENOSYS) => {
            let prog = libc::sock_fprog {
                len: program.len() as u16,
                // seccompiler's `sock_filter` is the kernel's layout, as libc's is.
                filter: program.as_ptr() as *mut libc::sock_filter,
            };
            // SAFETY: `prog` points into `program`, which outlives the call; the kernel copies the filter.
            let rc = unsafe {
                libc::prctl(
                    libc::PR_SET_SECCOMP,
                    libc::SECCOMP_MODE_FILTER as libc::c_ulong,
                    &prog as *const libc::sock_fprog,
                    0,
                    0,
                )
            };
            if rc == 0 {
                return Ok(());
            }
            unfiltered(std::io::Error::last_os_error())
        }
        Err(seccompiler::Error::Seccomp(e)) if e.raw_os_error() == Some(libc::EINVAL) => unfiltered(e),
        Err(e) => Err(e.into()),
    }
}

/// No seccomp filter on this kernel: carry on without it, unless `WIREPHOTO_SECCOMP=required`.
#[cfg(target_os = "linux")]
fn unfiltered(why: std::io::Error) -> anyhow::Result<()> {
    if std::env::var("WIREPHOTO_SECCOMP").as_deref() == Ok("required") {
        anyhow::bail!("this kernel has no seccomp filters ({why}) and WIREPHOTO_SECCOMP=required");
    }
    eprintln!(
        "decode worker: this kernel has no seccomp filters ({why}); running without the system-call filter \
         (still a separate process without privileges). Set WIREPHOTO_SECCOMP=required to refuse instead."
    );
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn enter() -> anyhow::Result<()> {
    Ok(())
}
