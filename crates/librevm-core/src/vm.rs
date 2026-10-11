//! Running a VM.
//!
//! crosvm is spawned as a child process, its pid recorded, and its console sent
//! to a socket or to our stdout. Keeping this in the core crate means the CLI and
//! the daemon start guests the same way.

use crate::backend::{self, ConsoleSink, Detected};
use crate::config::{Protection, VmConfig};
use crate::store::Store;
use anyhow::{bail, Context, Result};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// What we know about a VM's runtime state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Defined but not running.
    Stopped,
    /// A process is alive for it.
    Running { pid: i32 },
    /// A pid file exists but the process is gone: it died without cleaning up.
    Stale { pid: i32 },
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            State::Stopped => f.write_str("stopped"),
            State::Running { pid } => write!(f, "running (pid {pid})"),
            State::Stale { pid } => write!(f, "stopped (stale pid {pid})"),
        }
    }
}

/// Is there a live process with this pid?
///
/// `kill(pid, 0)` fails with `EPERM` when the process exists but belongs to another
/// user, which is the normal case for a GUI started as one user looking at a guest
/// started as another. Treating that as "dead" makes a perfectly healthy machine
/// look stale, and the caller then clears its pid file and tries to start a second
/// copy on top of the first.
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // signal 0 checks for existence without delivering anything
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    matches!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EPERM)
    )
}

/// Read the recorded pid for a VM, if any.
pub fn read_pid(store: &Store, name: &str) -> Option<i32> {
    let text = std::fs::read_to_string(store.pid_file(name)).ok()?;
    text.trim().parse::<i32>().ok()
}

/// Work out the state of a VM from its pid file and the process table.
pub fn state(store: &Store, name: &str) -> State {
    match read_pid(store, name) {
        Some(pid) if pid_alive(pid) => State::Running { pid },
        Some(pid) => State::Stale { pid },
        None => State::Stopped,
    }
}

/// A prepared launch: the argv, plus the resolved hypervisor.
pub struct Plan {
    pub argv: Vec<String>,
    pub hypervisor: Detected,
    pub console_is_socket: bool,
}

/// Resolve everything needed to start a VM, without starting it.
///
/// `socket_console` decides whether the console goes to a Unix socket (so
/// clients can attach and detach) or to the caller's stdout.
pub fn plan(store: &Store, vm: &VmConfig, socket_console: bool) -> Result<Plan> {
    vm.validate()?;

    let resolved = backend::resolve(vm.hypervisor.backend)?;
    let crosvm = backend::find_crosvm(vm.hypervisor.crosvm.as_deref())?;

    let sink = if socket_console {
        let path = vm
            .console
            .socket
            .clone()
            .unwrap_or_else(|| store.console_socket(&vm.name));
        ConsoleSink::UnixStream(path)
    } else {
        ConsoleSink::Stdout
    };
    let console_is_socket = matches!(sink, ConsoleSink::UnixStream(_));

    let argv = backend::build_command(vm, &resolved, &crosvm, &sink)?;

    Ok(Plan { argv, hypervisor: resolved, console_is_socket })
}

