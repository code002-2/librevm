//! `librevm` - the command line front end.
//!
//! Everything it does goes through librevm-core, so the daemon and the GUI get the
//! same behaviour.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use librevm_core::config::{
    BootConfig, ConsoleConfig, DiskBus, DiskConfig, Hypervisor, HypervisorConfig, Protection,
    SerialBackend, SharedDirConfig, VmConfig,
};
use librevm_core::disk;
use librevm_core::hub;
use librevm_core::store::Store;
use librevm_core::vm::{self, State};
use std::io::Write;
use std::path::{Path, PathBuf};

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

        /// Where the guest's console goes: stdout, file:<path>, pty, unix:<path>,
        /// unix-stream:<path>.
        ///
        /// stdout is what a foreground run uses. When the machine is started through the
        /// hub, the hub's own console socket takes precedence, so this matters for
        /// `librevm run`.
        #[arg(long, default_value = "stdout")]
        serial: String,

        /// Keep the guest kernel's log across reboots, so a crash can be read afterwards.
        /// The guest needs ramoops configured for it to have anywhere to write.
        #[arg(long)]
        pstore: bool,

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

    /// Guest networking.
    Net {
        #[command(subcommand)]
        action: NetCommand,
    },

    /// Fetch a system image and deploy it as a disk.
    Image {
        #[command(subcommand)]
        action: ImageCommand,
    },

    /// Reach a machine over SSH.
    Ssh {
        #[command(subcommand)]
        action: Option<SshCommand>,

        /// The machine, when connecting.
        name: Option<String>,

        /// A command to run instead of opening a shell.
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
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

        /// raw, sparse or qcow2. Raw images are made sparse on disk anyway, so this
        /// only matters for tools that expect the Android format; qcow2 is the one that
        /// can carry a backing file.
        #[arg(short, long, default_value = "raw")]
        format: String,

        /// Make this an overlay on an existing qcow2 image: blocks not written here are
        /// read from the backing file. That is how a snapshot layer is built -- the
        /// overlay starts empty, and deleting it is the rollback.
        #[arg(long)]
        backing: Option<PathBuf>,

        /// Also make a filesystem inside it: ext4, ext2 or vfat. Needs root,
        /// because it attaches the image to a loop device. Not possible on qcow2.
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

        /// How to present it: virtio, scsi, pmem or cdrom.
        #[arg(long, value_enum, default_value_t = BusArg::Virtio)]
        bus: BusArg,

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
enum ShareCommand {    /// Share a directory with a VM.
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

        /// How much the guest may cache: never, auto or always.
        ///
        /// Only meaningful for virtio-fs, which is what these shares use. `auto` gives
        /// close-to-open consistency, so a file written on the host is seen by the guest
        /// as soon as it is opened again; `always` is faster and can serve stale data.
        #[arg(long, value_enum)]
        cache: Option<CacheArg>,
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

#[derive(Subcommand, Debug)]
enum ImageCommand {
    /// List the images that can be fetched.
    List,

    /// Fetch one and deploy it as a disk, then attach it to a machine.
    Fetch {
        /// The id from `image list`.
        id: String,

        /// What to call the image. Defaults to the id.
        #[arg(long)]
        name: Option<String>,

        /// How big the disk should be: 2G, 4096M. Defaults to the catalogue's suggestion.
        #[arg(short, long)]
        size: Option<String>,

        /// Attach it to this machine when it is ready.
        #[arg(long)]
        attach_to: Option<String>,

        /// Mount it as the root device.
        ///
        /// Named `as_root` rather than `root`: clap's derive indexes by *field* name, and
        /// the short name collides with the `--root` option every command inherits for the
        /// store directory. Renaming only the long option is not enough.
        #[arg(long = "root")]
        as_root: bool,
    },

    /// Deploy an image from a URL or a local tarball, without using the catalogue.
    From {
        /// An http(s) URL, or a path to a tarball already on this machine.
        source: String,

        /// What to call it.
        #[arg(long)]
        name: String,

        #[arg(short, long)]
        size: Option<String>,

        /// tar's compression flag: -z, -J, or --zstd.
        #[arg(long, default_value = "-z")]
        compression: String,
    },

    /// What is already downloaded.
    Cached,
}

#[derive(Subcommand, Debug)]
enum SshCommand {
    /// Show the public key guests are given, making the keypair if needed.
    Key {
        /// Print only the key, for pasting elsewhere.
        #[arg(long)]
        quiet: bool,
    },

    /// Write an ssh config so guests can be reached by name outside this tool.
    Config,

    /// Check whether a machine is reachable.
    Check {
        /// The machine.
        name: String,
    },
}

#[derive(Subcommand, Debug)]
enum NetCommand {    /// Turn networking on for a VM and set the host side up now.
    Enable {
        /// The VM.
        name: String,

        /// Which guest address to use, as the last octet of the host's subnet. Two
        /// machines on one host need different values.
        #[arg(long, default_value = "0")]
        index: u8,
    },

    /// Turn networking off.
    Disable { name: String },

    /// Set the host side up without starting a machine, and say what the guest should use.
    Setup {
        #[arg(long, default_value = "0")]
        index: u8,
    },

    /// Take the host side down.
    Teardown,

    /// Report whether the tap exists and what it is addressed.
    Status,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
enum CacheArg {
    Never,
    Auto,
    Always,
}

impl From<CacheArg> for librevm_core::config::SharedDirCache {
    fn from(a: CacheArg) -> Self {
        use librevm_core::config::SharedDirCache as C;
        match a {
            CacheArg::Never => C::Never,
            CacheArg::Auto => C::Auto,
            CacheArg::Always => C::Always,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
enum BusArg {
    Virtio,
    Scsi,
    Pmem,
    Cdrom,
}

impl From<BusArg> for DiskBus {
    fn from(a: BusArg) -> Self {
        match a {
            BusArg::Virtio => DiskBus::Virtio,
            BusArg::Scsi => DiskBus::Scsi,
            BusArg::Pmem => DiskBus::Pmem,
            BusArg::Cdrom => DiskBus::Cdrom,
        }
    }
}

impl BusArg {
    fn name(self) -> &'static str {
        DiskBus::from(self).name()
    }
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
            serial,
            pstore,
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
            parse_serial_backend(serial)?,
            *pstore,
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
        Command::Net { action } => cmd_net(store, action),
        Command::Image { action } => cmd_image(store, action),
        Command::Ssh {
            action,
            name,
            command,
        } => cmd_ssh(store, action.as_ref(), name.as_deref(), command),
        Command::Share { action } => cmd_share(store, action),
        Command::Hub { name } => hub::run(store, name),
    }
}

// -- networking ---------------------------------------------------------------

fn cmd_net(store: &Store, action: &NetCommand) -> Result<()> {
    match action {
        NetCommand::Enable { name, index } => {
            let mut vm = store.load(name)?;
            vm.network.enabled = true;
            vm.network.index = *index;
            store.save(&vm)?;

            // Set the host side up now as well, so a mistake is reported here rather than
            // going to a log file when the machine starts.
            match librevm_core::net::setup(*index) {
                Ok(guest) => {
                    println!("networking enabled for {name}");
                    println!();
                    println!("guest:");
                    print!("{}", indent(&guest.script()));
                    Ok(())
                }
                Err(e) => {
                    eprintln!("librevm: saved the setting, but the host side did not come up:");
                    eprintln!("  {e:#}");
                    std::process::exit(1);
                }
            }
        }

        NetCommand::Disable { name } => {
            let mut vm = store.load(name)?;
            vm.network.enabled = false;
            store.save(&vm)?;
            librevm_core::net::teardown().ok();
            println!("networking disabled for {name}");
            Ok(())
        }

        NetCommand::Setup { index } => {
            let guest = librevm_core::net::setup(*index)?;
            println!("tap up");
            print!("{}", indent(&guest.script()));
            Ok(())
        }

        NetCommand::Teardown => {
            // Refuse while a machine is still attached to the tap.
            //
            // Removing it under a running guest leaves the host with no route to an address
            // the guest still believes it owns, and the symptom is "no route to host" from
            // ssh with nothing on the host side to explain it: no tap, no route, and an ARP
            // entry pointing at the Wi-Fi interface because that is where the subnet lives.
            let live: Vec<String> = store
                .list()
                .unwrap_or_default()
                .into_iter()
                .filter(|n| vm::state(store, n) != State::Stopped)
                .collect();
            if !live.is_empty() {
                bail!(
                    "{} is still running and using the tap; stop it first",
                    live.join(", ")
                );
            }
            librevm_core::net::teardown()?;
            println!("the tap is gone");
            Ok(())
        }

        NetCommand::Status => {
            let name = librevm_core::net::TAP_NAME;
            println!("tap      : {name}");
            println!(
                "present  : {}",
                if librevm_core::net::is_up() { "yes" } else { "no" }
            );
            for (label, path) in [
                ("address", format!("/sys/class/net/{name}/address")),
                ("mtu", format!("/sys/class/net/{name}/mtu")),
            ] {
                if let Ok(v) = std::fs::read_to_string(&path) {
                    println!("{label:<9}: {}", v.trim());
                }
            }
            Ok(())
        }
    }
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|l| format!("    {l}\n"))
        .collect::<String>()
}

// -- serial -----------------------------------------------------------------

/// Read the `--serial` value, which is a short form rather than a path on its own.
///
/// The shape is `backend` or `backend:path`, because a bare path would be ambiguous with
/// the name of a backend and a colon cannot appear in either.
fn parse_serial_backend(text: &str) -> Result<SerialBackend> {
    let (kind, path) = match text.split_once(':') {
        Some((k, p)) => (k, Some(p)),
        None => (text, None),
    };
    let need = |what: &str| -> Result<PathBuf> {
        path.map(PathBuf::from)
            .with_context(|| format!("{what} needs a path: {what}:<path>"))
    };
    Ok(match kind {
        "stdout" => SerialBackend::Stdout,
        "pty" => SerialBackend::Pty,
        "file" => SerialBackend::File(need("file")?),
        "unix" => SerialBackend::Unix(need("unix")?),
        "unix-stream" => SerialBackend::UnixStream(need("unix-stream")?),
        other => bail!(
            "unknown serial backend {other:?}; try stdout, file:<path>, pty, \
             unix:<path> or unix-stream:<path>"
        ),
    })
}

// -- images -----------------------------------------------------------------

fn cmd_image(store: &Store, action: &ImageCommand) -> Result<()> {
    match action {
        ImageCommand::List => {
            println!("fetching the image manifest...");
            let mirror = librevm_core::image::chosen_mirror();
            let catalogue = librevm_core::image::catalogue_from_manifest(&mirror)?;
            println!(
                "{:<26} {:>10} {:>9}  {}",
                "ID", "DOWNLOAD", "DISK", "DESCRIPTION"
            );
            for e in &catalogue {
                println!(
                    "{:<26} {:>7} M {:>6} M  {}",
                    e.id, e.download_mib, e.disk_mib, e.description
                );
            }
            println!();
            println!("mirror: {mirror}   (change with LIBREVM_MIRROR)");
            println!("Deploy one with: librevm image fetch <id> --name <name>");
            Ok(())
        }

        ImageCommand::Fetch {
            id,
            name,
            size,
            attach_to,
            as_root,
        } => {
            // The catalogue comes from the mirror's manifest, so what is offered is always
            // what the mirror actually has.
            let mirror = librevm_core::image::chosen_mirror();
            let catalogue = librevm_core::image::catalogue_from_manifest(&mirror)?;
            let entry = librevm_core::image::find(&catalogue, id)
                .with_context(|| format!("no image called {id}; try `librevm image list`"))?
                .clone();
            let name = name.clone().unwrap_or_else(|| id.clone());
            check_image_name(&name)?;
            let size_mib = match size {
                Some(s) => Some(librevm_core::disk::parse_size(s)? / (1024 * 1024)),
                None => None,
            };

            deploy_one(store, &entry, &name, size_mib, attach_to.as_deref(), *as_root)
        }

        ImageCommand::From {
            source,
            name,
            size,
            compression,
        } => {
            check_image_name(name)?;
            let size_mib = match size {
                Some(s) => Some(librevm_core::disk::parse_size(s)? / (1024 * 1024)),
                None => None,
            };

            // A local tarball is deployed directly; a URL is downloaded into the cache
            // first, so a repeat costs nothing.
            let archive = if source.starts_with("http://") || source.starts_with("https://") {
                let entry = librevm_core::image::ImageEntry::from_url(
                    source,
                    match compression.as_str() {
                        "-J" => librevm_core::image::Compression::Xz,
                        "--zstd" => librevm_core::image::Compression::Zstd,
                        _ => librevm_core::image::Compression::Gzip,
                    },
                );
                println!("downloading {source}");
                librevm_core::image::fetch(&store.images_dir(), &entry, |got, total| {
                    librevm_core::image::progress_line(got, total)
                })?
            } else {
                let p = PathBuf::from(source);
                if !p.is_file() {
                    bail!("{} is not a file", p.display());
                }
                p
            };
            println!();

            let compression = match compression.as_str() {
                "-J" => librevm_core::image::Compression::Xz,
                "--zstd" => librevm_core::image::Compression::Zstd,
                _ => librevm_core::image::Compression::Gzip,
            };
            deploy_archive(store, &archive, name, size_mib, compression, None).map(|_| ())
        }

        ImageCommand::Cached => {
            let dir = librevm_core::image::cache_dir(&store.images_dir());
            if !dir.is_dir() {
                println!("nothing downloaded yet ({})", dir.display());
                return Ok(());
            }
            let mut entries: Vec<_> = std::fs::read_dir(&dir)?
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .collect();
            entries.sort_by_key(|e| e.file_name());
            let mut total = 0u64;
            for e in &entries {
                let len = e.metadata().map(|m| m.len()).unwrap_or(0);
                total += len;
                println!("  {:>8.1} MiB  {}", len as f64 / 1048576.0, e.file_name().to_string_lossy());
            }
            println!(
                "  {} files, {:.1} MiB in {}",
                entries.len(),
                total as f64 / 1048576.0,
                dir.display()
            );
            Ok(())
        }
    }
}

/// Names become filenames, so keep them to something safe.
fn check_image_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        bail!("an image name must be 1 to 64 characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("an image name may use letters, digits, dash, underscore and dot");
    }
    Ok(())
}

fn deploy_one(
    store: &Store,
    entry: &librevm_core::image::ImageEntry,
    name: &str,
    size_mib: Option<u64>,
    attach_to: Option<&str>,
    root: bool,
) -> Result<()> {
    println!("{}: {}", entry.id, entry.description);
    println!("  {}", entry.url);
    println!();

    // The key has to exist before the image is built, because it is injected into it.
    let public = librevm_core::ssh::ensure_keypair(store.root())?;

    println!(
        "downloading ({}{} MiB)",
        if entry.download_mib > 0 {
            format!("about {}", entry.download_mib)
        } else {
            "size unknown, ".to_string()
        },
        if entry.download_mib > 0 { "" } else { "" }
    );
    let archive = librevm_core::image::fetch(&store.images_dir(), entry, |got, total| {
        librevm_core::image::progress_line(got, total)
    })?;
    println!();
    println!("  cached at {}", archive.display());

    deploy_archive(
        store,
        &archive,
        name,
        size_mib.or(Some(entry.disk_mib)),
        entry.compression,
        Some(&public),
    )
    .and_then(|path| {
        if let Some(machine) = attach_to {
            attach_image(store, &path, machine, root)
        } else {
            Ok(())
        }
    })
    .map(|_| ())
}

fn deploy_archive(
    store: &Store,
    archive: &Path,
    name: &str,
    size_mib: Option<u64>,
    compression: librevm_core::image::Compression,
    public_key: Option<&str>,
) -> Result<PathBuf> {
    let images = store.images_dir();
    std::fs::create_dir_all(&images)?;
    let target = images.join(format!("{name}.img"));
    if target.exists() {
        bail!("{} exists already", target.display());
    }

    // A rough size from the archive when the caller did not say. Unpacking a tree bigger
    // than the filesystem fails part way through, after most of the work.
    let mut size = size_mib.unwrap_or(2048);
    if size_mib.is_none() {
        if let Ok(Some(bytes)) = librevm_core::image::archive_uncompressed_size(archive) {
            let mib = bytes / 1048576;
            size = (mib * 2 + 256).max(512);
            println!("  the tree is about {mib} MiB; making the disk {size} MiB");
        }
    }

    println!("  making a {size} MiB ext4 image at {}", target.display());
    librevm_core::image::make_image(&target, size)?;

    println!("  unpacking");
    let key = public_key
        .map(|k| k.to_string())
        .or_else(|| librevm_core::ssh::ensure_keypair(store.root()).ok());
    let dns = librevm_core::image::host_nameservers();
    let injections = librevm_core::image::Injections {
        authorized_key: key,
        ssh_user: Some(librevm_core::ssh::USER.to_string()),
        dns,
        root_password: None,
    };

    match unpack_into(&target, compression, archive, &injections) {
        Ok(notes) => {
            println!("  done");
            for n in &notes {
                println!("    {n}");
            }
        }
        Err(e) => {
            std::fs::remove_file(&target).ok();
            return Err(e);
        }
    }

    let meta = std::fs::metadata(&target)?;
    let on_disk = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            librevm_core::disk::human(meta.blocks() * 512)
        }
        #[cfg(not(unix))]
        {
            librevm_core::disk::human(meta.len())
        }
    };
    println!();
    println!("{}", target.display());
    println!("  {} MiB, {} on disk", size, on_disk);
    println!();
    println!("Attach it with:");
    println!(
        "  librevm disk attach <machine> {} --label {name}",
        target.display()
    );
    Ok(target)
}

