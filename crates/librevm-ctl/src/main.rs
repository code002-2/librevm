//! `librevm` - the command line front end.
//!
//! Everything it does goes through librevm-core, so the daemon and the GUI get the
//! same behaviour.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use librevm_core::config::{
    BootConfig, ConsoleConfig, DiskConfig, Hypervisor, HypervisorConfig, Protection,
    SharedDirConfig, VmConfig,
};
use librevm_core::disk;
use librevm_core::hub;
use librevm_core::store::Store;
use librevm_core::vm::{self, State};
use std::io::Write;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "librevm",
    version,
    about = "Manage virtual machines, with first-class Gunyah support",
    long_about = "librevm creates, starts, inspects and stops virtual machines.\n\n\
                  It drives crosvm and picks a hypervisor that actually exists on \
                  the machine, preferring Gunyah on devices where Qualcomm's \
                  hypervisor already owns EL2."
)]
struct Cli {
    /// Where VM definitions live. Overrides LIBREVM_ROOT.
    #[arg(long, env = "LIBREVM_ROOT", global = true)]
    root: Option<PathBuf>,

    /// Where runtime files live. Overrides LIBREVM_RUN.
    #[arg(long, env = "LIBREVM_RUN", global = true)]
    run: Option<PathBuf>,

    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Show what this machine can do.
    Info,

    /// List the VMs in the store.
    List,

    /// Create a VM definition.
    Create {
        /// Name for the new VM.
        name: String,

        /// Kernel image to boot.
        #[arg(long)]
        kernel: PathBuf,

        /// Optional initramfs.
        #[arg(long)]
        initramfs: Option<PathBuf>,

        /// Kernel command line.
        #[arg(long)]
        cmdline: Option<String>,

        /// Guest memory in MiB, before the swiotlb window is taken out.
        #[arg(long, default_value_t = 128)]
        memory: u64,

        /// Size of the virtio swiotlb window in MiB.
        #[arg(long, default_value_t = 64)]
        swiotlb: u64,

        /// Number of vCPUs.
        #[arg(long, default_value_t = 1)]
        cpus: u32,

        /// Which hypervisor to use.
        #[arg(long, value_enum, default_value_t = HypervisorArg::Auto)]
        hypervisor: HypervisorArg,

        /// Memory isolation mode.
        #[arg(long, value_enum, default_value_t = ProtectionArg::WithoutFirmware)]
        protection: ProtectionArg,

        /// Protected VM firmware, for --protection with-firmware.
        #[arg(long)]
        firmware: Option<PathBuf>,

        /// Path to the crosvm binary, if it is somewhere unusual.
        #[arg(long)]
        crosvm: Option<PathBuf>,

        /// Free-form description.
        #[arg(long)]
        description: Option<String>,
    },

    /// Print a VM's definition.
    Show { name: String },

    /// Print the crosvm command a VM would run, without running it.
    Plan { name: String },

    /// Show whether a VM is running.
    Status { name: String },

    /// Start a VM in the background and return.
    Start { name: String },

    /// Run a VM in the foreground with its console attached.
    Run { name: String },

    /// Stop a running VM.
    Stop {
        name: String,

        /// How long to wait for it to go away on its own, in milliseconds.
        #[arg(long, default_value_t = 5000)]
        grace: u64,
    },

    /// Attach to a running VM's console.
    Console { name: String },

    /// Delete a VM and its files.
    Delete {
        name: String,

        /// Do not ask.
        #[arg(short, long)]
        force: bool,
    },

    /// Attach or detach a disk.
    Disk {
        #[command(subcommand)]
        action: DiskCommand,
    },

    /// Share a directory with a VM.
    Share {
        #[command(subcommand)]
        action: ShareCommand,
    },

    /// Internal: the per-VM console hub. Started detached by `start`.
    #[command(hide = true, name = "__hub")]
    Hub { name: String },
}

