//! VM definitions.
//!
//! A VM is one TOML file. Everything the backend needs to build a crosvm command
//! line lives here, so a VM can be created, edited and handed around as a plain
//! text file.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Which hypervisor to run on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Hypervisor {
    /// Pick the best available one at start time.
    Auto,
    /// Qualcomm Gunyah. Present on this class of device already owning EL2.
    Gunyah,
    /// Linux KVM.
    Kvm,
    /// MediaTek GenieZone.
    Geniezone,
}

impl Default for Hypervisor {
    fn default() -> Self {
        Hypervisor::Auto
    }
}

impl std::fmt::Display for Hypervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Hypervisor::Auto => "auto",
            Hypervisor::Gunyah => "gunyah",
            Hypervisor::Kvm => "kvm",
            Hypervisor::Geniezone => "geniezone",
        };
        f.write_str(s)
    }
}

/// How isolated the guest memory is.
///
/// This is not a knob to set casually: on Gunyah it decides which ioctl crosvm
/// uses to hand memory to the hypervisor, and some devices only accept one of
/// them. See `backend.rs` for what each choice means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protection {
    /// Host and guest share the memory normally. On Gunyah this uses
    /// `SET_USER_MEM_REGION`.
    None,
    /// Memory is lent to the guest, but no firmware runs first: the hypervisor
    /// jumps straight to the payload. On Gunyah this uses the LEND ioctl.
    WithoutFirmware,
    /// Lent memory plus a first-stage firmware image. Needs `firmware` set.
    WithFirmware,
}

impl Default for Protection {
    fn default() -> Self {
        Protection::WithoutFirmware
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HypervisorConfig {
    #[serde(default)]
    pub backend: Hypervisor,

    /// Size of the virtio swiotlb window, in MiB.
    ///
    /// Gunyah requires a static swiotlb, and crosvm computes the guest's usable
    /// RAM as `memory_mib - swiotlb_mib`. Its subtraction is unchecked, so this
    /// must stay strictly below `memory_mib` or crosvm panics on underflow.
    #[serde(default = "default_swiotlb")]
    pub swiotlb_mib: u64,

    #[serde(default)]
    pub protection: Protection,

    /// Path to the protected VM firmware, when `protection = "with-firmware"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<PathBuf>,

    /// Path to the crosvm binary. Empty means look it up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crosvm: Option<PathBuf>,

    /// Run crosvm without its minijail sandbox. Needed on systems without a
    /// usable `/var/empty`, and for the console to attach cleanly.
    #[serde(default = "default_true")]
    pub disable_sandbox: bool,
}

fn default_swiotlb() -> u64 {
    64
}

fn default_true() -> bool {
    true
}

impl Default for HypervisorConfig {
    fn default() -> Self {
        Self {
            backend: Hypervisor::default(),
            swiotlb_mib: default_swiotlb(),
            protection: Protection::default(),
            firmware: None,
            crosvm: None,
            disable_sandbox: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootConfig {
    /// The kernel image to boot.
    pub kernel: PathBuf,

    /// Optional initramfs. Usually unnecessary when the kernel has one embedded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initramfs: Option<PathBuf>,

    /// Kernel command line.
    #[serde(default = "default_cmdline")]
    pub cmdline: String,
}

fn default_cmdline() -> String {
    // ttyS0 because crosvm's default serial hardware is an 8250, and earlycon so
    // output starts before the driver probes.
    "console=ttyS0 earlycon loglevel=7".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsoleConfig {
    /// Serve the console on a Unix stream socket here. The socket is how a client
    /// attaches to a running guest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<PathBuf>,

    /// Also write the console to this file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<PathBuf>,
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        Self { socket: None, log: None }
    }
}

/// How a disk is presented to the guest.
///
/// virtio-blk is the default and is what the rest of this works with. The others exist
/// because a real system sometimes needs them: SCSI for a device that only speaks it,
/// CDROM to attach an ISO, and pmem for a region the guest can map directly rather than
/// block by block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskBus {
    /// virtio-blk. The default.
    Virtio,
    /// virtio-scsi. Attaches as a SCSI disk.
    Scsi,
    /// virtio-pmem. A persistent memory region, mapped rather than read as blocks.
    Pmem,
    /// An optical drive. Read-only by nature, so `readonly` is forced on.
    Cdrom,
}

impl Default for DiskBus {
    fn default() -> Self {
        DiskBus::Virtio
    }
}

impl DiskBus {
    /// Whether the bus can be a root device.
    ///
    /// A CDROM cannot: the guest mounts the filesystem on it, but nothing boots from an
    /// optical drive here.
    pub fn can_be_root(self) -> bool {
        !matches!(self, DiskBus::Cdrom)
    }

    /// The name to show in a list.
    pub fn name(self) -> &'static str {
        match self {
            DiskBus::Virtio => "virtio",
            DiskBus::Scsi => "scsi",
            DiskBus::Pmem => "pmem",
            DiskBus::Cdrom => "cdrom",
        }
    }
}

/// The on-disk format of an image.
///
/// Only what crosvm can actually open is listed. crosvm reads raw and qcow2; qcow2 is the
/// one that supports backing files, so it is what an overlay is built on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskFormat {
    /// A plain image. Sparse on most filesystems.
    Raw,
    /// qcow2, which supports backing files and compression.
    Qcow2,
}

impl Default for DiskFormat {
    fn default() -> Self {
        DiskFormat::Raw
    }
}

impl DiskFormat {
    /// Guess from a file's extension, falling back to raw.
    pub fn from_path(path: &Path) -> Self {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref()
        {
            Some("qcow2") | Some("qcow") => DiskFormat::Qcow2,
            _ => DiskFormat::Raw,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            DiskFormat::Raw => "raw",
            DiskFormat::Qcow2 => "qcow2",
        }
    }

    /// The extension a new image of this format should have.
    pub fn extension(self) -> &'static str {
        match self {
            DiskFormat::Raw => "img",
            DiskFormat::Qcow2 => "qcow2",
        }
    }
}

/// A disk attached to a machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskConfig {
    /// The image file.
    pub path: PathBuf,

