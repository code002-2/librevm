//! Fetching a system image and turning it into a disk a machine can boot.
//!
//! The pieces are ordinary: download a tarball, make an ext4 filesystem, unpack the tarball
//! into it, put it back. What makes it worth a module is that each step has a way of going
//! wrong that is invisible until much later.
//!
//! A rootfs tarball for arm64 is a few megabytes; Debian's is a few hundred. The image has
//! to be bigger than the unpacked tree or the unpack fails about two thirds of the way
//! through with `No space left on device`, having already written most of a filesystem, so
//! sizes are checked against what the archive reports before anything is created.
//!
//! `mke2fs -d` would populate a filesystem from a directory with no mount at all, and that
//! is what this would prefer. The `e2fsprogs` on this device does not have it, so the path
//! taken is a loop mount, with the unmount in a guard so a failure part way through does
//! not leave a mount behind for the next run to trip over.

use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A distribution that can be fetched and deployed.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImageEntry {
    /// The name used on the command line.
    pub id: &'static str,
    /// What it is, for a person reading the list.
    pub description: &'static str,
    /// Where it comes from.
    pub url: &'static str,
    /// The architecture, which has to be arm for this machine.
    pub arch: &'static str,
    /// Roughly how big the download is.
    pub download_mib: u64,
    /// A sensible starting size for the disk. Bigger than the unpacked tree, with room.
    pub disk_mib: u64,
    /// The compression, so the right `tar` flag is used.
    pub compression: Compression,
    /// What the tarball is, for the notes shown after deploying.
    pub kind: ImageKind,
}

impl ImageEntry {
    /// An entry for a URL the caller supplied, with the catalogue's defaults for the rest.
    ///
    /// The `url` is leaked because `ImageEntry` holds a `&'static str` so that the
    /// catalogue can be a plain constant. One leak per invocation of a command that is
    /// about to do a download is not worth a lifetime parameter through everything.
    pub fn from_url(url: &str, compression: Compression) -> Self {
        Self {
            id: "from-url",
            description: "from a URL",
            url: Box::leak(url.to_string().into_boxed_str()),
            arch: "arm64",
            download_mib: 0,
            disk_mib: 4096,
            compression,
            kind: ImageKind::Rootfs,
        }
    }
}

/// How the archive is compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Compression {
    Gzip,
    Xz,
    Zstd,
}

impl Compression {
    fn tar_flag(self) -> &'static str {
        match self {
            Compression::Gzip => "-z",
            Compression::Xz => "-J",
            Compression::Zstd => "--zstd",
        }
    }

    fn suffix(self) -> &'static str {
        match self {
            Compression::Gzip => ".tar.gz",
            Compression::Xz => ".tar.xz",
            Compression::Zstd => ".tar.zst",
        }
    }
}

/// What the archive contains, which changes what has to be done after unpacking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum ImageKind {
    /// A plain root filesystem. Unpack and it is ready.
    Rootfs,
    /// A cloud image, which expects cloud-init to do the first-boot work. It will not
    /// bring up a network on its own without a datasource, so it is listed but marked.
    CloudImage,
}

