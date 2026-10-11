//! Hypervisor detection and crosvm command construction.
//!
//! Everything in here exists because of something that was measured on real
//! hardware. The comments record why each flag is present, so the reasoning
//! survives.

use crate::config::{DiskBus, Hypervisor, Protection, SerialBackend, VmConfig};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// A hypervisor that this machine can actually use.
///
/// Serializable because the daemon hands these to the window over the bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Detected {
    pub hypervisor: Hypervisor,
    /// The device node it is reached through, when there is one.
    pub node: Option<PathBuf>,
    /// A short human-readable note, e.g. the version the driver reported.
    pub detail: String,
}

impl std::fmt::Display for Detected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.hypervisor)?;
        if let Some(node) = &self.node {
            write!(f, " ({})", node.display())?;
        }
        if !self.detail.is_empty() {
            write!(f, " - {}", self.detail)?;
        }
        Ok(())
    }
}

const GUNYAH_NODE: &str = "/dev/gunyah";
const KVM_NODE: &str = "/dev/kvm";
const GENIEZONE_NODE: &str = "/dev/gzvm";

/// Look for the hypervisors this machine offers, best first.
///
/// Gunyah is listed first on purpose: on a device where it owns EL2 there is no
/// KVM to fall back to anyway, and where both exist the hypervisor that was
/// already running before Linux booted is the one with the firmware's device
/// tree describing real hardware.
pub fn detect_all() -> Vec<Detected> {
    let mut found = Vec::new();

    if Path::new(GUNYAH_NODE).exists() {
        let detail = match std::fs::read_to_string("/proc/sys/kernel/osrelease") {
            Ok(_) => "Qualcomm hypervisor already resident".to_string(),
            Err(_) => String::new(),
        };
        found.push(Detected {
            hypervisor: Hypervisor::Gunyah,
            node: Some(PathBuf::from(GUNYAH_NODE)),
            detail,
        });
    }

    if Path::new(KVM_NODE).exists() {
        found.push(Detected {
            hypervisor: Hypervisor::Kvm,
            node: Some(PathBuf::from(KVM_NODE)),
            detail: String::new(),
        });
    }

    if Path::new(GENIEZONE_NODE).exists() {
        found.push(Detected {
            hypervisor: Hypervisor::Geniezone,
            node: Some(PathBuf::from(GENIEZONE_NODE)),
            detail: String::new(),
        });
    }

    found
}

/// True when the kernel booted under a hypervisor rather than at EL2 itself.
pub fn running_under_hypervisor() -> bool {
    std::fs::read_to_string("/proc/device-tree/hypervisor/compatible")
        .map(|s| s.contains("gunyah"))
        .unwrap_or(false)
}

/// Is Gunyah actually usable, as opposed to merely present?
///
/// `/dev/gunyah` on its own is not enough: a stale device node survives a module
/// reload, and crosvm needs the resource manager behind it. The module list is the
/// honest check, because the node only appears once the driver has bound.
pub fn gunyah_ready() -> bool {
    if !Path::new(GUNYAH_NODE).exists() {
        return false;
    }
    modules_loaded(&[
        "gunyah",
        "gunyah_hypercall",
        "gunyah_rsc_mgr",
    ])
}

/// Read `/proc/modules` and report whether every name is loaded.
fn modules_loaded(names: &[&str]) -> bool {
    let Ok(text) = std::fs::read_to_string("/proc/modules") else {
        return false;
    };
    names.iter().all(|want| {
        text.lines()
            .any(|line| line.split_whitespace().next() == Some(*want))
    })
}

