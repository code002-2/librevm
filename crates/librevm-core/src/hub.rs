//! The per-VM console hub.
//!
//! crosvm's `unix-stream` serial type is a *client*: it connects to a path that
//! already has a listener. Something therefore has to hold that listener for as
//! long as the guest runs, and it cannot be whatever asked for the start, because
//! that process returns immediately.
//!
//! The hub is that process. It is started detached, and it:
//!
//!   1. listens on the VM's console socket,
//!   2. spawns crosvm, which connects to that socket as a client,
//!   3. relays bytes in both directions,
//!   4. broadcasts guest output to any number of attached clients, replaying
//!      recent history so attaching to a booted guest is not a blank screen,
//!   5. records crosvm's pid so `status` and `stop` can find it, and
//!   6. answers the terminal queries the guest's shell waits on.
//!
//! This lives in the core crate so the CLI and the GUI share one implementation.

use crate::config::Protection;
use crate::store::Store;
use crate::vm;
use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

/// How much recent guest output to replay to a newly attached client.
const REPLAY_BYTES: usize = 64 * 1024;

/// A terminal that has no cursor to report can be asked for its position and will
/// never answer, which is enough to wedge an interactive shell. busybox does this
/// on startup, so the hub answers on the guest's behalf.
const DSR_QUERY: &[u8] = b"\x1b[6n";
const DSR_REPLY: &[u8] = b"\x1b[1;1R";

/// A shared ring of recent guest output.
#[derive(Default)]
struct History {
    buf: Vec<u8>,
}

impl History {
    fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        if self.buf.len() > REPLAY_BYTES {
            let excess = self.buf.len() - REPLAY_BYTES;
            self.buf.drain(..excess);
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        self.buf.clone()
    }
}

fn answer_cursor_queries(chunk: &[u8]) -> bool {
    chunk.windows(DSR_QUERY.len()).any(|w| w == DSR_QUERY)
}

/// Send guest output to every attached client.
///
/// The write is made non-blocking per call with `MSG_DONTWAIT` rather than by
/// putting the socket itself in non-blocking mode: the same descriptor is also
/// read by the client's input thread, and flipping O_NONBLOCK on it would make
/// that thread fail with EAGAIN instead of waiting for the operator to type.
///
/// A client that is not reading -- the GUI shows the log file and only sends
/// input -- simply misses these bytes rather than stalling everyone. A client
/// whose connection is gone is dropped.
fn broadcast_to_clients(clients: &Arc<Mutex<Vec<UnixStream>>>, data: &[u8]) {
    clients.lock().unwrap().retain(|c| write_nonblocking(c, data));
}

