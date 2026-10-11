//! `librevm` - the command line front end.
//!
//! Everything it does goes through librevm-core, so the daemon and the GUI get the
//! same behaviour.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use librevm_core::config::{
    BootConfig, ConsoleConfig, Hypervisor, HypervisorConfig, Protection, VmConfig,
};
use librevm_core::store::Store;
use librevm_core::vm::{self, State};
use librevm_core::hub;
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

    /// Internal: the per-VM console hub. Started detached by `start`.
    #[command(hide = true, name = "__hub")]
    Hub { name: String },
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
        Command::Hub { name } => hub::run(store, name),
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