/// Find a program that can install the Gunyah modules.
///
/// The GUI offers to run this when Gunyah is missing. Three places are checked, in
/// order of how much they can do: the full installer, the fetch-and-install helper,
/// and finally whatever sits beside this binary.
pub fn gunyah_installer() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = vec![
        PathBuf::from("/usr/local/sbin/install-gunyah"),
        PathBuf::from("/usr/local/bin/install-gunyah"),
        PathBuf::from("/usr/local/libexec/librevm-get-gunyah"),
    ];

    // a copy shipped next to the binaries, for a run from a build tree
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("install-gunyah"));
            candidates.push(dir.join("librevm-get-gunyah"));
            candidates.push(dir.join("scripts/install-gunyah.sh"));
            candidates.push(dir.join("scripts/librevm-get-gunyah.sh"));
        }
    }

    candidates.into_iter().find(|p| p.is_file())
}

/// Find the crosvm binary.
pub fn find_crosvm(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = override_path {
        if p.is_file() {
            return Ok(p.to_path_buf());
        }
        bail!("configured crosvm {} is not a file", p.display());
    }

    for candidate in [
        "/usr/bin/crosvm",
        "/usr/local/bin/crosvm",
        "/root/crosvm/target/release/crosvm",
        "/opt/crosvm/crosvm",
    ] {
        let p = Path::new(candidate);
        if p.is_file() {
            return Ok(p.to_path_buf());
        }
    }

    // last resort: the usual PATH entries
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let p = Path::new(dir).join("crosvm");
            if p.is_file() {
                return Ok(p);
            }
        }
    }

    // A crosvm under a directory this user cannot read looks exactly like no crosvm
    // at all, and `/root` is mode 0700. Say so rather than sending the operator
    // looking for a missing file.
    let unreadable: Vec<&str> = [
        "/root/crosvm/target/release/crosvm",
        "/root/crosvm",
    ]
    .into_iter()
    .filter(|c| Path::new(c).exists())
    .collect();
    if !unreadable.is_empty() {
        bail!(
            "no crosvm binary found in the usual places, but {} exists and this \
             user cannot read it (running as uid {}). Either run librevm as root, \
             or copy crosvm somewhere world-readable such as /usr/local/bin/crosvm",
            unreadable.join(" and "),
            unsafe { libc::geteuid() }
        );
    }

    bail!("no crosvm binary found; set hypervisor.crosvm in the vm definition")
}

/// Resolve `Hypervisor::Auto` against what the machine offers.
pub fn resolve(configured: Hypervisor) -> Result<Detected> {
    let all = detect_all();
    if all.is_empty() {
        bail!(
            "no hypervisor device found: looked for {GUNYAH_NODE}, {KVM_NODE} and {GENIEZONE_NODE}"
        );
    }
    match configured {
        Hypervisor::Auto => Ok(all.into_iter().next().unwrap()),
        want => all
            .into_iter()
            .find(|d| d.hypervisor == want)
            .with_context(|| format!("{want} was requested but its device node is not present")),
    }
}

/// How the console should be attached.
#[derive(Debug, Clone)]
pub enum ConsoleSink {
    /// Write to our own stdout. Right for a foreground `librevm console` run.
    Stdout,
    /// Serve on a Unix stream socket, so clients can attach and detach while the
    /// guest keeps running.
    UnixStream(PathBuf),
}

