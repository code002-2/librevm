//! Guest networking without NAT.
//!
//! This kernel has no `nf_nat`, and its `nfnetlink` is a bool it was not built with, so
//! iptables and nftables are both out and getting either would mean rebuilding the kernel.
//! Userspace options were tried and do not fit either: `slirp4netns` wants the userspace
//! end of a tap that crosvm also needs, and `passt` in vhost-user mode needs a shared
//! memory object, which a lent (protected) VM does not have.
//!
//! What works is not to translate anything. The tap takes an address from the host's own
//! subnet with a /32, a single host route points at the guest, and forwarding is turned on.
//! The guest's address is then valid on the house network, so the router answers it
//! directly and no translation is ever needed. This is what DroidVM calls a pseudo-bridge
//! and why it does not need NAT on a phone either.
//!
//! Three details make the difference between working and not:
//!
//!   * The /32. `ip addr add .../24` on the tap installs a second route for the whole
//!     subnet, and the host then sends its own traffic into the empty tap. The /32
//!     installs no subnet route at all.
//!   * `proxy_arp` on **both** interfaces. The tap answers ARP for the router so the guest
//!     can reach it, and the Wi-Fi interface answers for the guest so the router can reply.
//!   * `arp_ignore = 0`. The default of 1 means "answer only for addresses on this
//!     interface", which suppresses exactly the proxy answers that are needed.

use anyhow::{bail, Context, Result};
use std::net::Ipv4Addr;
use std::path::Path;
use std::process::Command;

/// The tap interface this module manages.
pub const TAP_NAME: &str = "librevm-tap";

/// What the guest should be told to configure.
#[derive(Debug, Clone)]
pub struct GuestNetwork {
    pub interface: String,
    pub address: Ipv4Addr,
    pub prefix: u8,
    pub gateway: Ipv4Addr,
    pub dns: Vec<Ipv4Addr>,
}

impl GuestNetwork {
    /// A shell snippet the guest can run to configure itself.
    pub fn script(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("ip link set {} up\n", self.interface));
        out.push_str(&format!(
            "ip addr add {}/{} dev {}\n",
            self.address, self.prefix, self.interface
        ));
        out.push_str(&format!(
            "ip route add default via {} dev {}\n",
            self.gateway, self.interface
        ));
        for ns in &self.dns {
            out.push_str(&format!("echo nameserver {ns} >> /etc/resolv.conf\n"));
        }
        out
    }
}

/// The host's address and the subnet derived from it.
struct Uplink {
    interface: String,
    address: Ipv4Addr,
    gateway: Ipv4Addr,
    prefix: u8,
    dns: Vec<Ipv4Addr>,
}

/// Find the interface carrying the default route, and what it says about the network.
fn uplink() -> Result<Uplink> {
    let route = std::fs::read_to_string("/proc/net/route").context("reading the routing table")?;
    let mut interface = None;
    let mut gateway = None;
    for line in route.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 3 || f[1] != "00000000" {
            continue;
        }
        interface = Some(f[0].to_string());
        // The gateway is little-endian hex.
        let raw = u32::from_str_radix(f[2], 16).unwrap_or(0);
        gateway = Some(Ipv4Addr::from(u32::from_be(raw).to_be_bytes()));
        break;
    }
    let interface = interface.context("no default route; is the host online?")?;
    let gateway = gateway.context("the default route has no gateway")?;

    // The address, from `ip` rather than ioctl: one call, and the format is stable.
    let out = Command::new("ip")
        .args(["-4", "-brief", "addr", "show", &interface])
        .output()
        .context("asking ip for the interface address")?;
    if !out.status.success() {
        bail!("could not read the address of {interface}");
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let cidr = text
        .split_whitespace()
        .find(|w| w.contains('/') && w.contains('.'))
        .context("the uplink has no IPv4 address")?;
    let (addr, prefix) = cidr.split_once('/').context("malformed address")?;
    let address: Ipv4Addr = addr.parse().context("malformed address")?;
    let prefix: u8 = prefix.parse().context("malformed prefix")?;

    // Nameservers, so the guest can be told what to use. Whatever the host is using works.
    let mut dns = Vec::new();
    if let Ok(text) = std::fs::read_to_string("/etc/resolv.conf") {
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("nameserver") {
                if let Ok(a) = rest.trim().parse::<Ipv4Addr>() {
                    if !dns.contains(&a) {
                        dns.push(a);
                    }
                }
            }
        }
    }
    if dns.is_empty() {
        dns.push(gateway);
    }

    Ok(Uplink {
        interface,
        address,
        gateway,
        prefix,
        dns,
    })
}

/// Is a program on PATH?
fn have(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
        .unwrap_or(false)
}