    /// Mount it read-only. Also the only safe way to attach an image that is
    /// shared between machines.
    #[serde(default)]
    pub readonly: bool,

    /// Attach it as the root device, so the kernel mounts it as `/` instead of
    /// needing a `root=` on the command line.
    #[serde(default)]
    pub root: bool,

    /// How to present it.
    #[serde(default, skip_serializing_if = "is_default_bus")]
    pub bus: DiskBus,

    /// Free-form label, purely for the operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

fn is_default_bus(b: &DiskBus) -> bool {
    *b == DiskBus::Virtio
}

/// How much the guest may cache of a shared directory.
///
/// Only meaningful for virtio-fs. `auto` gives close-to-open consistency, which is what a
/// person expects from a shared folder; `always` is faster and can serve stale data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SharedDirCache {
    Never,
    Auto,
    Always,
}

impl Default for SharedDirCache {
    fn default() -> Self {
        SharedDirCache::Auto
    }
}

impl SharedDirCache {
    pub fn name(self) -> &'static str {
        match self {
            SharedDirCache::Never => "never",
            SharedDirCache::Auto => "auto",
            SharedDirCache::Always => "always",
        }
    }
}

/// A directory shared with the guest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedDirConfig {
    /// The directory on the host.
    pub path: PathBuf,
    /// The tag the guest mounts it by.
    pub tag: String,
    /// Read-only?
    #[serde(default)]
    pub readonly: bool,
    /// How much the guest may cache.
    #[serde(default, skip_serializing_if = "is_default_cache")]
    pub cache: SharedDirCache,
}

fn is_default_cache(c: &SharedDirCache) -> bool {
    *c == SharedDirCache::Auto
}

