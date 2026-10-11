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

    /// Free-form description, purely for the operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
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