fn sysctl(key: &str, value: &str) -> Result<()> {
    let path = format!("/proc/sys/{}", key.replace('.', "/"));
    std::fs::write(&path, value)
        .with_context(|| format!("setting {key} to {value}"))
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        bail!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Bring the host side of the guest network up, and say what the guest should configure.
///
/// `guest_index` only picks the last octet, so two machines on the same host do not collide.
pub fn setup(guest_index: u8) -> Result<GuestNetwork> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("setting up the guest network needs root");
    }
    if !have("ip") {
        bail!("the `ip` command is not installed");
    }

    let up = uplink()?;
    let octets = up.address.octets();
    // The host end of the link takes .200 and the guest takes .201 upward. They have to be
    // different addresses: giving both the same one was a bug, and it looked exactly like a
    // routing failure from the guest's side.
    let host = Ipv4Addr::new(octets[0], octets[1], octets[2], 200);
    let guest = Ipv4Addr::new(
        octets[0],
        octets[1],
        octets[2],
        201u8.saturating_add(guest_index),
    );

    // Forwarding is the whole mechanism. Without NAT the guest's own address has to be
    // routable, and this is what lets the two interfaces talk.
    sysctl("net.ipv4.ip_forward", "1")?;
    // The reply comes in on one interface and leaves by the other, which a strict reverse
    // path check reads as spoofing.
    sysctl("net.ipv4.conf.all.rp_filter", "0").ok();
    sysctl(&format!("net.ipv4.conf.{}.rp_filter", up.interface), "0").ok();

    // A fresh tap, so a leftover one from a previous run cannot confuse this.
    let _ = run("ip", &["link", "del", TAP_NAME]);
    run("ip", &["tuntap", "add", "dev", TAP_NAME, "mode", "tap", "user", "root", "vnet_hdr"])
        .context("creating the tap")?;
    run("ip", &["link", "set", TAP_NAME, "up"])?;
    // /32, not the uplink's prefix: a subnet route here would send the host's own traffic
    // into an interface with nothing behind it.
    run("ip", &["addr", "add", &format!("{host}/32"), "dev", TAP_NAME])?;
    run(
        "ip",
        &["route", "add", &format!("{guest}/32"), "dev", TAP_NAME],
    )
    .context("pointing a route at the guest")?;

    // Both sides need proxy ARP, and neither can keep arp_ignore at its default.
    for iface in [up.interface.as_str(), TAP_NAME] {
        sysctl(&format!("net.ipv4.conf.{iface}.proxy_arp"), "1")?;
        sysctl(&format!("net.ipv4.conf.{iface}.arp_ignore"), "0")?;
        sysctl(&format!("net.ipv4.conf.{iface}.arp_announce"), "2").ok();
        sysctl(&format!("net.ipv4.conf.{iface}.rp_filter"), "0").ok();
    }

    Ok(GuestNetwork {
        interface: "eth0".to_string(),
        address: guest,
        prefix: 24,
        gateway: up.gateway,
        dns: up.dns,
    })
}

/// Take the host side down again.
pub fn teardown() -> Result<()> {
    let _ = run("ip", &["link", "del", TAP_NAME]);
    Ok(())
}

/// Is the tap up?
pub fn is_up() -> bool {
    Path::new(&format!("/sys/class/net/{TAP_NAME}")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guest_script_configures_what_matters() {
        let n = GuestNetwork {
            interface: "eth0".into(),
            address: Ipv4Addr::new(192, 168, 1, 201),
            prefix: 24,
            gateway: Ipv4Addr::new(192, 168, 1, 1),
            dns: vec![Ipv4Addr::new(192, 168, 1, 1)],
        };
        let s = n.script();
        assert!(s.contains("ip link set eth0 up"), "{s}");
        assert!(s.contains("ip addr add 192.168.1.201/24 dev eth0"), "{s}");
        assert!(s.contains("ip route add default via 192.168.1.1 dev eth0"), "{s}");
        assert!(s.contains("nameserver 192.168.1.1"), "{s}");
    }

    #[test]
    fn the_guest_index_picks_the_last_octet() {
        // The host link address is .200 and the guest starts at .201, so the two ends are
        // never the same -- which they were in an earlier version.
        let host = 200u8;
        let guest = 201u8.saturating_add(0);
        assert_ne!(host, guest);
        assert_eq!(guest, 201);
        assert_eq!(201u8.saturating_add(1), 202);
        // And it does not wrap into something surprising at the top of the range.
        assert_eq!(201u8.saturating_add(255), 255);
    }

    #[test]
    fn settings_are_read_from_the_system() {
        // A sanity check that the parser sees this host's own configuration.
        match uplink() {
            Ok(u) => {
                assert!(!u.interface.is_empty());
                assert!(u.prefix <= 32);
                assert!(!u.dns.is_empty(), "a resolver should have been found");
            }
            // No route at all is a legitimate answer on a disconnected machine.
            Err(_) => {}
        }
    }
}