/// Where the guest's console goes.
///
/// crosvm can send a serial port several places, and which one is wanted depends on what
/// is watching: a person at a terminal, a file for later, or a pipe for a program.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "backend", content = "path")]
pub enum SerialBackend {
    /// Straight to this process's standard output. What a foreground run uses.
    Stdout,
    /// Append to a file. Useful when nobody is watching but the output matters.
    File(PathBuf),
    /// A pseudo-terminal, whose name is printed so something can attach to it.
    Pty,
    /// A Unix stream socket, for a client this tool does not know about.
    Unix(PathBuf),
    /// A Unix datagram socket.
    UnixStream(PathBuf),
}

impl Default for SerialBackend {
    fn default() -> Self {
        // `hardware=serial,console` plus `type=stdout` is what the hub's console socket
        // path builds on, so stdout stays the default.
        SerialBackend::Stdout
    }
}

/// The console.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerialConfig {
    /// Where the guest's serial port goes.
    #[serde(default)]
    pub backend: SerialBackend,

    /// Also keep the kernel's log ring across reboots, via pstore.
    ///
    /// The guest needs `ramoops` set up for this to have anywhere to write, so it is a
    /// record of a crash only when the guest kernel supports it. When it does not, the
    /// option is inert rather than harmful.
    #[serde(default)]
    pub pstore: bool,
}

impl Default for SerialConfig {
    fn default() -> Self {
        Self {
            backend: SerialBackend::Stdout,
            pstore: false,
        }
    }
}

/// Guest networking.
///
/// This is not NAT. The kernel has no `nf_nat`, so the guest is given an address on the
/// host's own subnet instead and the host forwards; see `net.rs` for why that works and
/// what it costs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetConfig {
    /// Attach a network device.
    #[serde(default)]
    pub enabled: bool,

    /// The tap interface, on the host side.
    #[serde(default = "default_tap")]
    pub tap: String,

    /// Which guest address to use, as the last octet of the host's subnet. Two machines
    /// on one host need different values.
    #[serde(default)]
    pub index: u8,

    /// The interface name the guest should configure.
    #[serde(default = "default_guest_iface")]
    pub guest_interface: String,
}

fn default_tap() -> String {
    crate::net::TAP_NAME.to_string()
}

fn default_guest_iface() -> String {
    "eth0".to_string()
}