/// Loop-mount the image, unpack into it, inject, and put it back.
fn unpack_into(
    image: &Path,
    compression: librevm_core::image::Compression,
    archive: &Path,
    injections: &librevm_core::image::Injections,
) -> Result<Vec<String>> {
    let flag = match compression {
        librevm_core::image::Compression::Gzip => "-z",
        librevm_core::image::Compression::Xz => "-J",
        librevm_core::image::Compression::Zstd => "--zstd",
    };

    let loop_device = {
        let out = std::process::Command::new("losetup")
            .args(["--find", "--show", &image.display().to_string()])
            .output()
            .context("running losetup; is util-linux installed?")?;
        if !out.status.success() {
            bail!("losetup: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    let mount_point = PathBuf::from(format!("/tmp/librevm-deploy-{}", std::process::id()));
    std::fs::create_dir_all(&mount_point)?;

    // Everything from here has to be undone, including on the error paths.
    let result = (|| -> Result<Vec<String>> {
        // The same helper the daemon uses: `mount(8)` is a setuid program and refuses to
        // do anything under a unit that forbids gaining privileges, with a message that
        // does not say so.
        librevm_core::image::mount_loop_for_cli(&loop_device, &mount_point)?;

        let out = std::process::Command::new("tar")
            .args([
                "-x",
                flag,
                "-f",
                &archive.display().to_string(),
                "-C",
                &mount_point.display().to_string(),
                "--numeric-owner",
                // See image.rs: the setuid bits cannot be restored under the daemon's
                // unit, and they are not needed in a guest where root is root already.
                "--no-same-permissions",
                "--warning=no-unknown-keyword",
            ])
            .output()
            .context("running tar")?;
        if !out.status.success() {
            bail!("tar: {}", String::from_utf8_lossy(&out.stderr).trim());
        }

        librevm_core::image::inject(&mount_point, injections)
    })();

    // Unmount and detach whatever happened, and in that order.
    librevm_core::image::unmount_for_cli(&mount_point);
    let _ = std::process::Command::new("losetup")
        .args(["--detach", &loop_device])
        .output();
    let _ = std::fs::remove_dir(&mount_point);

    result
}


fn attach_image(store: &Store, image: &Path, machine: &str, root: bool) -> Result<()> {
    let mut vm = store.load(machine)?;
    let label = image
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "root".to_string());
    vm.disks.push(librevm_core::config::DiskConfig {
        path: image.to_path_buf(),
        readonly: false,
        root,
        bus: Default::default(),
        label: Some(label),
    });
    vm.validate()?;
    store.save(&vm)?;
    println!("attached to {machine}");
    Ok(())
}

// -- ssh --------------------------------------------------------------------

fn cmd_ssh(
    store: &Store,
    action: Option<&SshCommand>,
    name: Option<&str>,
    command: &[String],
) -> Result<()> {
    let (have_ssh, have_keygen) = librevm_core::ssh::tools_available();
    if !have_ssh || !have_keygen {
        bail!("openssh-client is needed: ssh and ssh-keygen must be on PATH");
    }

    match action {
        Some(SshCommand::Key { quiet }) => {
            let key = librevm_core::ssh::ensure_keypair(store.root())?;
            if *quiet {
                println!("{key}");
            } else {
                println!("{}", librevm_core::ssh::public_key(store.root()).display());
                println!("{key}");
                println!();
                println!("This is injected into images deployed with `image fetch`.");
            }
            Ok(())
        }

        Some(SshCommand::Config) => {
            librevm_core::ssh::ensure_keypair(store.root())?;
            let machines = ssh_targets(store);
            if machines.is_empty() {
                println!("no machines yet");
            }
            let path = librevm_core::ssh::write_client_config(store.root(), &machines)?;
            println!("{}", path.display());
            println!();
            for (name, address) in &machines {
                println!("  librevm-{name}  ->  {address}");
            }
            println!();
            println!("Add this to ~/.ssh/config to use those names:");
            println!("  Include {}", path.display());
            Ok(())
        }

        Some(SshCommand::Check { name }) => {
            let address = guest_address(store, name)?;
            println!("{name} at {address}");
            let out = librevm_core::ssh::run(store.root(), &address, "uname -a");
            match out {
                Ok(text) => {
                    println!("reachable");
                    println!("  {}", text.trim());
                    Ok(())
                }
                Err(e) => {
                    println!("not reachable: {e}");
                    std::process::exit(1);
                }
            }
        }

        None => {
            let name = name.context("which machine? try `librevm list`")?;
            let address = guest_address(store, name)?;
            let joined = if command.is_empty() {
                None
            } else {
                Some(command.join(" "))
            };
            librevm_core::ssh::connect(store.root(), &address, joined.as_deref())
        }
    }
}

/// Every machine that has a network, and the address it will be on.
fn ssh_targets(store: &Store) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for name in store.list().unwrap_or_default() {
        if let Ok(vm) = store.load(&name) {
            if vm.network.enabled {
                if let Ok(addr) = guest_address(store, &name) {
                    out.push((name, addr));
                }
            }
        }
    }
    out
}

/// The address a machine's guest will take, from its network index.
///
/// This is the same arithmetic `net::setup` does, kept here so a command can name a guest
/// without the host side having to be up.
pub fn guest_address(store: &Store, name: &str) -> Result<String> {
    let vm = store.load(name)?;
    if !vm.network.enabled {
        bail!("{name} has no network; `librevm net enable {name}`");
    }
    let octets = host_subnet()?;
    let last = 201u8.saturating_add(vm.network.index);
    Ok(format!("{}.{}.{}.{}", octets[0], octets[1], octets[2], last))
}

/// The host's own address, so the guest's can be derived from it.
fn host_subnet() -> Result<[u8; 3]> {
    let route = std::fs::read_to_string("/proc/net/route").context("reading the routing table")?;
    let mut interface = None;
    for line in route.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() >= 2 && f[1] == "00000000" {
            interface = Some(f[0].to_string());
            break;
        }
    }
    let interface = interface.context("no default route; is this host online?")?;
    let out = std::process::Command::new("ip")
        .args(["-4", "-brief", "addr", "show", &interface])
        .output()
        .context("asking ip for the address")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let cidr = text
        .split_whitespace()
        .find(|w| w.contains('/') && w.contains('.'))
        .context("the uplink has no IPv4 address")?;
    let addr: std::net::Ipv4Addr = cidr
        .split('/')
        .next()
        .context("malformed address")?
        .parse()
        .context("malformed address")?;
    let o = addr.octets();
    Ok([o[0], o[1], o[2]])
}