#[derive(Subcommand, Debug)]
enum DiskCommand {
    /// Create an image, optionally with a filesystem in it.
    Create {
        /// Where to put it.
        path: PathBuf,

        /// How big, with an optional suffix: 512M, 8G, 1T.
        #[arg(short, long, default_value = "4G")]
        size: String,

        /// raw or sparse. Raw images are made sparse on disk anyway, so this only
        /// matters for tools that expect the Android format.
        #[arg(short, long, default_value = "raw")]
        format: String,

        /// Also make a filesystem inside it: ext4, ext2 or vfat. Needs root,
        /// because it attaches the image to a loop device.
        #[arg(long)]
        fs: Option<String>,
    },

    /// List the images in a directory.
    List {
        /// Defaults to the store's images directory.
        dir: Option<PathBuf>,
    },

    /// Show what an image is.
    Info { path: PathBuf },

    /// Grow or shrink an image.
    Resize {
        path: PathBuf,

        /// The new size, with an optional suffix.
        size: String,

        /// Allow shrinking. The filesystem inside must have been shrunk first.
        #[arg(long)]
        force: bool,
    },

    /// Copy an image. Holes are preserved, so an empty 4 GiB disk does not become
    /// 4 GiB of host disk.
    Clone { from: PathBuf, to: PathBuf },

    /// Delete an image.
    Delete {
        path: PathBuf,

        /// Do not ask.
        #[arg(short, long)]
        force: bool,
    },

    /// Attach an image to a VM.
    Attach {
        /// The VM.
        name: String,

        /// The image.
        path: PathBuf,

        /// Mount it read-only.
        #[arg(long)]
        readonly: bool,

        /// Make it the root device.
        ///
        /// The field is not called `root` because the top level CLI already has a
        /// `root` (the store directory) and clap's derive indexes arguments by field
        /// name: two fields with the same name collide at runtime with a panic about
        /// mismatched TypeIds, whatever the long option is called.
        #[arg(long = "root-disk")]
        root_disk: bool,

        /// A label for the operator.
        #[arg(long)]
        label: Option<String>,
    },

    /// Detach an image from a VM.
    Detach {
        /// The VM.
        name: String,

        /// The image path, or its index as shown by `librevm show`.
        path: String,
    },
}

#[derive(Subcommand, Debug)]
enum ShareCommand {
    /// Share a directory with a VM.
    Add {
        /// The VM.
        name: String,

        /// The directory on the host.
        path: PathBuf,

        /// The tag the guest mounts it by. Defaults to the directory's name.
        #[arg(long)]
        tag: Option<String>,

        /// Share it read-only.
        #[arg(long)]
        readonly: bool,
    },

    /// Stop sharing a directory.
    Remove {
        /// The VM.
        name: String,

        /// The tag, or the host path.
        tag: String,
    },