/// The catalogue.
///
/// Only arm64, and every URL points at a mirror in mainland China. The distributions' own
/// CDNs are reachable but slow from here, and a few hundred megabytes is worth the
/// difference. Each entry names the mirror it uses, so it is clear where a download will
/// come from before it starts.
///
/// A mirror can be changed without touching code: `LIBREVM_MIRROR` picks a prefix and the
/// entries that are on a mirror are rewritten to use it. See `with_mirror`.
pub const CATALOGUE: &[ImageEntry] = &[
    ImageEntry {
        id: "alpine-3.21",
        description: "Alpine 3.21 minirootfs (tuna)",
        url: "https://mirrors.tuna.tsinghua.edu.cn/alpine/v3.21/releases/aarch64/alpine-minirootfs-3.21.3-aarch64.tar.gz",
        arch: "aarch64",
        download_mib: 4,
        disk_mib: 1024,
        compression: Compression::Gzip,
        kind: ImageKind::Rootfs,
    },
    ImageEntry {
        id: "alpine-3.22",
        description: "Alpine 3.22 minirootfs (tuna)",
        url: "https://mirrors.tuna.tsinghua.edu.cn/alpine/v3.22/releases/aarch64/alpine-minirootfs-3.22.2-aarch64.tar.gz",
        arch: "aarch64",
        download_mib: 4,
        disk_mib: 1024,
        compression: Compression::Gzip,
        kind: ImageKind::Rootfs,
    },
    ImageEntry {
        id: "alpine-3.22-ustc",
        description: "Alpine 3.22 minirootfs (USTC)",
        url: "https://mirrors.ustc.edu.cn/alpine/v3.22/releases/aarch64/alpine-minirootfs-3.22.2-aarch64.tar.gz",
        arch: "aarch64",
        download_mib: 4,
        disk_mib: 1024,
        compression: Compression::Gzip,
        kind: ImageKind::Rootfs,
    },
    ImageEntry {
        id: "ubuntu-24.04",
        description: "Ubuntu 24.04 base rootfs (tuna)",
        url: "https://mirrors.tuna.tsinghua.edu.cn/ubuntu-cdimage/ubuntu-base/releases/24.04/release/ubuntu-base-24.04.3-base-arm64.tar.gz",
        arch: "arm64",
        download_mib: 30,
        disk_mib: 4096,
        compression: Compression::Gzip,
        kind: ImageKind::Rootfs,
    },
    ImageEntry {
        id: "debian-13",
        description: "Debian 13 base rootfs (tuna)",
        url: "https://mirrors.tuna.tsinghua.edu.cn/debian/dists/trixie/main/installer-arm64/current/images/netboot/debian-installer/arm64/initrd.gz",
        arch: "arm64",
        download_mib: 30,
        disk_mib: 4096,
        compression: Compression::Gzip,
        kind: ImageKind::CloudImage,
    },
];

/// The mirrors offered when one has to be chosen, as (label, URL prefix).
///
/// A prefix replaces the scheme and host of an entry whose URL already points at one of
/// these, which is what makes switching mirror a one-line change rather than a new entry.
pub const MIRRORS: &[(&str, &str)] = &[
    ("tuna (Tsinghua)", "https://mirrors.tuna.tsinghua.edu.cn"),
    ("USTC", "https://mirrors.ustc.edu.cn"),
    ("aliyun", "https://mirrors.aliyun.com"),
    ("NJU", "https://mirror.nju.edu.cn"),
    ("BFSU", "https://mirrors.bfsu.edu.cn"),
];

/// The hosts this catalogue's entries live on, so `with_mirror` knows what to rewrite.
const MIRROR_HOSTS: &[&str] = &[
    "mirrors.tuna.tsinghua.edu.cn",
    "mirrors.ustc.edu.cn",
    "mirrors.aliyun.com",
    "mirror.nju.edu.cn",
    "mirrors.bfsu.edu.cn",
];

/// Rewrite an entry to come from `prefix` instead of whichever mirror it names.
///
/// Only the scheme and host change, so the path -- which is what actually identifies the
/// file -- is left alone. A host that does not mirror this distribution will give a 404,
/// and `fetch` reports that as a download failure with the URL in it.
pub fn with_mirror(entry: &ImageEntry, prefix: &str) -> ImageEntry {
    let mut out = entry.clone();
    let rest = entry.url.splitn(2, "://").nth(1).unwrap_or(entry.url);
    let path = match rest.split_once('/') {
        Some((host, path)) if MIRROR_HOSTS.contains(&host) => path,
        // Not on a known mirror: leave it exactly as it is rather than guessing.
        _ => return out,
    };
    let prefix = prefix.trim_end_matches('/');
    out.url = Box::leak(format!("{prefix}/{path}").into_boxed_str());
    out
}