/// Build the crosvm `run` argument list for a VM.
///
/// The result is the full argv *after* the binary path.
pub fn build_command(
    vm: &VmConfig,
    resolved: &Detected,
    crosvm: &Path,
    console: &ConsoleSink,
) -> Result<Vec<String>> {
    vm.validate()?;

    let mut args: Vec<String> = vec![
        crosvm.display().to_string(),
        "run".to_string(),
        "--hypervisor".to_string(),
        resolved.hypervisor.to_string(),
    ];

    // Protection decides which memory ioctl crosvm uses. On Gunyah:
    //   None             -> SET_USER_MEM_REGION
    //   WithoutFirmware  -> the LEND ioctl, no firmware
    //   WithFirmware     -> the LEND ioctl plus a firmware image
    // Some devices only accept the LEND path, so this matters more than it looks.
    match vm.hypervisor.protection {
        Protection::None => {}
        Protection::WithoutFirmware => {
            args.push("--protected-vm-without-firmware".to_string());
        }
        Protection::WithFirmware => {
            let fw = vm
                .hypervisor
                .firmware
                .as_ref()
                .context("protection = with-firmware without a firmware path")?;
            // The firmware path is the flag's value; there is no separate --pvm-fw.
            args.push("--protected-vm-with-firmware".to_string());
            args.push(fw.display().to_string());
        }
    }

    // Memory. gunyah requires a static swiotlb and crosvm subtracts it from
    // --mem without checking, so both have to be passed explicitly.
    args.push("--mem".to_string());
    args.push(vm.memory_mib.to_string());

    if vm.hypervisor.protection != Protection::None {
        args.push("--swiotlb".to_string());
        args.push(vm.hypervisor.swiotlb_mib.to_string());
    }

    args.push("--cpus".to_string());
    args.push(vm.cpus.to_string());

    if vm.hypervisor.disable_sandbox {
        args.push("--disable-sandbox".to_string());
    }

    // Turn off the devices that cannot work here.
    //
    // On this class of device virtio-balloon kills crosvm with SIGBUS the moment the
    // guest's driver probes it, and crosvm adds one by default. It is not a device
    // anybody asked for -- it is a memory-reclaiming helper -- so declining it costs
    // nothing and is the difference between a machine that boots and one that does not.
    //
    // This was found the hard way. An earlier version of this file disabled a fixed set
    // of defaults on the strength of one observation, concluded that virtio as a whole
    // was unusable on Gunyah, and wrote that down. It is not: with the balloon gone,
    // virtio-blk mounts and reads and writes, and virtio-net comes up.
    args.push("--no-balloon".to_string());

    // The rest of crosvm's default devices are harmless here, but a VM that is not asked
    // for a random number generator does not need one, and neither does it need an xHCI
    // controller or performance counters.
    args.push("--no-rng".to_string());
    args.push("--no-pmu".to_string());
    args.push("--no-smt".to_string());

    // A virtual GPU, when the machine asks for one.
    //
    // Three things about this are worth knowing, because each cost an afternoon:
    //
    // * crosvm run as the desktop user does not start at all here. Gunyah refuses the VM
    //   with `unknown gh exit reason: 3`, which crosvm reports as an unknown vcpu error --
    //   and it does that whether or not a GPU is involved, so the GPU was blamed for
    //   something it had no part in. Run as root, the same command works.
    // * so the daemon, which is root, runs crosvm, and the display is opened through the
    //   desktop user's Wayland socket. That works because the socket is world-writable and
    //   root is not stopped by the mode of the directory holding it.
    // * the scanout only appears once something in the guest draws. A guest sitting on the
    //   text console has a connected output and no window, which is correct rather than a
    //   fault to chase.
    if vm.gpu.enabled {
        // The display has to be described through `displays`, not through the older
        // `width`/`height` pair. Those still parse, and crosvm warns that they are
        // deprecated -- but a machine configured with them opens the wayland connection,
        // creates no window, and reports nothing wrong, which is a very quiet way to fail.
        //
        // The shape of the value is awkward and was found by bisection:
        //   displays=[[mode=windowed[W,H]]]
        // Two levels of list, and the size attached to the `mode` key rather than to the
        // display. `displays=[[windowed]]` is rejected with "expected '['", and
        // `displays=[[mode=windowed]]` likewise: the size is not optional.
        let mut gpu = format!("backend={}", vm.gpu.backend);
        if vm.gpu.width > 0 && vm.gpu.height > 0 {
            gpu.push_str(&format!(
                ",displays=[[mode=windowed[{},{}]]]",
                vm.gpu.width, vm.gpu.height
            ));
        }
        args.push("--gpu".to_string());
        args.push(gpu);

        // The socket is what selects the Wayland backend. Without it crosvm stays on its
        // default and reports "unsupported by the implementation", which sounds like a
        // missing feature and is really a missing argument. The environment is set where
        // the child is spawned; both are needed.
        if let Some(sock) = crate::vm::wayland_socket(vm) {
            args.push("--wayland-sock".to_string());
            args.push(sock.display().to_string());
        }

        // A window that takes no input is a picture, not a desktop.
        //
        // crosvm forwards nothing from the display window unless asked, so a guest with a
        // desktop running is visible and completely unusable: the pointer moves on the host
        // and never arrives. These two flags are the whole difference.
        args.push("--display-window-keyboard".to_string());
        args.push("--display-window-mouse".to_string());
    }

    // Kernel command line. Force the console parameters the emulated UART needs,
    // without duplicating anything the operator already asked for.
    let cmdline = ensure_console_params(&vm.boot.cmdline);
    args.push("--params".to_string());
    args.push(cmdline);

    // Serial. crosvm's default hardware is an 8250 UART; `earlycon` makes it tell
    // the kernel where that UART is, which is what gets output out of the very
    // first instructions.
    let serial = match &vm.serial.backend {
        // The hub's console socket takes precedence when one is configured, because that
        // is what a client attaches to; otherwise the configured backend is used.
        SerialBackend::Stdout if matches!(console, ConsoleSink::UnixStream(_)) => match console {
            ConsoleSink::UnixStream(path) => {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)
                        .with_context(|| format!("creating {}", dir.display()))?;
                }
                // The socket must already be listening: crosvm's unix-stream serial is
                // a client that connects to this path. Whoever bound it owns it, so it
                // is emphatically not ours to unlink here -- doing so deletes the only
                // name crosvm could have connected to, and the guest then has a console
                // it can write to but that nothing can type into.
                // `input-unix-stream` is what makes the socket carry input as well as
                // output. Without it crosvm uses the socket for guest output only, so
                // the guest can be watched but never typed into -- the operator sees a
                // prompt and every keystroke silently goes nowhere.
                format!(
                    "type=unix-stream,path={},input-unix-stream,hardware=serial,console,earlycon",
                    path.display()
                )
            }
            ConsoleSink::Stdout => unreachable!("guarded by the match arm above"),
        },

        SerialBackend::Stdout => "type=stdout,hardware=serial,console,earlycon".to_string(),

        // A file wants no `console` flag: crosvm treats the presence of a console sink as
        // a request to wire up the input side too, and a file has no input side.
        SerialBackend::File(path) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
            }
            format!(
                "type=file,path={},hardware=serial,console,earlycon",
                path.display()
            )
        }

        // A pty has to exist before crosvm can be told to use it, so one is allocated here
        // and its name logged. crosvm has no way to make one itself, and a pty nobody can
        // find is not worth having.
        SerialBackend::Pty => match allocate_pty() {
            Some(path) => {
                log::info!("console pty: {path}  (attach with: screen {path} 115200)");
                format!("type=file,path={path},hardware=serial,console,earlycon")
            }
            None => {
                log::warn!("could not allocate a pty; falling back to stdout");
                "type=stdout,hardware=serial,console,earlycon".to_string()
            }
        },

        SerialBackend::Unix(path) => format!(
            "type=unix,path={},hardware=serial,console,earlycon",
            path.display()
        ),

        SerialBackend::UnixStream(path) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
            }
            format!(
                "type=unix-stream,path={},input-unix-stream,hardware=serial,console,earlycon",
                path.display()
            )
        }
    };
    args.push("--serial".to_string());
    args.push(serial);

    // pstore: crosvm keeps the guest's kernel log in host memory across a reboot, so a
    // crash that killed the console can still be read afterwards.
    if vm.serial.pstore {
        args.push("--pstore".to_string());
        args.push("path=/tmp/librevm-pstore,size=1048576".to_string());
    }

    if let Some(initrd) = &vm.boot.initramfs {
        args.push("--initrd".to_string());
        args.push(initrd.display().to_string());
    }

    // Disks. The bus decides which crosvm option carries it, and read-only is spelled
    // differently on each. A CDROM is read-only by nature, so it is forced.
    for disk in &vm.disks {
        let ro = disk.readonly || disk.bus == DiskBus::Cdrom;
        match disk.bus {
            DiskBus::Virtio if disk.root => {
                args.push("--rwroot".to_string());
                args.push(format!("{},ro=false,lock=false", disk.path.display()));
            }
            DiskBus::Virtio => {
                let mut spec = disk.path.display().to_string();
                if ro {
                    spec.push_str(",ro=true");
                }
                spec.push_str(",lock=false");
                args.push("--block".to_string());
                args.push(spec);
            }
            DiskBus::Scsi => {
                let mut spec = disk.path.display().to_string();
                if ro {
                    spec.push_str(",ro=true");
                }
                spec.push_str(",lock=false");
                args.push("--scsi-block".to_string());
                args.push(spec);
            }
            DiskBus::Cdrom => {
                args.push("--scsi-block".to_string());
                args.push(format!(
                    "{},ro=true,type=cdrom,lock=false",
                    disk.path.display()
                ));
            }
            DiskBus::Pmem => {
                let mut spec = disk.path.display().to_string();
                if ro {
                    spec.push_str(",ro=true");
                }
                args.push("--pmem".to_string());
                args.push(spec);
            }
        }
    }

    // Shared directories.
    //
    // virtio-fs, not 9p. crosvm's 9p device is declared but not implemented in the tree
    // this targets, and its configuration type rejects unknown fields, so `type=p9` is
    // either refused or silently upgraded. virtio-fs is the one that works, and it also
    // carries the cache setting that makes a shared folder behave the way a person
    // expects.
    for dir in &vm.shared_dirs {
        args.push("--shared-dir".to_string());
        args.push(format!(
            "{}:{}:type=fs:cache={}{}",
            dir.path.display(),
            dir.tag,
            dir.cache.name(),
            if dir.readonly { ":writeback=never" } else { "" }
        ));
    }

    // Networking. The tap is created and addressed by `net::setup` before crosvm starts,
    // so all crosvm needs is its name. No mac: the guest picks a random one, which keeps
    // two machines on the host from colliding.
    if vm.network.enabled {
        args.push("--net".to_string());
        args.push(format!("tap-name={}", vm.network.tap));
    }

    // crosvm takes the kernel as a positional argument.
    args.push(vm.boot.kernel.display().to_string());

    Ok(args)
}