/// Write as much as the peer will take without ever blocking.
///
/// Returns false when the connection is gone and the client should be dropped.
/// A full buffer is not a failure: the client simply misses these bytes.
fn write_nonblocking(sock: &UnixStream, data: &[u8]) -> bool {
    use std::os::unix::io::AsRawFd;

    let mut sent = 0usize;
    while sent < data.len() {
        // SAFETY: `sock` owns the descriptor for the duration of the call, and the
        // pointer and remaining length both describe the tail of `data`.
        let n = unsafe {
            libc::send(
                sock.as_raw_fd(),
                data[sent..].as_ptr() as *const libc::c_void,
                data.len() - sent,
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        if n > 0 {
            sent += n as usize;
            continue;
        }
        if n == 0 {
            continue;
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            // buffer full, or interrupted: keep the client, drop the rest
            Some(libc::EAGAIN) | Some(libc::EWOULDBLOCK) | Some(libc::EINTR) => return true,
            // anything else means the peer is gone
            _ => return false,
        }
    }
    true
}

/// Crashing with SIGPIPE when a client disappears mid-write would take the whole
/// VM down with it, so it is ignored process-wide.
pub fn ignore_sigpipe() {
    // SAFETY: setting a signal disposition to SIG_IGN is always safe.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

/// The program that can host a hub: it must accept `--root`, `--run` and the
/// hidden `__hub` subcommand.
///
/// The CLI has all three. The GUI does not, so a GUI asks for the CLI instead of
/// re-executing itself. A `librevm` sitting beside the caller wins, then one on
/// PATH, and only then does the caller try to host the hub itself.
fn hub_program(self_exe: &std::path::Path) -> std::path::PathBuf {
    if let Some(dir) = self_exe.parent() {
        let sibling = dir.join("librevm");
        if sibling.is_file() {
            return sibling;
        }
    }
    for dir in ["/usr/local/bin", "/usr/bin"] {
        let p = std::path::Path::new(dir).join("librevm");
        if p.is_file() {
            return p;
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let p = std::path::Path::new(dir).join("librevm");
            if p.is_file() {
                return p;
            }
        }
    }
    self_exe.to_path_buf()
}

/// Start a VM detached, under a hub that will own its console socket.
///
/// `self_exe` is normally `std::env::current_exe()`. It is only used if no
/// `librevm` binary can be found, since only that program understands the hidden
/// `__hub` subcommand.
///
/// Blocks briefly until the hub has recorded crosvm's pid, so that a `status`
/// immediately afterwards is meaningful.
pub fn spawn(store: &Store, name: &str, self_exe: &std::path::Path) -> Result<i32> {
    let config = store.load(name)?;
    config.validate()?;

    match vm::state(store, name) {
        vm::State::Running { pid } => bail!("{name} is already running (pid {pid})"),
        vm::State::Stale { pid } => {
            log::warn!("{name}: clearing stale pid {pid}");
            let _ = std::fs::remove_file(store.pid_file(name));
        }
        vm::State::Stopped => {}
    }

    // Resolve the plan here so configuration problems surface in the caller's
    // terminal rather than silently in a detached process's log.
    let plan = vm::plan(store, &config, true)?;
    log::info!("{name} will run on {}", plan.hypervisor);

    store.create_dirs()?;

    let hub_log = store.hub_log(name);
    if let Some(dir) = hub_log.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    let log = std::fs::File::create(&hub_log)
        .with_context(|| format!("creating {}", hub_log.display()))?;

    let program = hub_program(self_exe);
    log::debug!("{name}: hub will be hosted by {}", program.display());

    let mut cmd = Command::new(&program);
    cmd.arg("--root")
        .arg(store.root())
        .arg("--run")
        .arg(store.run_dir())
        .arg("__hub")
        .arg(name)
        // The hub's log is the only record of what happened to a machine that was
        // started detached, so keep it at debug rather than the default error.
        .env("RUST_LOG", "librevm_core=debug,librevm=debug,warn")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        // its own session, so it survives the caller's terminal going away
        .process_group(0);

    let child = cmd.spawn().context("starting the console hub")?;
    let hub_pid = child.id();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut guest_pid = None;
    while std::time::Instant::now() < deadline {
        if let Some(p) = vm::read_pid(store, name) {
            guest_pid = Some(p);
            break;
        }
        if !vm::pid_alive(hub_pid as i32) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    match guest_pid {
        Some(pid) => {
            log::info!("{name} started (guest pid {pid}, hub pid {hub_pid})");
            Ok(pid)
        }
        None => {
            let text = std::fs::read_to_string(&hub_log).unwrap_or_default();
            let tail: Vec<&str> = text.lines().rev().take(10).collect();
            let mut msg = format!("{name} did not start");
            if !tail.is_empty() {
                msg.push_str("; the hub said:\n");
                for line in tail.iter().rev() {
                    msg.push_str("  ");
                    msg.push_str(line);
                    msg.push('\n');
                }
            }
            msg.push_str(&format!("  hub log: {}", hub_log.display()));
            bail!(msg)
        }
    }
}

/// Run the hub for a VM. Does not return until the guest is gone.
pub fn run(store: &Store, name: &str) -> Result<()> {
    ignore_sigpipe();

    let config = store.load(name)?;
    config.validate()?;

    // Set when the touch forwarder is started, so it can be told to stop when the machine
    // does. Without it the thread would keep reading the panel after the guest was gone.
    let touch_stop: Arc<Mutex<Option<Arc<std::sync::atomic::AtomicBool>>>> =
        Arc::new(Mutex::new(None));

    let plan = vm::plan(store, &config, true)?;
    let socket_path = store.console_socket(name);

    let _ = std::fs::remove_file(&socket_path);
    if let Some(dir) = socket_path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("listening on {}", socket_path.display()))?;

    // The socket has to be group writable, or the window cannot connect to it.
    //
    // `bind` creates it with 0777 minus the umask, which for a root process is usually 0755
    // -- group read and execute but not write. Connecting to a Unix socket is a write, so
    // the desktop user, who is in the `librevm` group precisely so it can do this, is
    // refused with EACCES. A `chmod` after the fact in the unit does not help either: the
    // socket does not exist until a hub is started, which is long after the unit's own
    // start-up commands have run.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::Permissions::from_mode(0o660);
        if let Err(e) = std::fs::set_permissions(&socket_path, mode) {
            log::warn!(
                "{name}: could not make {} group writable ({e}); the window may not be able \
                 to reach the console",
                socket_path.display()
            );
        }
    }

    log::info!("{name}: console listening on {}", socket_path.display());

    let log_path = store.console_log(name);
    if let Some(dir) = log_path.parent() {
        std::fs::create_dir_all(dir)?;
    }

    // Bring the host side of the guest's network up before crosvm starts, because the tap
    // has to exist and be addressed before crosvm opens it by name.
    if config.network.enabled {
        match crate::net::setup(config.network.index) {
            Ok(guest) => {
                log::info!(
                    "{name}: tap {} up, guest should use {}/{} via {}",
                    config.network.tap,
                    guest.address,
                    guest.prefix,
                    guest.gateway
                );
            }
            // A machine without a network is still worth starting, so this is not fatal.
            Err(e) => log::warn!("{name}: could not set up the guest network: {e:#}"),
        }
    }

    let mut child = Command::new(&plan.argv[0])
        .args(&plan.argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {}", plan.argv[0]))?;

    let pid = child.id() as i32;
    std::fs::write(store.pid_file(name), format!("{pid}\n"))
        .with_context(|| format!("writing {}", store.pid_file(name).display()))?;
    log::info!("{name}: crosvm pid {pid}");

    // The touchscreen, when the machine asks for it and the host has one.
    //
    // This is served from here rather than from crosvm because the direct path grabs the
    // device exclusively and the desktop is already using it; see `touch.rs`. The socket is
    // the one `build_command` told crosvm to read.
    if config.gpu.enabled && config.gpu.touch {
        match crate::touch::find_touchscreen() {
            Some(device) => {
                let socket = crate::touch::socket_path(store.run_dir(), name);
                let stopping = Arc::new(std::sync::atomic::AtomicBool::new(false));
                *touch_stop.lock().unwrap() = Some(stopping.clone());
                let path = device.path.clone();
                let what = device.name.clone();
                let size = crate::touch::fallback_size(&config);
                let area = config.gpu.touch_area;
                // The machine's name is borrowed, so the thread gets its own copy.
                let machine = name.to_string();
                std::thread::spawn(move || {
                    log::info!("{machine}: forwarding {what} ({})", path.display());
                    if let Err(e) = crate::touch::serve(&path, &socket, size, area, stopping) {
                        log::warn!("{machine}: the touch forwarder stopped: {e:#}");
                    }
                });
            }
            None => log::warn!(
                "{name}: asked for a touchscreen but the host does not report one; the guest \
                 will have a keyboard and a pointer but no touch"
            ),
        }
    }

    let clients: Arc<Mutex<Vec<UnixStream>>> = Arc::new(Mutex::new(Vec::new()));
    let history: Arc<Mutex<History>> = Arc::new(Mutex::new(History::default()));

    if let Some(out) = child.stdout.take() {
        broadcast(out, clients.clone(), history.clone(), Some(log_path.clone()), "out");
    }
    if let Some(err) = child.stderr.take() {
        broadcast(err, clients.clone(), history.clone(), Some(log_path.clone()), "err");
    }

    // crosvm connects once, first; everything after that is a human.
    let crosvm_console = accept_console(&listener, name);
    match crosvm_console {
        Some(_) => log::info!("{name}: crosvm connected to the console socket"),
        None => log::error!(
            "{name}: crosvm never connected to {}; input will go nowhere",
            socket_path.display()
        ),
    }

    let console_in: Arc<Mutex<Option<UnixStream>>> =
        Arc::new(Mutex::new(crosvm_console.as_ref().and_then(|c| c.try_clone().ok())));
    if console_in.lock().unwrap().is_none() {
        log::error!("{name}: no writable console handle; the guest cannot be typed into");
    }

    {
        let clients = clients.clone();
        let history = history.clone();
        let console_in = console_in.clone();
        let listener = listener.try_clone()?;
        let socket_for_cleanup = socket_path.clone();
        let name = name.to_string();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let s = match stream {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!("accept failed on {}: {e}", socket_for_cleanup.display());
                        break;
                    }
                };
                log::info!("client attached to {name}");

                // Start forwarding input first. A client that sends a line and
                // disconnects immediately -- which is exactly what a GUI does when
                // the operator presses Enter -- must not lose that line to a failed
                // history replay.
                if let Ok(mut write_side) = s.try_clone() {
                    let console_in = console_in.clone();
                    let name = name.clone();
                    std::thread::spawn(move || {
                        let mut buf = [0u8; 1024];
                        loop {
                            match write_side.read(&mut buf) {
                                Ok(0) => {
                                    log::debug!("{name}: an input client disconnected");
                                    break;
                                }
                                Ok(n) => {
                                    log::debug!(
                                        "{name}: forwarding {n} input bytes: {:?}",
                                        String::from_utf8_lossy(&buf[..n])
                                    );
                                    let mut guard = console_in.lock().unwrap();
                                    match guard.as_mut() {
                                        Some(c) => {
                                            if let Err(e) = c.write_all(&buf[..n]) {
                                                log::warn!("{name}: could not write to the guest: {e}");
                                                break;
                                            }
                                            let _ = c.flush();
                                            log::debug!("{name}: wrote {n} bytes to the guest");
                                        }
                                        None => {
                                            log::warn!("{name}: no console connection to write to");
                                            break;
                                        }
                                    }
                                }
                                Err(e) => {
                                    log::debug!("{name}: input read failed: {e}");
                                    break;
                                }
                            }
                        }
                    });
                }

                // Replay recent output so attaching to a booted guest shows its
                // history rather than an empty screen. Best effort: sent with the
                // same non-blocking write as live output, so a client that is not
                // reading cannot stall the hub, and the socket stays blocking so
                // the input thread can wait for a keystroke.
                {
                    let replayed = history.lock().unwrap().snapshot();
                    if !replayed.is_empty() {
                        write_nonblocking(&s, &replayed);
                    }
                }

                clients.lock().unwrap().push(s);
            }
        });
    }

    if let Some(mut console) = crosvm_console {
        let clients_for_out = clients.clone();
        let history_for_out = history.clone();
        let console_in_for_out = console_in.clone();
        let log_for_out = log_path.clone();
        let name = name.to_string();
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_for_out)
                .ok();
            loop {
                match console.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Some(f) = log.as_mut() {
                            let _ = f.write_all(&buf[..n]);
                            let _ = f.flush();
                        }
                        // Behave like a terminal and answer the cursor query, or
                        // an interactive shell never runs anything.
                        if answer_cursor_queries(&buf[..n]) {
                            if let Some(c) = console_in_for_out.lock().unwrap().as_mut() {
                                let _ = c.write_all(DSR_REPLY);
                                let _ = c.flush();
                            }
                        }
                        history_for_out.lock().unwrap().push(&buf[..n]);
                        broadcast_to_clients(&clients_for_out, &buf[..n]);
                    }
                    Err(_) => break,
                }
            }
            log::info!("{name}: guest console closed");
        });
        let _ = reader.join();
    }

    let status = child.wait().ok();
    let _ = std::fs::remove_file(store.pid_file(name));
    let _ = std::fs::remove_file(&socket_path);
    log::info!("{name}: crosvm exited ({status:?})");
    Ok(())
}