    /// List what a VM has shared.
    List { name: String },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
enum HypervisorArg {
    Auto,
    Gunyah,
    Kvm,
    Geniezone,
}

impl From<HypervisorArg> for Hypervisor {
    fn from(a: HypervisorArg) -> Self {
        match a {
            HypervisorArg::Auto => Hypervisor::Auto,
            HypervisorArg::Gunyah => Hypervisor::Gunyah,
            HypervisorArg::Kvm => Hypervisor::Kvm,
            HypervisorArg::Geniezone => Hypervisor::Geniezone,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
enum ProtectionArg {
    None,
    WithoutFirmware,
    WithFirmware,
}

impl From<ProtectionArg> for Protection {
    fn from(a: ProtectionArg) -> Self {
        match a {
            ProtectionArg::None => Protection::None,
            ProtectionArg::WithoutFirmware => Protection::WithoutFirmware,
            ProtectionArg::WithFirmware => Protection::WithFirmware,
        }
    }
}

fn main() {
    let cli = Cli::parse();

    let level = match cli.verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level)).init();

    let mut store = Store::from_env();
    if cli.root.is_some() || cli.run.is_some() {
        store = Store::new(
            cli.root
                .clone()
                .unwrap_or_else(|| PathBuf::from(librevm_core::store::DEFAULT_ROOT)),
            cli.run
                .clone()
                .unwrap_or_else(|| PathBuf::from(librevm_core::store::DEFAULT_RUN)),
        );
    }

    if let Err(e) = dispatch(&cli, &store) {
        eprintln!("librevm: {e:#}");
        std::process::exit(1);
    }
}

fn dispatch(cli: &Cli, store: &Store) -> Result<()> {
    match &cli.command {
        Command::Info => cmd_info(store),
        Command::List => cmd_list(store),
        Command::Create {
            name,
            kernel,
            initramfs,
            cmdline,
            memory,
            swiotlb,
            cpus,
            hypervisor,
            protection,
            firmware,
            crosvm,
            description,
        } => cmd_create(
            store,
            name,
            kernel,
            initramfs.clone(),
            cmdline.clone(),
            *memory,
            *swiotlb,
            *cpus,
            (*hypervisor).into(),
            (*protection).into(),
            firmware.clone(),
            crosvm.clone(),
            description.clone(),
        ),
        Command::Show { name } => cmd_show(store, name),
        Command::Plan { name } => cmd_plan(store, name),
        Command::Status { name } => cmd_status(store, name),
        Command::Start { name } => cmd_start(store, name),
        Command::Run { name } => cmd_run(store, name),
        Command::Stop { name, grace } => cmd_stop(store, name, *grace),
        Command::Console { name } => cmd_console(store, name),
        Command::Delete { name, force } => cmd_delete(store, name, *force),
        Command::Disk { action } => cmd_disk(store, action),
        Command::Share { action } => cmd_share(store, action),
        Command::Hub { name } => hub::run(store, name),
    }
}

// -- disks ------------------------------------------------------------------

fn cmd_disk(store: &Store, action: &DiskCommand) -> Result<()> {
    match action {
        DiskCommand::Create { path, size, format, fs } => {
            let size = disk::parse_size(size)?;
            let format = disk::Format::parse(format)?;
            let made = disk::create(path, size, format)?;

            println!("created {}", made.path.display());
            println!("  format    : {}", made.format.name());
            println!("  capacity  : {}", disk::human(made.capacity));
            println!("  on disk   : {}", disk::human(made.allocated));

            if let Some(kind) = fs {
                if unsafe { libc::geteuid() } != 0 {
                    bail!("making a {kind} filesystem needs root (it uses a loop device)");
                }
                disk::make_filesystem(path, kind)?;
                println!("  filesystem: {kind}");
            }
            Ok(())
        }

        DiskCommand::List { dir } => {
            let dir = dir.clone().unwrap_or_else(|| store.images_dir());
            let images = disk::list(&dir)?;
            if images.is_empty() {
                println!("no images in {}", dir.display());
                return Ok(());
            }
            println!(
                "{:<34} {:<8} {:>10} {:>10}  {}",
                "IMAGE", "FORMAT", "CAPACITY", "ON DISK", "SAVED"
            );
            for d in &images {
                println!(
                    "{:<34} {:<8} {:>10} {:>10}  {:.0}%",
                    d.path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("?"),
                    d.format.name(),
                    disk::human(d.capacity),
                    disk::human(d.allocated),
                    d.sparse_ratio() * 100.0
                );
            }
            Ok(())
        }

        DiskCommand::Info { path } => {
            let d = disk::open(path)?;
            println!("{}", d.path.display());
            println!("  format    : {}", d.format.name());
            println!("  capacity  : {}", disk::human(d.capacity));
            println!("  on disk   : {}", disk::human(d.allocated));
            println!("  sparse    : {:.0}% saved", d.sparse_ratio() * 100.0);
            println!("  read-only : {}", if d.readonly { "yes" } else { "no" });
            println!(
                "  writable  : {}",
                if d.format.writable() {
                    "yes".to_string()
                } else {
                    "no; this format needs qemu, which is not installed".to_string()
                }
            );
            Ok(())
        }

        DiskCommand::Resize { path, size, force } => {
            let size = disk::parse_size(size)?;
            let d = disk::resize(path, size, *force)?;
            println!(
                "{} is now {} ({} on disk)",
                d.path.display(),
                disk::human(d.capacity),
                disk::human(d.allocated)
            );
            Ok(())
        }

        DiskCommand::Clone { from, to } => {
            let d = disk::clone_to(from, to)?;
            println!(
                "{} -> {} ({}, {} on disk)",
                from.display(),
                d.path.display(),
                disk::human(d.capacity),
                disk::human(d.allocated)
            );
            Ok(())
        }

        DiskCommand::Delete { path, force } => {
            let d = disk::open(path)?;
            let attached: Vec<String> = store
                .list()?
                .into_iter()
                .filter(|n| {
                    store
                        .load(n)
                        .map(|v| v.disks.iter().any(|x| x.path == d.path))
                        .unwrap_or(false)
                })
                .collect();
            if !attached.is_empty() {
                bail!(
                    "{} is attached to {}; detach it first",
                    d.path.display(),
                    attached.join(", ")
                );
            }

            if !force {
                eprint!("delete {} ({})? [y/N] ", d.path.display(), disk::human(d.capacity));
                std::io::stderr().flush().ok();
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                    println!("left alone");
                    return Ok(());
                }
            }
            disk::delete(&d.path)?;
            println!("deleted {}", d.path.display());
            Ok(())
        }

        DiskCommand::Attach { name, path, readonly, root_disk, label } => {
            let mut vm = store.load(name)?;
            if !path.exists() {
                bail!("{} does not exist", path.display());
            }
            if vm.disks.iter().any(|d| d.path == *path) {
                bail!("{} is already attached to {name}", path.display());
            }
            if *root_disk && vm.disks.iter().any(|d| d.root) {
                bail!("{name} already has a root disk");
            }
            if *root_disk && *readonly {
                bail!("the root disk cannot be read-only");
            }

            vm.disks.push(DiskConfig {
                path: path.clone(),
                readonly: *readonly,
                root: *root_disk,
                label: label.clone(),
            });
            vm.validate()?;
            store.save(&vm)?;

            println!("attached {} to {name}", path.display());
            println!("  read-only : {}", if *readonly { "yes" } else { "no" });
            println!("  root      : {}", if *root_disk { "yes" } else { "no" });
            if vm::state(store, name) != State::Stopped {
                println!("  note: {name} is running; restart it for this to take effect");
            }
            Ok(())
        }

        DiskCommand::Detach { name, path } => {
            let mut vm = store.load(name)?;

            // accept either a path or the index `show` prints
            let index = if let Ok(i) = path.parse::<usize>() {
                if i < vm.disks.len() {
                    Some(i)
                } else {
                    bail!("{name} has no disk number {i}");
                }
            } else {
                let wanted = PathBuf::from(path);
                vm.disks.iter().position(|d| d.path == wanted)
            };

            let Some(index) = index else {
                bail!("{name} has no disk {path}");
            };
            let removed = vm.disks.remove(index);
            store.save(&vm)?;

            println!("detached {} from {name}", removed.path.display());
            if vm::state(store, name) != State::Stopped {
                println!("  note: {name} is running; restart it for this to take effect");
            }
            Ok(())
        }
    }
}

// -- shared directories ------------------------------------------------------

fn cmd_share(store: &Store, action: &ShareCommand) -> Result<()> {
    match action {
        ShareCommand::Add { name, path, tag, readonly } => {
            let mut vm = store.load(name)?;
            if !path.is_dir() {
                bail!("{} is not a directory", path.display());
            }
            let tag = tag.clone().unwrap_or_else(|| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("share")
                    .to_string()
            });
            if vm.shared_dirs.iter().any(|d| d.tag == tag) {
                bail!("{name} already has a share tagged {tag:?}");
            }

            vm.shared_dirs.push(SharedDirConfig {
                path: path.clone(),
                tag: tag.clone(),
                readonly: *readonly,
            });
            vm.validate()?;
            store.save(&vm)?;

            println!("sharing {} with {name} as {tag:?}", path.display());
            println!("  in the guest: mount -t 9p -o trans=virtio,version=9p2000.L {tag} /mnt");
            if vm::state(store, name) != State::Stopped {
                println!("  note: {name} is running; restart it for this to take effect");
            }
            Ok(())
        }

        ShareCommand::Remove { name, tag } => {
            let mut vm = store.load(name)?;
            let index = vm
                .shared_dirs
                .iter()
                .position(|d| &d.tag == tag || d.path.display().to_string() == *tag);
            let Some(index) = index else {
                bail!("{name} has no share {tag:?}");
            };
            let removed = vm.shared_dirs.remove(index);
            store.save(&vm)?;
            println!("stopped sharing {:?}", removed.tag);
            Ok(())
        }

        ShareCommand::List { name } => {
            let vm = store.load(name)?;
            if vm.shared_dirs.is_empty() {
                println!("{name} shares nothing");
                return Ok(());
            }
            println!("{:<20} {:<10} {}", "TAG", "MODE", "HOST PATH");
            for d in &vm.shared_dirs {
                println!(
                    "{:<20} {:<10} {}",
                    d.tag,
                    if d.readonly { "read-only" } else { "read-write" },
                    d.path.display()
                );
            }
            Ok(())
        }
    }
}