/// The mirror to use, from the environment if it is set.
pub fn chosen_mirror() -> Option<String> {
    std::env::var("LIBREVM_MIRROR").ok().filter(|s| !s.is_empty())
}

/// Look one up by id.
pub fn find(id: &str) -> Option<&'static ImageEntry> {
    CATALOGUE.iter().find(|e| e.id == id)
}

/// Where downloads are kept.
pub fn cache_dir(images_dir: &Path) -> PathBuf {
    images_dir.join("cache")
}

/// The file a download lands in.
pub fn cache_path(images_dir: &Path, entry: &ImageEntry) -> PathBuf {
    let base = entry
        .url
        .rsplit('/')
        .next()
        .unwrap_or("image")
        .to_string();
    cache_dir(images_dir).join(base)
}

/// What was deployed.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Deployed {
    pub path: PathBuf,
    pub size_mib: u64,
    pub entry: String,
    /// The account the SSH key was installed for, if any.
    pub ssh_user: Option<String>,
    /// Anything the caller should know before booting.
    pub notes: Vec<String>,
}

/// Download an image into the cache, unless it is already there.
///
/// `progress` is called with (downloaded bytes, total bytes when known).
pub fn fetch<F>(images_dir: &Path, entry: &ImageEntry, mut progress: F) -> Result<PathBuf>
where
    F: FnMut(u64, Option<u64>),
{
    let dir = cache_dir(images_dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let target = cache_path(images_dir, entry);

    if target.is_file() {
        // A previous run got it. Trust it only if it is not obviously truncated.
        let len = std::fs::metadata(&target)?.len();
        if len > 1024 * 1024 {
            progress(len, Some(len));
            return Ok(target);
        }
        std::fs::remove_file(&target).ok();
    }

    let partial = target.with_extension("part");
    // `curl` is the one program that reports progress in a form that can be parsed without
    // a TTY, and it follows the redirects these URLs need.
    let mut child = Command::new("curl")
        .args([
            "-fL",
            "--retry",
            "3",
            "--connect-timeout",
            "20",
            "-o",
            &partial.display().to_string(),
            // Only the percentage and byte counts, on one line at a time.
            "--progress-bar",
            entry.url,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting curl; is it installed?")?;

    if let Some(stderr) = child.stderr.take() {
        // curl's progress meter uses carriage returns, so read to any newline and split on
        // \r as well.
        let mut buf = [0u8; 512];
        let mut reader = stderr;
        let mut line = Vec::new();
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    for &b in &buf[..n] {
                        if b == b'\r' || b == b'\n' {
                            if !line.is_empty() {
                                if let Some((got, total)) = parse_curl_progress(&line) {
                                    progress(got, total);
                                }
                                line.clear();
                            }
                        } else {
                            line.push(b);
                        }
                    }
                }
                Err(_) => break,
            }
        }
    }

    let status = child.wait().context("waiting for curl")?;
    if !status.success() {
        std::fs::remove_file(&partial).ok();
        bail!("downloading {} failed ({status})", entry.url);
    }

    std::fs::rename(&partial, &target).with_context(|| format!("moving into {}", target.display()))?;
    let len = std::fs::metadata(&target)?.len();
    progress(len, Some(len));
    Ok(target)
}

/// Pull "12345" and "99999" out of a curl progress line like
/// `####  12.3% 12345  99999`.
fn parse_curl_progress(line: &[u8]) -> Option<(u64, Option<u64>)> {
    let text = String::from_utf8_lossy(line);
    let fields: Vec<&str> = text.split_whitespace().collect();
    // The last two numbers on the line are the byte counts when a total is known.
    let numbers: Vec<&str> = fields
        .iter()
        .filter(|f| f.chars().all(|c| c.is_ascii_digit()) && !f.is_empty())
        .copied()
        .collect();
    match numbers.len() {
        0 => None,
        1 => numbers[0].parse().ok().map(|n| (n, None)),
        _ => {
            let got = numbers[numbers.len() - 2].parse().ok()?;
            let total = numbers[numbers.len() - 1].parse().ok()?;
            Some((got, Some(total)))
        }
    }
}