fn accept_console(listener: &UnixListener, name: &str) -> Option<UnixStream> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        match listener.accept() {
            Ok((stream, _)) => {
                log::info!("{name}: crosvm attached to the console");
                return Some(stream);
            }
            Err(e) => {
                log::warn!("{name}: accept failed: {e}");
                return None;
            }
        }
    }
    log::warn!("{name}: no console connection arrived within 20s");
    None
}

fn broadcast<R: Read + Send + 'static>(
    mut src: R,
    clients: Arc<Mutex<Vec<UnixStream>>>,
    history: Arc<Mutex<History>>,
    log_path: Option<std::path::PathBuf>,
    tag: &'static str,
) {
    std::thread::spawn(move || {
        let mut log = log_path.and_then(|p| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .ok()
        });
        let mut buf = [0u8; 4096];
        loop {
            match src.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Some(f) = log.as_mut() {
                        let _ = f.write_all(&buf[..n]);
                        let _ = f.flush();
                    }
                    history.lock().unwrap().push(&buf[..n]);
                    broadcast_to_clients(&clients, &buf[..n]);
                }
                Err(_) => break,
            }
        }
        log::debug!("broadcast thread for {tag} finished");
    });
}

/// Attach the current terminal to a running VM's console.
///
/// Detaching leaves the guest running, because the process is not ours.
pub fn attach(store: &Store, name: &str) -> Result<()> {
    let socket = store.console_socket(name);

    if let vm::State::Stopped | vm::State::Stale { .. } = vm::state(store, name) {
        bail!(
            "{name} is not running. Start it with `librevm start {name}`, \
             or use `librevm run {name}` for a foreground console"
        );
    }

    let stream = UnixStream::connect(&socket)
        .with_context(|| format!("connecting to {}", socket.display()))?;
    eprintln!("attached to {name}; Ctrl-C detaches, the guest keeps running");

    let mut to_guest = stream.try_clone()?;
    let mut from_guest = stream;

    let saved = RawGuard::install();

    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match from_guest.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let mut out = std::io::stdout();
                    if out.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    let _ = out.flush();
                }
                Err(_) => break,
            }
        }
    });

    let mut buf = [0u8; 4096];
    loop {
        match std::io::stdin().read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if to_guest.write_all(&buf[..n]).is_err() {
                    break;
                }
                let _ = to_guest.flush();
            }
            Err(_) => break,
        }
    }

    let _ = reader.join();
    drop(saved);
    Ok(())
}

/// Restores the terminal when dropped.
struct RawGuard(Option<libc::termios>);

impl RawGuard {
    fn install() -> Self {
        if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
            return RawGuard(None);
        }
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut t) } != 0 {
            return RawGuard(None);
        }
        let saved = t;
        unsafe { libc::cfmakeraw(&mut t) };
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t) };
        RawGuard(Some(saved))
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        if let Some(t) = self.0 {
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t) };
        }
    }
}

/// Does this VM's configuration need the hub at all?
///
/// A foreground run writes the console to our own stdout and needs no socket.
pub fn needs_hub(config: &crate::config::VmConfig) -> bool {
    config.hypervisor.protection != Protection::None || true
}