fn cmd_info(store: &Store) -> Result<()> {
    let info = vm::host_info();

    println!("librevm {}", librevm_core::VERSION);
    println!();

    println!("hypervisors:");
    if info.hypervisors.is_empty() {
        println!("  none found");
    }
    for h in &info.hypervisors {
        let node = h
            .node
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "-".into());
        println!("  {:<10} {:<16} {}", h.hypervisor.to_string(), node, h.detail);
    }

    println!();
    println!(
        "running under a hypervisor: {}",
        if info.under_hypervisor { "yes" } else { "no" }
    );

    match &info.crosvm {
        Some(p) => {
            println!("crosvm: {}", p.display());
            // Ask it which hypervisors its build actually contains, since a
            // crosvm without the gunyah feature will refuse the flag.
            match std::process::Command::new(p)
                .args(["run", "--hypervisor", "definitely-not-a-hypervisor"])
                .output()
            {
                Ok(out) => {
                    let text = String::from_utf8_lossy(&out.stderr).to_string()
                        + &String::from_utf8_lossy(&out.stdout);
                    if text.contains("Unrecognized argument")
                        || text.contains("arg parsing failed")
                    {
                        println!("  note: it rejected an unknown hypervisor name, which is normal");
                    }
                }
                Err(e) => println!("  could not run it: {e}"),
            }
        }
        None => println!(
            "crosvm: not found ({})",
            info.crosvm_error.as_deref().unwrap_or("no reason given")
        ),
    }

    println!();
    println!("store: {}", store.root().display());
    Ok(())
}