/// Leave the section out of a definition that takes all the defaults.
fn is_default_serial(s: &SerialConfig) -> bool {
    s.backend == SerialBackend::Stdout && !s.pstore
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            tap: default_tap(),
            index: 0,
            guest_interface: default_guest_iface(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmConfig {
    pub name: String,

    #[serde(default = "default_cpus")]
    pub cpus: u32,

    /// Total guest memory in MiB, before the swiotlb window is taken out.
    #[serde(default = "default_memory")]
    pub memory_mib: u64,

    #[serde(default)]
    pub hypervisor: HypervisorConfig,

    pub boot: BootConfig,

    #[serde(default)]
    pub console: ConsoleConfig,

    /// The console's own settings: where the serial port goes, and whether to keep
    /// the kernel's log across reboots.
    #[serde(default, skip_serializing_if = "is_default_serial")]
    pub serial: SerialConfig,

    /// Disk images.
    ///
    /// virtio-blk works here; see `docs/virtio-on-gunyah.md` for what did not and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disks: Vec<DiskConfig>,

    /// Directories shared with the guest over virtio-fs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared_dirs: Vec<SharedDirConfig>,

    /// Guest networking.
    #[serde(default, skip_serializing_if = "is_default_net")]
    pub network: NetConfig,

    /// A virtual GPU.
    #[serde(default, skip_serializing_if = "is_default_gpu")]
    pub gpu: GpuConfig,

    /// Free-form description, purely for the operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Leave the section out of a definition that does not use it.
fn is_default_gpu(g: &GpuConfig) -> bool {
    !g.enabled
}

/// A virtio-gpu device, and the window it draws into.
///
/// Two things have to line up for this to be useful, and both are easy to get wrong:
///
/// * the guest kernel must have `CONFIG_DRM_VIRTIO_GPU` built in. Our guest images boot from
///   an initramfs with no module tree, so a modular driver is simply absent and the device
///   is enumerated with nothing bound to it. `kernels/mini-gpu2-Image` is built for this.
/// * the display has to be opened by a process that can reach the compositor. crosvm run as
///   the desktop user cannot start at all on this device -- Gunyah refuses the VM with
///   `unknown gh exit reason: 3` -- while run as root it works, and root can reach the
///   user's Wayland socket because the socket itself is world-writable. So the command
///   carries the socket path and the environment, and the daemon, which is root, runs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuConfig {
    /// Whether to attach a virtio-gpu at all. Off by default: the guest kernel has to
    /// support it, and a device nothing can drive is only noise in the log.
    #[serde(default)]
    pub enabled: bool,

    /// `2d`, `virglrenderer` or `gfxstream`. 2d needs no host GPU stack and is all that
    /// shows a framebuffer; the others want a renderer this device does not have.
    #[serde(default = "default_gpu_backend")]
    pub backend: String,

    /// Width and height of the window. Zero means "let the guest choose", which is what a
    /// kernel with fbdev emulation does on its own. Both or neither: a size with only one
    /// of the pair is not a window crosvm can describe.
    #[serde(default)]
    pub width: u32,

    #[serde(default)]
    pub height: u32,

    /// The Wayland socket to draw on. Left empty, the daemon fills in the desktop user's,
    /// which is what an operator wants almost every time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wayland_sock: Option<PathBuf>,
}

fn default_gpu_backend() -> String {
    "2d".to_string()
}

impl Default for GpuConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: default_gpu_backend(),
            width: 0,
            height: 0,
            wayland_sock: None,
        }
    }
}

/// Leave the section out of a definition that does not use it.
fn is_default_net(n: &NetConfig) -> bool {
    !n.enabled && n.index == 0
}

fn default_cpus() -> u32 {
    1
}

fn default_memory() -> u64 {
    128
}

impl VmConfig {
    /// Guest RAM the kernel will actually see.
    ///
    /// Gunyah's static swiotlb is carved out of the guest memory region, so this
    /// is what remains for the kernel.
    pub fn guest_memory_mib(&self) -> u64 {
        self.memory_mib.saturating_sub(self.hypervisor.swiotlb_mib)
    }