// -- disks ------------------------------------------------------------------

fn cmd_disk(store: &Store, action: &DiskCommand) -> Result<()> {
    match action {
        DiskCommand::Create { path, size, format, backing, fs } => {
            let size = disk::parse_size(size)?;
            let format = disk::Format::parse(format)?;

            // A backing file makes this an overlay, which means the size comes from the
            // base rather than from the command line.
            if let Some(base) = backing {
                if format != disk::Format::Qcow2 {
                    bail!("only qcow2 can have a backing file; --format qcow2 is needed");
                }
                if !base.is_file() {
                    bail!("backing file {} does not exist", base.display());
                }
            }

            let made = disk::create_with_backing(path, size, format, backing.as_deref())?;

            println!("created {}", made.path.display());
            println!("  format    : {}", made.format.name());
            println!("  capacity  : {}", disk::human(made.capacity));
            println!("  on disk   : {}", disk::human(made.allocated));
            if let Some(base) = backing {
                println!("  backing   : {}", base.display());
                println!();
                println!("This is an overlay: it starts empty and reads anything not written");
                println!("from the base. Deleting it is the rollback.");
            }

            if let Some(kind) = fs {
                if format == disk::Format::Qcow2 {
                    bail!(
                        "a filesystem cannot be written into a qcow2 here: that needs \
                         qemu-img, which is not installed"
                    );
                }
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

        DiskCommand::Attach { name, path, readonly, root_disk, bus, label } => {
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
                bus: (*bus).into(),
                label: label.clone(),
            });
            vm.validate()?;
            store.save(&vm)?;

            println!("attached {} to {name}", path.display());
            println!("  read-only : {}", if *readonly { "yes" } else { "no" });
            println!("  root      : {}", if *root_disk { "yes" } else { "no" });
            println!("  bus       : {}", bus.name());
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
        ShareCommand::Add { name, path, tag, readonly, cache } => {
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
                cache: cache.map(Into::into).unwrap_or_default(),
            });
            vm.validate()?;
            store.save(&vm)?;

            println!("sharing {} with {name} as {tag:?}", path.display());
            // virtio-fs, not 9p: see backend.rs for why the 9p device is not used here.
            println!("  in the guest: mount -t virtiofs {tag} /mnt");
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
    serial: SerialBackend,
    pstore: bool,
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
        serial: librevm_core::config::SerialConfig { backend: serial, pstore },
        disks: Vec::new(),
        shared_dirs: Vec::new(),
        network: Default::default(),
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