/// Allocate a pseudo-terminal and return the path of its slave side.
///
/// crosvm only writes to a path, so the pty has to exist first. The master side is kept
/// open for the life of this process: with no master the slave has nobody on the other end
/// and crosvm's writes fail, but closing the master also closes the slave, so it is
/// deliberately leaked rather than dropped.
///
/// Nothing here is Android-specific; it is the oldest pty API there is.
#[cfg(unix)]
fn allocate_pty() -> Option<String> {
    use std::ffi::CStr;

    // SAFETY: each call is the plain libc one, and every failure is checked before the
    // result is used. The file descriptor is intentionally not closed -- see above.
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if master < 0 {
            return None;
        }
        if libc::grantpt(master) != 0 || libc::unlockpt(master) != 0 {
            libc::close(master);
            return None;
        }
        let name = libc::ptsname(master);
        if name.is_null() {
            libc::close(master);
            return None;
        }
        let path = CStr::from_ptr(name).to_string_lossy().to_string();

        // The slave has to carry the console's line settings. crosvm sets its own on the
        // path it opens, but a raw pty with echo on would double every keystroke.
        let slave = libc::open(name, libc::O_RDWR | libc::O_NOCTTY);
        if slave >= 0 {
            let mut term: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(slave, &mut term) == 0 {
                libc::cfmakeraw(&mut term);
                libc::tcsetattr(slave, libc::TCSANOW, &term);
            }
            // Left open on purpose for the same reason as the master.
        }

        Some(path)
    }
}