    /// Check the definition for problems that would only show up at start time.
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            bail!("vm name must not be empty");
        }
        if !self
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            bail!(
                "vm name {:?} may only contain letters, digits, '-', '_' and '.'",
                self.name
            );
        }
        if self.cpus == 0 {
            bail!("cpus must be at least 1");
        }
        if self.memory_mib == 0 {
            bail!("memory_mib must be at least 1");
        }

        if self.hypervisor.protection != Protection::None
            && self.hypervisor.swiotlb_mib >= self.memory_mib
        {
            bail!(
                "swiotlb_mib ({}) must be smaller than memory_mib ({}): crosvm computes \
                 guest RAM as memory_mib - swiotlb_mib and panics when that underflows",
                self.hypervisor.swiotlb_mib,
                self.memory_mib
            );
        }

        if self.hypervisor.protection == Protection::WithFirmware
            && self.hypervisor.firmware.is_none()
        {
            bail!("protection = \"with-firmware\" requires a firmware path");
        }

        if !self.boot.kernel.exists() {
            bail!("kernel {} does not exist", self.boot.kernel.display());
        }

        if let Some(initrd) = &self.boot.initramfs {
            if !initrd.exists() {
                bail!("initramfs {} does not exist", initrd.display());
            }
        }

        for disk in &self.disks {
            if !disk.path.exists() {
                bail!("disk {} does not exist", disk.path.display());
            }
            if !disk.path.is_file() {
                bail!("disk {} is not a regular file", disk.path.display());
            }
        }

        let roots = self.disks.iter().filter(|d| d.root).count();
        if roots > 1 {
            bail!("at most one disk can be the root device, and {roots} are marked as such");
        }
        if self.disks.iter().any(|d| d.root && d.readonly) {
            bail!("the root disk cannot be read-only");
        }
        for disk in &self.disks {
            if disk.root && !disk.bus.can_be_root() {
                bail!(
                    "{} is on the {} bus, which cannot be the root device",
                    disk.path.display(),
                    disk.bus.name()
                );
            }
        }

        // A socket path has to be somewhere that exists, or crosvm fails to bind with a
        // message that does not say which end is wrong.
        match &self.serial.backend {
            SerialBackend::Unix(p) | SerialBackend::UnixStream(p) => {
                if let Some(dir) = p.parent() {
                    if !dir.as_os_str().is_empty() && !dir.is_dir() {
                        bail!("{} does not exist, so {} cannot be bound", dir.display(), p.display());
                    }
                }
            }
            SerialBackend::File(p) => {
                if let Some(dir) = p.parent() {
                    if !dir.as_os_str().is_empty() && !dir.is_dir() {
                        bail!("{} does not exist, so {} cannot be written", dir.display(), p.display());
                    }
                }
            }
            _ => {}
        }

        for dir in &self.shared_dirs {
            if !dir.path.is_dir() {
                bail!("shared directory {} does not exist", dir.path.display());
            }
            if dir.tag.is_empty() {
                bail!("shared directory {} needs a tag", dir.path.display());
            }
            if dir.tag.contains(':') {
                bail!(
                    "the tag {:?} contains a colon, which crosvm uses as its separator",
                    dir.tag
                );
            }
        }

        Ok(())
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("serialising the vm definition")
    }

    pub fn from_toml(text: &str) -> Result<Self> {
        toml::from_str(text).context("parsing the vm definition")
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::from_toml(&text).with_context(|| format!("in {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = self.to_toml()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> VmConfig {
        VmConfig {
            name: "test".into(),
            cpus: 1,
            memory_mib: 128,
            hypervisor: HypervisorConfig::default(),
            boot: BootConfig {
                kernel: PathBuf::from("/bin/sh"), // exists on any unix
                initramfs: None,
                cmdline: default_cmdline(),
            },
            console: ConsoleConfig::default(),
            description: None,
        }
    }

    #[test]
    fn guest_memory_subtracts_swiotlb() {
        let vm = base();
        assert_eq!(vm.guest_memory_mib(), 64);
    }

    #[test]
    fn swiotlb_larger_than_memory_is_rejected() {
        let mut vm = base();
        vm.memory_mib = 32;
        vm.hypervisor.swiotlb_mib = 64;
        let err = vm.validate().unwrap_err().to_string();
        assert!(err.contains("swiotlb_mib"), "unexpected error: {err}");
    }

    #[test]
    fn unprotected_vm_ignores_swiotlb() {
        let mut vm = base();
        vm.memory_mib = 32;
        vm.hypervisor.swiotlb_mib = 64;
        vm.hypervisor.protection = Protection::None;
        assert!(vm.validate().is_ok());
    }

    #[test]
    fn with_firmware_needs_a_path() {
        let mut vm = base();
        vm.hypervisor.protection = Protection::WithFirmware;
        assert!(vm.validate().is_err());
    }

    #[test]
    fn roundtrips_through_toml() {
        let vm = base();
        let text = vm.to_toml().unwrap();
        let back = VmConfig::from_toml(&text).unwrap();
        assert_eq!(back.name, vm.name);
        assert_eq!(back.memory_mib, vm.memory_mib);
        assert_eq!(back.boot.cmdline, vm.boot.cmdline);
    }

    #[test]
    fn bad_names_are_rejected() {
        let mut vm = base();
        vm.name = "has space".into();
        assert!(vm.validate().is_err());
    }
}