fn cmd_list(store: &Store) -> Result<()> {
    let all = vm::list(store)?;
    if all.is_empty() {
        println!("no VMs. create one with: librevm create <name> --kernel <path>");
        return Ok(());
    }

    println!(
        "{:<20} {:<22} {:<10} {:>9} {:>9} {:>5}",
        "NAME", "STATE", "HYPERVISOR", "MEMORY", "GUEST", "CPUS"
    );
    for s in all {
        println!(
            "{:<20} {:<22} {:<10} {:>7} M {:>7} M {:>5}",
            s.name,
            s.state.to_string(),
            s.hypervisor,
            s.memory_mib,
            s.guest_memory_mib,
            s.cpus
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_create(
    store: &Store,
    name: &str,
    kernel: &PathBuf,
    initramfs: Option<PathBuf>,
    cmdline: Option<String>,
    memory: u64,
    swiotlb: u64,
    cpus: u32,
    hypervisor: Hypervisor,
    protection: Protection,
    firmware: Option<PathBuf>,
    crosvm: Option<PathBuf>,
    description: Option<String>,
) -> Result<()> {
    if store.exists(name) {
        bail!("a VM named {name:?} already exists");
    }

    if !kernel.exists() {
        bail!("kernel {} does not exist", kernel.display());
    }

    let vm = VmConfig {
        name: name.to_string(),
        cpus,
        memory_mib: memory,
        hypervisor: HypervisorConfig {
            backend: hypervisor,
            swiotlb_mib: swiotlb,
            protection,
            firmware,
            crosvm,
            disable_sandbox: true,
        },
        boot: BootConfig {
            kernel: kernel.clone(),
            initramfs,
            cmdline: cmdline.unwrap_or_else(|| "console=ttyS0 earlycon loglevel=7".to_string()),
        },
        console: ConsoleConfig {
            socket: Some(store.console_socket(name)),
            log: Some(store.console_log(name)),
        },
        disks: Vec::new(),
        shared_dirs: Vec::new(),
        description,
    };

    vm.validate()?;
    store.save(&vm)?;

    println!("created {name}");
    println!("  definition : {}", store.config_path(name).display());
    println!("  kernel     : {}", vm.boot.kernel.display());
    println!("  memory     : {} MiB total, {} MiB to the guest", vm.memory_mib, vm.guest_memory_mib());
    println!("  hypervisor : {}", vm.hypervisor.backend);
    println!("  protection : {:?}", vm.hypervisor.protection);
    println!();
    println!("start it with: librevm start {name}");
    Ok(())
}

fn cmd_show(store: &Store, name: &str) -> Result<()> {
    let vm = store.load(name)?;
    print!("{}", vm.to_toml()?);
    Ok(())
}

fn cmd_plan(store: &Store, name: &str) -> Result<()> {
    let vm = store.load(name)?;
    let plan = vm::plan(store, &vm, true)?;

    println!("# hypervisor: {}", plan.hypervisor);
    if plan.console_is_socket {
        println!("# console on {}", store.console_socket(name).display());
    }
    println!();

    // One argument per line is easier to read than a wrapped shell command, and
    // it makes it obvious which are values and which are flags.
    print!("{}", plan.argv[0]);
    for arg in &plan.argv[1..] {
        if arg.starts_with('-') {
            print!(" \\\n  {}", arg);
        } else {
            print!(" {}", arg);
        }
    }
    println!();
    Ok(())
}

fn cmd_status(store: &Store, name: &str) -> Result<()> {
    if !store.exists(name) {
        bail!("no VM named {name:?}");
    }
    let s = vm::summary(store, name)?;
    println!("{:<12} {}", "name", s.name);
    println!("{:<12} {}", "state", s.state);
    println!("{:<12} {}", "hypervisor", s.hypervisor);
    println!("{:<12} {} MiB ({} MiB to the guest)", "memory", s.memory_mib, s.guest_memory_mib);
    println!("{:<12} {}", "cpus", s.cpus);
    println!("{:<12} {}", "kernel", s.kernel.display());

    match s.state {
        State::Running { .. } => {
            println!("{:<12} {}", "console", store.console_socket(name).display());
            println!("{:<12} {}", "log", store.console_log(name).display());
        }
        State::Stale { .. } => {
            println!();
            println!("the process is gone but the pid file remained; `librevm start` will clear it");
        }
        State::Stopped => {}
    }
    Ok(())
}

/// Start a VM, detached.
///
/// crosvm's serial client connects to a socket that must already have a listener,
/// and this process exits immediately, so the work is done by a detached hub that
/// owns that socket and outlives us. The hub lives in the core crate, so the GUI
/// starts machines exactly the same way.
fn cmd_start(store: &Store, name: &str) -> Result<()> {
    let exe = std::env::current_exe().context("finding our own path")?;
    let pid = hub::spawn(store, name, &exe)?;

    println!("{name} started (pid {pid})");
    println!("  console : librevm console {name}");
    println!("  log     : {}", store.console_log(name).display());
    Ok(())
}

fn cmd_run(store: &Store, name: &str) -> Result<()> {
    let code = vm::run_foreground(store, name)?;
    std::process::exit(code);
}

fn cmd_stop(store: &Store, name: &str, grace: u64) -> Result<()> {
    vm::stop(store, name, grace)?;
    println!("{name} stopped");
    Ok(())
}

fn cmd_delete(store: &Store, name: &str, force: bool) -> Result<()> {
    if !store.exists(name) {
        bail!("no VM named {name:?}");
    }

    if let State::Running { pid } = vm::state(store, name) {
        if !force {
            bail!("{name} is running (pid {pid}); stop it first or pass --force");
        }
        vm::stop(store, name, 5000)?;
    }

    if !force {
        print!("delete {name} and all its files? [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("cancelled");
            return Ok(());
        }
    }

    store.remove(name)?;
    println!("deleted {name}");
    Ok(())
}

/// Attach to a running VM's console.
///
/// The hub owns the socket and broadcasts guest output to every attached client,
/// so detaching with Ctrl-C leaves the guest running.
fn cmd_console(store: &Store, name: &str) -> Result<()> {
    hub::attach(store, name)
}