#[cfg(not(unix))]
fn allocate_pty() -> Option<String> {
    None
}

/// Make sure the command line carries a console and earlycon.
fn ensure_console_params(cmdline: &str) -> String {
    let mut out: Vec<String> = cmdline.split_whitespace().map(|s| s.to_string()).collect();

    let has_console = out.iter().any(|a| a.starts_with("console="));
    let has_earlycon = out.iter().any(|a| a == "earlycon" || a.starts_with("earlycon="));

    if !has_console {
        out.push("console=ttyS0".to_string());
    }
    if !has_earlycon {
        out.push("earlycon".to_string());
    }

    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;

    fn vm() -> VmConfig {
        VmConfig {
            name: "t".into(),
            cpus: 2,
            memory_mib: 128,
            hypervisor: HypervisorConfig::default(),
            boot: BootConfig {
                kernel: PathBuf::from("/bin/sh"),
                initramfs: None,
                cmdline: "console=ttyS0 earlycon".into(),
            },
            console: ConsoleConfig::default(),
            description: None,
        }
    }

    fn det() -> Detected {
        Detected {
            hypervisor: Hypervisor::Gunyah,
            node: Some(PathBuf::from(GUNYAH_NODE)),
            detail: String::new(),
        }
    }

    #[test]
    fn command_has_the_gunyah_essentials() {
        let args = build_command(
            &vm(),
            &det(),
            Path::new("/usr/bin/crosvm"),
            &ConsoleSink::Stdout,
        )
        .unwrap();
        let joined = args.join(" ");

        assert!(joined.contains("--hypervisor gunyah"));
        assert!(joined.contains("--protected-vm-without-firmware"));
        assert!(joined.contains("--mem 128"));
        assert!(joined.contains("--swiotlb 64"));
        assert!(joined.contains("--cpus 2"));
        assert!(joined.contains("earlycon"));
        // the kernel path is positional and comes last
        assert_eq!(args.last().unwrap(), "/bin/sh");
    }

    #[test]
    fn unprotected_omits_swiotlb() {
        let mut v = vm();
        v.hypervisor.protection = Protection::None;
        let args = build_command(&v, &det(), Path::new("/c"), &ConsoleSink::Stdout).unwrap();
        assert!(!args.join(" ").contains("--swiotlb"));
    }

    #[test]
    fn with_firmware_passes_the_path_as_the_flag_value() {
        let mut v = vm();
        v.hypervisor.protection = Protection::WithFirmware;
        v.hypervisor.firmware = Some(PathBuf::from("/fw.bin"));
        let args = build_command(&v, &det(), Path::new("/c"), &ConsoleSink::Stdout).unwrap();
        let i = args
            .iter()
            .position(|a| a == "--protected-vm-with-firmware")
            .unwrap();
        assert_eq!(args[i + 1], "/fw.bin");
        assert!(!args.iter().any(|a| a == "--pvm-fw"));
    }

    #[test]
    fn console_defaults_are_added_only_once() {
        assert_eq!(ensure_console_params(""), "console=ttyS0 earlycon");
        assert_eq!(
            ensure_console_params("console=ttyAMA0 earlycon loglevel=7"),
            "console=ttyAMA0 earlycon loglevel=7"
        );
        assert_eq!(
            ensure_console_params("quiet"),
            "quiet console=ttyS0 earlycon"
        );
    }

    #[test]
    fn unix_stream_console_uses_a_socket() {
        let args = build_command(
            &vm(),
            &det(),
            Path::new("/c"),
            &ConsoleSink::UnixStream(PathBuf::from("/tmp/nope/x.sock")),
        )
        .unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("type=unix-stream,path=/tmp/nope/x.sock"));
    }
}