/// Start a VM in the background, detached from us.
///
/// Returns the child's pid. The console is served on the VM's socket.
pub fn start(store: &Store, name: &str) -> Result<i32> {
    let vm = store.load(name)?;
    vm.validate()?;

    match state(store, name) {
        State::Running { pid } => bail!("{name} is already running (pid {pid})"),
        State::Stale { pid } => {
            log::warn!("{name}: clearing stale pid {pid}");
            let _ = std::fs::remove_file(store.pid_file(name));
        }
        State::Stopped => {}
    }

    store.create_dirs()?;
    let plan = plan(store, &vm, true)?;

    let log_path = store.console_log(name);
    if let Some(dir) = log_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let log = std::fs::File::create(&log_path)
        .with_context(|| format!("creating {}", log_path.display()))?;

    log::info!("starting {name} on {}", plan.hypervisor);
    log::debug!("argv: {}", plan.argv.join(" "));

    let child = Command::new(&plan.argv[0])
        .args(&plan.argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        // New session, so the guest is not killed when the caller's terminal goes
        // away. This is what makes `librevm start` safe to run over ssh.
        .process_group(0)
        .spawn()
        .with_context(|| format!("spawning {}", plan.argv[0]))?;

    let pid = child.id() as i32;
    std::fs::write(store.pid_file(name), format!("{pid}\n"))
        .with_context(|| format!("writing {}", store.pid_file(name).display()))?;

    // The child is deliberately not reaped here: it is meant to outlive us. The
    // daemon, or a later `status`, notices when it is gone.
    std::mem::forget(child);

    log::info!("{name} started with pid {pid}");
    Ok(pid)
}

/// Run a VM in the foreground, with its console on our stdout.
///
/// Returns the process exit status.
pub fn run_foreground(store: &Store, name: &str) -> Result<i32> {
    let vm = store.load(name)?;
    let plan = plan(store, &vm, false)?;

    log::info!("running {name} in the foreground on {}", plan.hypervisor);

    let status = Command::new(&plan.argv[0])
        .args(&plan.argv[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("running {}", plan.argv[0]))?;

    Ok(status.code().unwrap_or(-1))
}

/// Ask a VM to stop, then insist if it does not.
///
/// The guest is not given a chance to flush anything: crosvm does not forward a
/// signal into the guest. Use the guest's own shutdown when that matters.
pub fn stop(store: &Store, name: &str, grace_ms: u64) -> Result<()> {
    let pid = match state(store, name) {
        State::Running { pid } => pid,
        State::Stale { pid } => {
            let _ = std::fs::remove_file(store.pid_file(name));
            bail!("{name} was not running (cleared stale pid {pid})");
        }
        State::Stopped => bail!("{name} is not running"),
    };

    log::info!("stopping {name} (pid {pid})");
    unsafe { libc::kill(pid, libc::SIGTERM) };

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(grace_ms);
    while std::time::Instant::now() < deadline {
        if !pid_alive(pid) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    if pid_alive(pid) {
        log::warn!("{name} ignored SIGTERM, sending SIGKILL");
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline && pid_alive(pid) {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    let _ = std::fs::remove_file(store.pid_file(name));
    let _ = std::fs::remove_file(store.console_socket(name));

    if pid_alive(pid) {
        bail!("{name} (pid {pid}) is still alive after SIGKILL");
    }

    log::info!("{name} stopped");
    Ok(())
}

/// A one-line summary of a VM, for `list`.
pub struct Summary {
    pub name: String,
    pub state: State,
    pub hypervisor: String,
    pub memory_mib: u64,
    pub guest_memory_mib: u64,
    pub cpus: u32,
    pub kernel: PathBuf,
}

pub fn summary(store: &Store, name: &str) -> Result<Summary> {
    let vm = store.load(name)?;
    Ok(Summary {
        name: vm.name.clone(),
        state: state(store, name),
        hypervisor: vm.hypervisor.backend.to_string(),
        memory_mib: vm.memory_mib,
        guest_memory_mib: vm.guest_memory_mib(),
        cpus: vm.cpus,
        kernel: vm.boot.kernel.clone(),
    })
}

/// Every VM in the store, with its state.
pub fn list(store: &Store) -> Result<Vec<Summary>> {
    let mut out = Vec::new();
    for name in store.list()? {
        match summary(store, &name) {
            Ok(s) => out.push(s),
            Err(e) => log::warn!("{name}: {e}"),
        }
    }
    Ok(out)
}

/// Describe what the machine offers, for `librevm info`.
pub struct HostInfo {
    pub hypervisors: Vec<Detected>,
    pub under_hypervisor: bool,
    pub crosvm: Option<PathBuf>,
    pub crosvm_error: Option<String>,
}

pub fn host_info() -> HostInfo {
    let (crosvm, crosvm_error) = match backend::find_crosvm(None) {
        Ok(p) => (Some(p), None),
        Err(e) => (None, Some(e.to_string())),
    };
    HostInfo {
        hypervisors: backend::detect_all(),
        under_hypervisor: backend::running_under_hypervisor(),
        crosvm,
        crosvm_error,
    }
}

/// The default protection mode to suggest for a new VM on this machine.
pub fn suggested_protection() -> Protection {
    // On Gunyah the unprotected memory path is refused by the resource manager on
    // at least one shipping device, while the lend path works. Default to the one
    // that starts.
    Protection::WithoutFirmware
}