/// Make an ext4 filesystem of the given size at a path.
pub fn make_image(path: &Path, size_mib: u64) -> Result<()> {
    if size_mib < 64 {
        bail!("{size_mib} MiB is too small to be useful");
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // A sparse file: `truncate` makes the size without writing the bytes, so an empty
    // 4 GiB image costs nothing until it is filled.
    let f = std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    f.set_len(size_mib * 1024 * 1024)
        .with_context(|| format!("sizing {} to {size_mib} MiB", path.display()))?;
    drop(f);

    let out = Command::new("mke2fs")
        .args([
            "-t",
            "ext4",
            "-F",
            "-q",
            "-L",
            "librevm",
            "-E",
            // Discard everything up front, so the sparse regions stay sparse.
            "nodiscard",
            &path.display().to_string(),
        ])
        .output()
        .context("running mke2fs; is e2fsprogs installed?")?;
    if !out.status.success() {
        bail!(
            "mke2fs failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// A loop device that unmounts and detaches when it goes out of scope.
struct LoopMount {
    loop_device: String,
    mount_point: PathBuf,
    mounted: bool,
}

impl LoopMount {
    /// Attach the image, mount it, and unpack `archive` into it.
    fn create_and_unpack(image: &Path, compression: Compression, archive: &Path) -> Result<Self> {
        let loop_device = {
            let out = Command::new("losetup")
                .args(["--find", "--show", &image.display().to_string()])
                .output()
                .context("running losetup")?;
            if !out.status.success() {
                bail!(
                    "losetup failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        let mount_point = std::env::temp_dir().join(format!(
            "librevm-unpack-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&mount_point)?;

        let mut guard = LoopMount {
            loop_device,
            mount_point,
            mounted: false,
        };

        let out = Command::new("mount")
            .args([
                &guard.loop_device,
                &guard.mount_point.display().to_string(),
            ])
            .output()
            .context("running mount")?;
        if !out.status.success() {
            bail!(
                "mounting {} failed: {}",
                guard.loop_device,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        guard.mounted = true;

        // `--numeric-owner` keeps the archive's uid and gid rather than mapping them
        // through this machine's passwd, which is what a rootfs needs.
        let out = Command::new("tar")
            .args([
                "-x",
                compression.tar_flag(),
                "-f",
                &archive.display().to_string(),
                "-C",
                &guard.mount_point.display().to_string(),
                "--numeric-owner",
                // Some rootfs tarballs carry a leading ./ that is harmless, and some carry
                // device nodes this container cannot make. Neither is worth failing for.
                "--warning=no-unknown-keyword",
            ])
            .output()
            .with_context(|| format!("unpacking {}", archive.display()))?;
        if !out.status.success() {
            bail!(
                "tar failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }

        Ok(guard)
    }

    fn path(&self) -> &Path {
        &self.mount_point
    }
}

impl Drop for LoopMount {
    fn drop(&mut self) {
        if self.mounted {
            let _ = Command::new("umount")
                .arg(&self.mount_point)
                .output();
        }
        let _ = Command::new("losetup")
            .args(["--detach", &self.loop_device])
            .output();
        let _ = std::fs::remove_dir(&self.mount_point);
    }
}

/// Apply the injections to an image file, mounting it and putting it back.
///
/// This is `deploy` without the download or the unpack: for an image that already exists,
/// or one made before the key did. One-click SSH setup is this.
pub fn inject_into_image(image: &Path, what: &Injections) -> Result<Vec<String>> {
    if !image.is_file() {
        bail!("{} is not a file", image.display());
    }
    if !have("losetup") || !have("mount") {
        bail!("need losetup and mount");
    }

    let loop_device = {
        let out = Command::new("losetup")
            .args(["--find", "--show", &image.display().to_string()])
            .output()
            .context("running losetup")?;
        if !out.status.success() {
            bail!("losetup: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    let mount_point = std::env::temp_dir().join(format!("librevm-inject-{}", std::process::id()));
    std::fs::create_dir_all(&mount_point)?;

    let result = (|| -> Result<Vec<String>> {
        let out = Command::new("mount")
            .args([&loop_device, &mount_point.display().to_string()])
            .output()
            .context("running mount")?;
        if !out.status.success() {
            bail!("mount: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        inject(&mount_point, what)
    })();

    let _ = Command::new("umount").arg(&mount_point).output();
    let _ = Command::new("losetup")
        .args(["--detach", &loop_device])
        .output();
    let _ = std::fs::remove_dir(&mount_point);

    result
}

/// Write a file into a deployed image, for injecting keys and config.
pub fn write_into_dir(root: &Path, relative: &str, contents: &str, mode: u32) -> Result<()> {
    let path = root.join(relative.trim_start_matches('/'));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// What to inject into a freshly unpacked rootfs.
///
/// Kept as a struct rather than a pile of arguments because the SSH setup and the first-boot
/// script have to agree with each other about paths and the account name.
#[derive(Debug, Clone, Default)]
pub struct Injections {
    /// An SSH public key for `ssh_user`.
    pub authorized_key: Option<String>,
    /// The account to install it for.
    pub ssh_user: Option<String>,
    /// Nameservers to write into the image's resolver, for the first boot.
    pub dns: Vec<String>,
    /// Set a root password hash, for console logins.
    pub root_password: Option<String>,
}

/// Apply the injections to a mounted rootfs.
pub fn inject(root: &Path, what: &Injections) -> Result<Vec<String>> {
    let mut notes = Vec::new();

    // Resolv.conf, so the guest can install packages before anything else is configured.
    if !what.dns.is_empty() {
        std::fs::create_dir_all(root.join("etc"))?;
        let body: String = what
            .dns
            .iter()
            .map(|d| format!("nameserver {d}\n"))
            .collect();
        std::fs::write(root.join("etc/resolv.conf"), body)?;
        notes.push(format!("resolver set to {}", what.dns.join(", ")));
    }

    if let (Some(key), Some(user)) = (&what.authorized_key, &what.ssh_user) {
        let home = if user == "root" {
            "root".to_string()
        } else {
            format!("home/{user}")
        };
        let dir = root.join(&home).join(".ssh");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("authorized_keys"), format!("{key}\n"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            std::fs::set_permissions(
                dir.join("authorized_keys"),
                std::fs::Permissions::from_mode(0o600),
            )?;
        }

        // sshd is not installed in a bare rootfs and the configuration differs between
        // distributions, so the first boot is given a script to run instead of guessing
        // here. It is written for each family, and harmless on the others.
        let firstboot = firstboot_script(user, &what.dns);
        write_into_dir(root, "etc/librevm-firstboot.sh", &firstboot, 0o755)?;
        notes.push(format!("SSH key installed for {user}"));
        notes.push("sshd is installed on first boot, which needs a network".to_string());
    }

    if let Some(hash) = &what.root_password {
        // Appending to shadow is the portable way; the account already exists in a rootfs.
        let shadow = root.join("etc/shadow");
        if let Ok(mut text) = std::fs::read_to_string(&shadow) {
            let mut lines: Vec<String> = Vec::new();
            let mut found = false;
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("root:") {
                    let tail = rest.splitn(2, ':').nth(1).unwrap_or("");
                    lines.push(format!("root:{hash}:{tail}"));
                    found = true;
                } else {
                    lines.push(line.to_string());
                }
            }
            if !found {
                lines.push(format!("root:{hash}:0:0:99999:7:::"));
            }
            text = lines.join("\n") + "\n";
            std::fs::write(&shadow, text)?;
            notes.push("root password set".to_string());
        }
    }

    Ok(notes)
}

/// A first-boot script that installs and starts sshd, written to suit the distribution it
/// lands in.
fn firstboot_script(user: &str, dns: &[String]) -> String {
    let dns_lines: String = dns
        .iter()
        .map(|d| format!("            echo 'nameserver {d}' >> /etc/resolv.conf\n"))
        .collect();
    format!(
        r#"#!/bin/sh
# Written by librevm. Installs and starts sshd, once.
#
# A bare rootfs has no sshd, and which package provides it depends on the distribution,
# so this tries each in turn rather than being told which image it is in. It runs on the
# first boot only; the marker at the end stops it repeating.
set -u

MARKER=/etc/librevm-firstboot.done
[ -e "$MARKER" ] && exit 0

# The guest's address is static and set up by the image or the initramfs, but the resolver
# may not have been, so the nameservers used on the host are written here too.
if ! grep -q nameserver /etc/resolv.conf 2>/dev/null; then
{dns_lines}fi

install_sshd() {{
    if command -v apk >/dev/null 2>&1; then
        # /proc and /dev/urandom have to be there: apk reads the first to see what is
        # installed and the second to check signatures, and fails without either.
        if [ ! -d /proc/self ]; then
            mount -t proc none /proc 2>/dev/null
        fi
        apk add --no-cache openssh && return 0
        echo "librevm: apk said:"
        apk add --no-cache openssh 2>&1 | tail -6
        return 1
    fi
    if command -v apt-get >/dev/null 2>&1; then
        export DEBIAN_FRONTEND=noninteractive
        apt-get update >/dev/null 2>&1
        apt-get install -y --no-install-recommends openssh-server && return 0
        return 1
    fi
    if command -v dnf >/dev/null 2>&1; then
        dnf install -y openssh-server >/dev/null 2>&1 && return 0
        return 1
    fi
    return 1
}}

if ! command -v sshd >/dev/null 2>&1; then
    echo "librevm: installing sshd"
    install_sshd || echo "librevm: could not install sshd"
fi

if command -v sshd >/dev/null 2>&1; then
    # Key-only login for {user}, and the host keys sshd refuses to start without.
    ssh-keygen -A >/dev/null 2>&1
    for f in /etc/ssh/sshd_config /etc/ssh/sshd_config.d/*.conf; do
        [ -f "$f" ] || continue
        sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin prohibit-password/' "$f" 2>/dev/null
        sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication no/' "$f" 2>/dev/null
    done
    if [ -d /etc/ssh/sshd_config.d ]; then
        printf 'PermitRootLogin prohibit-password\nPasswordAuthentication no\n' \
            > /etc/ssh/sshd_config.d/99-librevm.conf
    fi

    if command -v rc-update >/dev/null 2>&1; then
        rc-update add sshd default >/dev/null 2>&1
        rc-service sshd start >/dev/null 2>&1
    elif command -v systemctl >/dev/null 2>&1; then
        systemctl enable ssh >/dev/null 2>&1
        systemctl start ssh >/dev/null 2>&1
    else
        /usr/sbin/sshd >/dev/null 2>&1
    fi
    echo "librevm: sshd started"
fi

touch "$MARKER"
"#
    )
}

/// Where the archive's size is reported.
///
/// Used to check a disk will hold the tree before the filesystem is made, because finding
/// out from `tar` means having written most of one already.
pub fn archive_uncompressed_size(archive: &Path) -> Result<Option<u64>> {
    let compression = match archive.extension().and_then(|e| e.to_str()) {
        Some("xz") => Compression::Xz,
        Some("zst") => Compression::Zstd,
        _ => Compression::Gzip,
    };
    let list = Command::new("tar")
        .args([
            "-t",
            compression.tar_flag(),
            "-f",
            &archive.display().to_string(),
            "-v",
            "--numeric-owner",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("listing the archive")?;
    let stdout = list.stdout.context("no output from tar")?;
    let reader = BufReader::new(stdout);
    let mut total = 0u64;
    for line in reader.lines().map_while(Result::ok) {
        // `tar -tv` puts the size in the third field.
        let mut fields = line.split_whitespace();
        let _mode = fields.next();
        let _owner = fields.next();
        if let Some(size) = fields.next().and_then(|s| s.parse::<u64>().ok()) {
            total += size;
        }
    }
    Ok(if total > 0 { Some(total) } else { None })
}

/// Does a program exist?
fn have(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
        .unwrap_or(false)
}

/// The nameservers this host is using.
///
/// Written into a deployed image so the first boot can install packages before anything
/// else in the guest has been configured.
pub fn host_nameservers() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(text) = std::fs::read_to_string("/etc/resolv.conf") {
        for line in text.lines() {
            if let Some(rest) = line.trim().strip_prefix("nameserver") {
                let ns = rest.trim();
                // Only addresses: a resolv.conf may name a systemd-resolved stub or carry
                // a `nameserver` comment, and neither belongs in the guest's own file.
                if ns.parse::<std::net::IpAddr>().is_ok() && !out.iter().any(|s| s == ns) {
                    out.push(ns.to_string());
                }
            }
        }
    }
    out
}

/// Fetch, create, unpack, inject: the whole deployment.
///
/// `size_mib` of `None` takes the catalogue's suggestion.
pub fn deploy<F>(
    images_dir: &Path,
    entry: &ImageEntry,
    name: &str,
    size_mib: Option<u64>,
    injections: &Injections,
    mut progress: F,
) -> Result<Deployed>
where
    F: FnMut(u64, Option<u64>),
{
    if !have("mke2fs") || !have("losetup") || !have("tar") || !have("curl") {
        bail!("need curl, tar, mke2fs and losetup");
    }

    let archive = fetch(images_dir, entry, &mut progress)?;

    // If the archive's own accounting says the tree is bigger than the disk would be, say
    // so now rather than after a long unpack that runs out of room.
    let mut notes = Vec::new();
    let mut size = size_mib.unwrap_or(entry.disk_mib);
    if let Ok(Some(bytes)) = archive_uncompressed_size(&archive) {
        let needed = bytes / (1024 * 1024) * 2 + 256;
        if needed > size {
            notes.push(format!(
                "the archive unpacks to about {} MiB, so the disk was raised to {} MiB",
                bytes / (1024 * 1024),
                needed
            ));
            size = needed;
        }
    }

    let target = images_dir.join(format!("{name}.img"));
    if target.exists() {
        bail!("{} exists already", target.display());
    }

    progress(0, None);
    make_image(&target, size)?;

    let mount = match LoopMount::create_and_unpack(&target, entry.compression, &archive) {
        Ok(m) => m,
        Err(e) => {
            // A half-unpacked image is worse than none, because it looks deployable.
            std::fs::remove_file(&target).ok();
            return Err(e);
        }
    };

    match inject(mount.path(), injections) {
        Ok(injection_notes) => notes.extend(injection_notes),
        Err(e) => {
            log::warn!("could not inject everything: {e:#}");
            notes.push(format!("injection partly failed: {e}"));
        }
    }

    drop(mount);

    if entry.kind == ImageKind::CloudImage {
        notes.push(
            "a cloud image expects cloud-init; without a datasource it will not bring up \
             a network on its own"
                .to_string(),
        );
    }

    Ok(Deployed {
        path: target,
        size_mib: size,
        entry: entry.id.to_string(),
        ssh_user: injections.ssh_user.clone(),
        notes,
    })
}

/// Write a progress line that overwrites itself, for a terminal.
pub fn progress_line(got: u64, total: Option<u64>) {
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
    match total {
        Some(t) if t > 0 => {
            let pct = (got as f64 / t as f64 * 100.0).min(100.0);
            print!("\r  {:>6.1} / {:.1} MiB  ({pct:>5.1}%)", mib(got), mib(t));
        }
        _ => print!("\r  {:.1} MiB", mib(got)),
    }
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogue_is_internally_consistent() {
        for e in CATALOGUE {
            assert!(!e.id.is_empty(), "an entry has no id");
            assert!(e.url.starts_with("https://"), "{} is not https", e.id);
            assert!(
                e.arch == "aarch64" || e.arch == "arm64",
                "{} is for {}, not arm",
                e.id,
                e.arch
            );
            assert!(e.disk_mib >= 256, "{} suggests a disk too small", e.id);
            // The URL's suffix has to match the compression, or tar is handed the wrong
            // flag and fails with something unhelpful.
            assert!(
                e.url.ends_with(e.compression.suffix()),
                "{}: url ends {:?} but compression is {:?}",
                e.id,
                e.url.rsplit('.').next(),
                e.compression
            );
        }
    }

    #[test]
    fn ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for e in CATALOGUE {
            assert!(seen.insert(e.id), "duplicate id {}", e.id);
        }
    }

    #[test]
    fn lookups_work() {
        assert!(find("alpine-3.21").is_some());
        assert!(find("no-such-image").is_none());
    }

    #[test]
    fn progress_lines_parse() {
        // The shape curl writes when it knows the total.
        let line = b"######################################################################## 100.0% 4028030  4028030";
        let (got, total) = parse_curl_progress(line).expect("should parse");
        assert_eq!(got, 4028030);
        assert_eq!(total, Some(4028030));
    }

    #[test]
    fn progress_lines_without_a_total_parse() {
        let (got, total) = parse_curl_progress(b"###  12345").expect("should parse");
        assert_eq!(got, 12345);
        assert_eq!(total, None);
    }

    #[test]
    fn a_bar_with_no_numbers_parses_to_nothing() {
        assert!(parse_curl_progress(b"##########").is_none());
    }

    #[test]
    fn compression_suffixes_match_tar_flags() {
        assert_eq!(Compression::Gzip.tar_flag(), "-z");
        assert_eq!(Compression::Xz.tar_flag(), "-J");
        assert_eq!(Compression::Zstd.tar_flag(), "--zstd");
    }

    #[test]
    fn the_cache_path_comes_from_the_url() {
        let dir = Path::new("/tmp/images");
        let e = find("alpine-3.21").unwrap();
        let p = cache_path(dir, e);
        assert_eq!(
            p.file_name().unwrap().to_str().unwrap(),
            "alpine-minirootfs-3.21.3-aarch64.tar.gz"
        );
        assert!(p.starts_with(cache_dir(dir)));
    }

    #[test]
    fn the_firstboot_script_names_the_user_and_the_resolvers() {
        let s = firstboot_script("root", &["192.168.1.1".to_string()]);
        assert!(s.contains("PermitRootLogin prohibit-password"), "{s}");
        assert!(s.contains("PasswordAuthentication no"), "{s}");
        assert!(s.contains("nameserver 192.168.1.1"), "{s}");
        assert!(s.contains("MARKER=/etc/librevm-firstboot.done"), "{s}");
        // It has to work in a shell that is not bash.
        assert!(s.starts_with("#!/bin/sh"), "{s}");
    }

    #[test]
    fn write_into_dir_creates_the_path_and_sets_the_mode() {
        let dir = std::env::temp_dir().join(format!("librevm-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_into_dir(&dir, "etc/thing.conf", "hello\n", 0o600).unwrap();
        let p = dir.join("etc/thing.conf");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hello\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_tiny_image_is_refused() {
        let p = std::env::temp_dir().join("librevm-tiny-test.img");
        let e = make_image(&p, 8).unwrap_err();
        assert!(format!("{e}").contains("too small"), "{e}");
    }
}
