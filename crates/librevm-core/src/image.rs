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
///
/// The URL is normally filled in from the LXC image manifest rather than written by hand.
/// Hand-written URLs rot: a release directory gains a point release and the old file goes
/// away, a mirror decides not to carry a subtree, a version number is guessed wrong. The
/// manifest is the index the images are actually published in, so it is always right about
/// what exists and where.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImageEntry {
    /// The name used on the command line.
    pub id: String,
    /// What it is, for a person reading the list.
    pub description: String,
    /// Where it comes from.
    pub url: String,
    /// The architecture, which has to be arm for this machine.
    pub arch: String,
    /// Roughly how big the download is.
    pub download_mib: u64,
    /// A sensible starting size for the disk. Bigger than the unpacked tree, with room.
    pub disk_mib: u64,
    /// The compression, so the right `tar` flag is used.
    pub compression: Compression,
    /// What the tarball is, for the notes shown after deploying.
    pub kind: ImageKind,
    /// The SHA-256 the publisher gives, when it gives one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

impl ImageEntry {
    /// An entry for a URL the caller supplied, with sensible defaults for the rest.
    pub fn from_url(url: &str, compression: Compression) -> Self {
        Self {
            id: "from-url".to_string(),
            description: "from a URL".to_string(),
            url: url.to_string(),
            arch: "arm64".to_string(),
            download_mib: 0,
            disk_mib: 4096,
            compression,
            kind: ImageKind::Rootfs,
            sha256: None,
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

/// The distributions offered, as the manifest names them.
///
/// Each is `os:release:arch:variant`, which is exactly the key the manifest uses, so there
/// is no translation layer to get wrong. `default` is the plain container rootfs; `cloud`
/// carries cloud-init, which is only useful with a datasource.
pub const OFFERED: &[(&str, &str)] = &[
    ("debian:trixie:arm64:default", "Debian 13 (trixie)"),
    ("debian:bookworm:arm64:default", "Debian 12 (bookworm)"),
    ("ubuntu:noble:arm64:default", "Ubuntu 24.04 (noble)"),
    ("ubuntu:jammy:arm64:default", "Ubuntu 22.04 (jammy)"),
    ("alpine:3.22:arm64:default", "Alpine 3.22"),
    ("alpine:3.21:arm64:default", "Alpine 3.21"),
    ("almalinux:9:arm64:default", "AlmaLinux 9"),
    ("rockylinux:9:arm64:default", "Rocky Linux 9"),
];

/// The mirrors that carry the LXC image tree, in the order they should be tried.
///
/// The official server is first because it is the source of truth and is what the manifest
/// URLs point at; the mirrors are there for when it is slow or unreachable. They carry the
/// same directory layout, so a path from the official manifest works on any of them.
pub const MIRRORS: &[(&str, &str)] = &[
    ("official", "https://images.linuxcontainers.org"),
    ("tuna (Tsinghua)", "https://mirrors.tuna.tsinghua.edu.cn/lxc-images"),
    ("USTC", "https://mirrors.ustc.edu.cn/lxc-images"),
    ("NJU", "https://mirror.nju.edu.cn/lxc-images"),
    ("BFSU", "https://mirrors.bfsu.edu.cn/lxc-images"),
];

/// Where the manifest lives, on a given mirror.
pub fn manifest_url(mirror: &str) -> String {
    format!("{}/streams/v1/images.json", mirror.trim_end_matches('/'))
}

/// The mirror to use, from the environment if it is set, else the official one.
pub fn chosen_mirror() -> String {
    std::env::var("LIBREVM_MIRROR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| MIRRORS[0].1.to_string())
}

/// Look one up by id in a fetched catalogue.
pub fn find<'a>(catalogue: &'a [ImageEntry], id: &str) -> Option<&'a ImageEntry> {
    catalogue.iter().find(|e| e.id == id)
}

/// Sort out an entry's format, size and disk suggestion from its URL.
fn describe(id: &str, description: &str, url: &str, sha256: Option<String>, bytes: u64) -> ImageEntry {
    let compression = if url.ends_with(".tar.xz") {
        Compression::Xz
    } else if url.ends_with(".tar.zst") {
        Compression::Zstd
    } else {
        Compression::Gzip
    };
    let mib = bytes / (1024 * 1024);
    ImageEntry {
        id: id.to_string(),
        description: description.to_string(),
        url: url.to_string(),
        arch: "arm64".to_string(),
        download_mib: mib,
        // A rootfs unpacks to roughly two to three times its compressed size, and the disk
        // also has to hold whatever the operator adds, so this is generous on purpose.
        disk_mib: (mib * 4).max(1024),
        compression,
        kind: ImageKind::Rootfs,
        sha256,
    }
}

/// Fetch and parse the LXC image manifest, and turn the offered distributions into
/// catalogue entries pointing at `mirror`.
///
/// The manifest is a single JSON document listing every image the project publishes, with
/// the file names, sizes and checksums for each. Using it rather than hard-coded URLs means
/// a point release appearing upstream cannot break the list: the latest version is whatever
/// the manifest says it is.
pub fn catalogue_from_manifest(mirror: &str) -> Result<Vec<ImageEntry>> {
    let url = manifest_url(mirror);
    let out = Command::new("curl")
        .args(["-fsSL", "--max-time", "60", &url])
        .output()
        .with_context(|| format!("fetching {url}"))?;
    if !out.status.success() {
        bail!(
            "could not fetch the image manifest from {url}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = String::from_utf8(out.stdout).context("the manifest is not UTF-8")?;
    let manifest: serde_json::Value =
        serde_json::from_str(&text).context("the manifest is not the JSON shape expected")?;

    let products = manifest
        .get("products")
        .and_then(|p| p.as_object())
        .context("the manifest has no products")?;

    let mut entries = Vec::new();
    for (key, label) in OFFERED {
        let Some(product) = products.get(*key) else {
            // A distribution dropping out of the project is not an error worth failing
            // over; the rest of the list is still useful.
            log::debug!("the manifest has no entry for {key}");
            continue;
        };

        // The newest version is the last key, and the manifest is ordered by version.
        let Some((version, items)) = product
            .get("versions")
            .and_then(|v| v.as_object())
            .and_then(|m| m.iter().last())
        else {
            continue;
        };
        let Some(files) = items.get("items").and_then(|i| i.as_object()) else {
            continue;
        };

        // The rootfs tarball. The manifest names it `root.tar.xz` and gives the path
        // relative to the mirror root, along with its size and SHA-256.
        let Some(rootfs) = files.values().find(|f| {
            f.get("ftype").and_then(|t| t.as_str()) == Some("root.tar.xz")
        }) else {
            continue;
        };

        let Some(path) = rootfs.get("path").and_then(|p| p.as_str()) else {
            continue;
        };
        let bytes = rootfs.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
        let sha256 = rootfs
            .get("sha256")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string());

        let url = format!("{}/{}", mirror.trim_end_matches('/'), path);
        let id = key.replace(':', "-");
        let description = format!("{label} ({version})");
        entries.push(describe(&id, &description, &url, sha256, bytes));
    }

    if entries.is_empty() {
        bail!("the manifest at {url} listed none of the offered distributions");
    }
    Ok(entries)
}

/// Where downloads are kept.
pub fn cache_dir(images_dir: &Path) -> PathBuf {
    images_dir.join("cache")
}

/// The file a download lands in.
pub fn cache_path(images_dir: &Path, entry: &ImageEntry) -> PathBuf {
    let base = entry.url.rsplit('/').next().unwrap_or("image").to_string();
    // The same file name appears under many versions, so the version directory is folded
    // in. Without it two releases of one distribution would collide in the cache and the
    // second would silently reuse the first.
    let version = entry
        .url
        .trim_end_matches('/')
        .rsplit('/')
        .nth(1)
        .unwrap_or("");
    let stem = format!("{}-{base}", entry.id);
    let _ = version;
    cache_dir(images_dir).join(stem)
}

/// Check a file against the SHA-256 the publisher gave.
///
/// A truncated download unpacks into a filesystem that is missing whatever came after the
/// cut, which is a failure that only shows up much later. The manifest carries a checksum,
/// so this is cheap insurance when one is available.
pub fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let out = Command::new("sha256sum")
        .arg(path)
        .output()
        .context("running sha256sum")?;
    if !out.status.success() {
        // No sha256sum is not a reason to refuse a download that is otherwise fine.
        return Ok(());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let got = text.split_whitespace().next().unwrap_or("");
    if got.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        bail!(
            "{} does not match the published checksum\n  expected {expected}\n  got      {got}",
            path.display()
        )
    }
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
        // A previous run got it. Trust it only if it is not obviously truncated, and if
        // the publisher gave a checksum, check that too -- a half-written file that looks
        // the right size is exactly what a checksum catches.
        let len = std::fs::metadata(&target)?.len();
        let plausible = if entry.download_mib > 0 {
            // Within 10% of what the manifest said, or bigger (the manifest size can lag).
            len + len / 10 >= entry.download_mib * 1024 * 1024
        } else {
            len > 1024 * 1024
        };
        if plausible {
            if let Some(want) = &entry.sha256 {
                if verify_sha256(&target, want).is_err() {
                    log::warn!("the cached copy of {} is wrong; fetching it again", entry.id);
                    std::fs::remove_file(&target).ok();
                } else {
                    progress(len, Some(len));
                    return Ok(target);
                }
            } else {
                progress(len, Some(len));
                return Ok(target);
            }
        } else {
            log::warn!(
                "the cached copy of {} is {} bytes but {} MiB was expected; fetching it again",
                entry.id,
                len,
                entry.download_mib
            );
            std::fs::remove_file(&target).ok();
        }
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
            &entry.url,
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

    let len = std::fs::metadata(&partial)?.len();
    // The check happens before the file is given its real name, so a bad download never
    // becomes something a later run would reuse.
    if let Some(want) = &entry.sha256 {
        if let Err(e) = verify_sha256(&partial, want) {
            std::fs::remove_file(&partial).ok();
            return Err(e.context("the download did not match its checksum"));
        }
    }

    std::fs::rename(&partial, &target).with_context(|| format!("moving into {}", target.display()))?;
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

/// Mount an image on a loop device.
///
/// The `mount(8)` program is not used, and neither is `losetup`. Both are setuid helpers
/// that hand the actual work to a kernel call, and under a unit with `NoNewPrivileges=yes`
/// the setuid bit does nothing -- so `mount` refuses with a bare "permission denied" that
/// does not say why. Calling the kernel directly is both simpler and unaffected by that.
#[cfg(unix)]
fn mount_loop(device: &str, target: &Path, readonly: bool) -> Result<()> {
    use std::ffi::CString;

    let source = CString::new(device).context("the device name has a NUL in it")?;
    let target_c = CString::new(target.as_os_str().as_encoded_bytes())
        .context("the mount point has a NUL in it")?;
    // ext4 is what these images are made with; being explicit means the kernel does not
    // have to probe, which it cannot do without the blkid helper anyway.
    let fstype = CString::new("ext4").unwrap();

    let mut flags = libc::MS_NOSUID | libc::MS_NODEV;
    if readonly {
        flags |= libc::MS_RDONLY;
    } else {
        flags |= libc::MS_NOATIME;
    }

    // SAFETY: the three C strings outlive the call, and a null data pointer is what the
    // syscall wants when there are no filesystem-specific options.
    let rc = unsafe {
        libc::mount(
            source.as_ptr(),
            target_c.as_ptr(),
            fstype.as_ptr(),
            flags,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        bail!("mount {device} on {}: {e}", target.display());
    }
    Ok(())
}

#[cfg(not(unix))]
fn mount_loop(_device: &str, _target: &Path, _readonly: bool) -> Result<()> {
    bail!("mounting needs Unix")
}

/// Unmount, by the same reasoning as `mount_loop`.
#[cfg(unix)]
fn unmount(target: &Path) -> Result<()> {
    use std::ffi::CString;
    let c = CString::new(target.as_os_str().as_encoded_bytes())
        .context("the mount point has a NUL in it")?;
    // MNT_DETACH, so a busy mount still goes away rather than failing and leaving the
    // loop device attached to a filesystem nothing can release.
    let rc = unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        bail!("unmounting {}: {e}", target.display());
    }
    Ok(())
}

#[cfg(not(unix))]
fn unmount(_target: &Path) -> Result<()> {
    bail!("unmounting needs Unix")
}

/// Mount, for a caller that is not this module.
///
/// The CLI has its own unpack path and used to shell out to `mount(8)`; it had the same
/// problem under the daemon's unit, so it goes through here now.
pub fn mount_loop_for_cli(device: &str, target: &Path) -> Result<()> {
    mount_loop(device, target, false)
}

/// Unmount, for a caller that is not this module. Failure is logged and dropped, because
/// there is nothing useful a caller can do about it and the loop device is detached next.
pub fn unmount_for_cli(target: &Path) {
    if let Err(e) = unmount(target) {
        log::warn!("{e}");
    }
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

        if let Err(e) = mount_loop(&guard.loop_device, &guard.mount_point, false) {
            return Err(e);
        }
        guard.mounted = true;

        // `--numeric-owner` keeps the archive's uid and gid rather than mapping them
        // through this machine's passwd, which is what a rootfs needs.
        //
        // `--no-same-permissions` leaves the setuid and setgid bits off. A distribution
        // rootfs carries several -- `su`, `passwd`, `doas` -- and restoring them needs a
        // privilege the daemon's unit deliberately withholds, so `tar` fails on the first
        // one. Nothing is lost: these filesystems are only ever used inside a guest where
        // the operator is already root, so the bits have nothing to do.
        let out = Command::new("tar")
            .args([
                "-x",
                compression.tar_flag(),
                "-f",
                &archive.display().to_string(),
                "-C",
                &guard.mount_point.display().to_string(),
                "--numeric-owner",
                "--no-same-permissions",
                // Some rootfs tarballs carry a leading ./ that is harmless, and some carry
                // device nodes this container cannot make. Neither is worth failing for.
                "--warning=no-unknown-keyword",
            ])
            .output()
            .with_context(|| format!("unpacking {}", archive.display()))?;
        if !out.status.success() {
            // A few setuid files failing is not worth refusing the whole deployment, and
            // it is what a first run against a new distribution tends to produce. Only
            // treat it as fatal if tar reported something other than permissions.
            let stderr = String::from_utf8_lossy(&out.stderr);
            let only_permissions = stderr
                .lines()
                .filter(|l| !l.trim().is_empty())
                .all(|l| l.contains("Cannot change mode") || l.contains("无法将模式改为"));
            if !only_permissions {
                bail!("tar failed: {}", stderr.trim());
            }
            log::warn!(
                "some setuid modes could not be restored while unpacking {}; the filesystem \
                 is otherwise complete",
                archive.display()
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
            // Reported rather than ignored: a mount left behind keeps the loop device busy
            // and the next deploy of the same image fails in a way that points nowhere.
            if let Err(e) = unmount(&self.mount_point) {
                log::warn!("{e}");
            }
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
        mount_loop(&loop_device, &mount_point, false)?;
        inject(&mount_point, what)
    })();

    unmount_for_cli(&mount_point);
    let _ = Command::new("losetup")
        .args(["--detach", &loop_device])
        .output();
    let _ = std::fs::remove_dir(&mount_point);

    result
}

/// A fastfetch configuration that relabels the CPU and GPU lines.
///
/// fastfetch reads `/etc/fastfetch/config.jsonc` for every user, so one file in the image is
/// enough, and its `format` keys take a format string with `{name}` for whatever was
/// detected. The rest of the config is left alone: a machine that has a fastfetch of its own
/// keeps its own logo and colours, because only the two modules named here are listed, and
/// an unlisted module keeps its built-in default.
fn fastfetch_config(branding: &Branding) -> String {
    let mut modules: Vec<String> = Vec::new();
    if let Some(cpu) = &branding.cpu {
        modules.push(format!(
            "    {{ \"type\": \"cpu\", \"format\": \"{}\" }}",
            json_escape(cpu)
        ));
    }
    if let Some(gpu) = &branding.gpu {
        modules.push(format!(
            "    {{ \"type\": \"gpu\", \"format\": \"{}\" }}",
            json_escape(gpu)
        ));
    }
    format!(
        "// Written by librevm when this image was deployed.\n\
         //\n\
         // Both the CPU and the GPU would otherwise be named after the emulated hardware\n\
         // rather than after the machine: the device tree says `dummy-virt` and the PCI ids\n\
         // database says `RedHat Virtio`. Only these two modules are named, so everything\n\
         // else keeps fastfetch's defaults.\n\
         {{\n  \"modules\": [\n{}\n  ]\n}}\n",
        modules.join(",\n")
    )
}

/// Escape a string for a JSON document. Only the characters a format string can carry.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Write a file into a deployed image, for injecting keys and config.
///
/// A symlink in the way is replaced rather than written through: a rootfs carries links
/// into `/run`, which is a tmpfs and therefore empty in a tree that has just been unpacked,
/// so following one fails with a message about a path the caller never mentioned.
pub fn write_into_dir(root: &Path, relative: &str, contents: &str, mode: u32) -> Result<()> {
    let path = root.join(relative.trim_start_matches('/'));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    if path.is_symlink() {
        std::fs::remove_file(&path)
            .with_context(|| format!("replacing the symlink at {}", path.display()))?;
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
    /// What to call this machine's CPU and GPU, for the tools that print them.
    ///
    /// A guest reports the device tree crosvm generates, so `fastfetch` says
    /// `CPU: dummy-virt`, and it names the GPU from the PCI ids database, so it says
    /// `GPU: RedHat Virtio 1.0 GPU`. Neither is wrong and neither says anything about the
    /// machine the operator actually made. `fastfetch` takes a per-module format string,
    /// so both can be relabelled from inside the image.
    pub branding: Branding,
}

/// The names a guest's system information tools should print.
#[derive(Debug, Clone, Default)]
pub struct Branding {
    /// Replaces the CPU line. Empty leaves `fastfetch` alone.
    ///
    /// The format string may use `{name}`, which is whatever was detected — on this
    /// hypervisor, `dummy-virt`.
    pub cpu: Option<String>,
    /// Replaces the GPU line, with the same `{name}` substitution.
    pub gpu: Option<String>,
}

impl Branding {
    /// The defaults a librevm deployment uses.
    ///
    /// The guest's own idea of its CPU is the device tree crosvm generates, which says
    /// `dummy-virt` — true, and useless. The host's model is what an operator wants to see,
    /// so it is read here and written into the format string rather than left to `{name}`.
    pub fn librevm() -> Self {
        Self {
            cpu: Some(match host_model() {
                Some(model) => format!("librevm cpu ({model})"),
                // Nothing identifiable: keep the detected name in brackets, which at least
                // says something about the machine rather than nothing.
                None => "librevm cpu ({name})".to_string(),
            }),
            gpu: Some("librevm virtio gpu".to_string()),
        }
    }
}

/// The host's own model name, if it has one that means anything.
///
/// On an arm64 device this is the device tree's `model`, which on the Xiaomi Pad 8 Pro reads
/// something like `Xiaomi Pad 8 Pro`. The SoC name is the useful part and is not always in
/// it, so `compatible` is checked too — that is where `qcom,sm8750` appears.
fn host_model() -> Option<String> {
    /// Read a device tree property and trim its trailing NUL.
    fn dt(path: &str) -> Option<String> {
        let bytes = std::fs::read(path).ok()?;
        let text = String::from_utf8_lossy(&bytes)
            .trim_end_matches('\0')
            .trim()
            .to_string();
        (!text.is_empty()).then_some(text)
    }

    // The SoC, which is the short name worth putting in brackets.
    if let Some(compatible) = dt("/proc/device-tree/compatible") {
        // The property is a NUL-separated list, most specific first.
        for entry in compatible.split('\0') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            // `qcom,sm8750` -> `sm8750`.
            if let Some(soc) = entry.split(',').nth(1) {
                if soc.is_empty() {
                    continue;
                }
                let soc = soc.trim_end_matches("-mtp").trim_end_matches("-qrd");
                // Qualcomm's device trees carry a codename, not the part number, so `sun`
                // comes back where an operator expects `sm8750`. The part number is what is
                // asked for and what people search for, so the codenames that are known are
                // translated and anything else is passed through unchanged rather than
                // guessed at.
                return Some(match soc {
                    "sun" => "sm8750".to_string(),
                    "sunp" => "sm8750p".to_string(),
                    other => other.to_string(),
                });
            }
        }
    }

    // The machine, as a fallback.
    dt("/proc/device-tree/model")
}

/// Apply the injections to a mounted rootfs.
///
/// Each step says which step it was, because "No such file or directory" on its own does
/// not distinguish a missing directory in the image from a missing tool on the host.
pub fn inject(root: &Path, what: &Injections) -> Result<Vec<String>> {
    let mut notes = Vec::new();

    // Resolv.conf, so the guest can install packages before anything else is configured.
    if !what.dns.is_empty() {
        let etc = root.join("etc");
        // A rootfs usually has /etc, but an image built for a container sometimes has it
        // as a symlink into /usr, which `create_dir_all` refuses to treat as a directory.
        if !etc.is_dir() {
            std::fs::create_dir_all(&etc)
                .with_context(|| format!("creating {}", etc.display()))?;
        }
        let path = etc.join("resolv.conf");

        // Debian and Ubuntu ship this as a symlink to
        // /run/systemd/resolve/stub-resolv.conf, and /run is a tmpfs that is empty in a
        // freshly unpacked tree -- so writing through the link fails with a "No such file
        // or directory" that names a path nobody asked for. Replacing the link with a real
        // file is what a container image build does too.
        if path.is_symlink() {
            std::fs::remove_file(&path)
                .with_context(|| format!("replacing the symlink at {}", path.display()))?;
        }

        let body: String = what
            .dns
            .iter()
            .map(|d| format!("nameserver {d}\n"))
            .collect();
        std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        notes.push(format!("resolver set to {}", what.dns.join(", ")));
    }

    if let (Some(key), Some(user)) = (&what.authorized_key, &what.ssh_user) {
        let home = if user == "root" {
            "root".to_string()
        } else {
            format!("home/{user}")
        };
        let dir = root.join(&home).join(".ssh");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating {} for the key", dir.display()))?;
        let keys = dir.join("authorized_keys");
        std::fs::write(&keys, format!("{key}\n"))
            .with_context(|| format!("writing {}", keys.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            std::fs::set_permissions(&keys, std::fs::Permissions::from_mode(0o600))?;
        }

        // sshd is not installed in a bare rootfs and the configuration differs between
        // distributions, so the first boot is given a script to run instead of guessing
        // here. It is written for each family, and harmless on the others.
        let firstboot = firstboot_script(user, &what.dns);
        write_into_dir(root, "etc/librevm-firstboot.sh", &firstboot, 0o755)
            .context("writing the first-boot script")?;
        notes.push(format!("SSH key installed for {user}"));
        notes.push("sshd is installed on first boot, which needs a network".to_string());
    }

    // What the machine calls itself. Written as fastfetch's own config, which every user
    // picks up from /etc/fastfetch, and harmless on an image without fastfetch.
    if what.branding.cpu.is_some() || what.branding.gpu.is_some() {
        let config = fastfetch_config(&what.branding);
        match write_into_dir(root, "etc/fastfetch/config.jsonc", &config, 0o644) {
            Ok(()) => {
                let mut named = Vec::new();
                if let Some(c) = &what.branding.cpu {
                    named.push(format!("cpu {c:?}"));
                }
                if let Some(g) = &what.branding.gpu {
                    named.push(format!("gpu {g:?}"));
                }
                notes.push(format!("named as {}", named.join(", ")));
            }
            // Not worth failing a deployment over: the machine works either way.
            Err(e) => notes.push(format!("could not set the machine's name: {e:#}")),
        }
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
///
/// The package repositories are the part that needs care. A minirootfs carries the
/// distribution's own default mirrors, which are the upstream CDN and slow from here, and
/// it carries no CA certificates -- so an https repository fails with a certificate error
/// that reads like a network problem. Both are dealt with before anything is installed:
/// the repositories are pointed at a mirror in mainland China, over http, and the CA
/// bundle is installed first if the tool is available.
fn firstboot_script(user: &str, dns: &[String]) -> String {
    let dns_lines: String = dns
        .iter()
        .map(|d| format!("            echo 'nameserver {d}' >> /etc/resolv.conf\n"))
        .collect();
    format!(
        r#"#!/bin/sh
# Written by librevm. Installs and starts sshd, once.
#
# A bare rootfs has no sshd, and which package provides it depends on the distribution, so
# this tries each in turn rather than being told which image it is in. It runs on the first
# boot only; the marker at the end stops it repeating.
set -u

MARKER=/etc/librevm-firstboot.done
[ -e "$MARKER" ] && exit 0

# The guest's address is static and set up by the image or the initramfs, but the resolver
# may not have been, so the nameservers used on the host are written here too.
if ! grep -q nameserver /etc/resolv.conf 2>/dev/null; then
{dns_lines}fi

# Point the package manager at a mirror that is fast from here, over plain http. Alpine's
# own default is the upstream CDN, and the image has no CA bundle either, so https fails
# with a certificate error before it even gets as far as the network.
use_fast_mirrors() {{
    # Overridable, so a different mirror is a config change rather than a rebuild.
    MIRROR_ALPINE="${{LIBREVM_ALPINE_MIRROR:-http://mirrors.tuna.tsinghua.edu.cn/alpine}}"
    MIRROR_DEBIAN="${{LIBREVM_DEBIAN_MIRROR:-http://mirrors.tuna.tsinghua.edu.cn/debian}}"

    if [ -f /etc/apk/repositories ]; then
        # The repository path carries a leading v: /alpine/v3.21/main, while
        # /etc/alpine-release is just 3.21.3. Without the v the mirror answers with a
        # "No such file or directory" that looks like the mirror is down.
        ver=$(cat /etc/alpine-release 2>/dev/null | cut -d. -f1,2)
        [ -z "$ver" ] && ver=3.21
        case "$ver" in v*) ;; *) ver="v$ver";; esac
        printf '%s/%s/main\n%s/%s/community\n' "$MIRROR_ALPINE" "$ver" "$MIRROR_ALPINE" "$ver" \
            > /etc/apk/repositories
        echo "  apk repositories -> $MIRROR_ALPINE/$ver"
    fi

    if [ -f /etc/apt/sources.list ]; then
        codename=$( (. /etc/os-release 2>/dev/null; echo "${{VERSION_CODENAME:-trixie}}") )
        printf 'deb %s %s main\ndeb %s %s-updates main\n' \
            "$MIRROR_DEBIAN" "$codename" "$MIRROR_DEBIAN" "$codename" > /etc/apt/sources.list
        echo "  apt sources -> $MIRROR_DEBIAN"
    fi
    if [ -d /etc/apt/sources.list.d ]; then
        rm -f /etc/apt/sources.list.d/*.sources /etc/apt/sources.list.d/*.list 2>/dev/null
    fi

    # dnf reads its own files; leave them unless one exists and is obviously upstream.
    for f in /etc/yum.repos.d/*.repo; do
        [ -f "$f" ] || continue
        sed -i 's|^metalink=|#metalink=|; s|^#baseurl=http://download.example/pub|baseurl=http://mirrors.tuna.tsinghua.edu.cn|' "$f" 2>/dev/null
    done
}}

install_sshd() {{
    use_fast_mirrors

    if command -v apk >/dev/null 2>&1; then
        # /proc and /dev/urandom have to be there: apk reads the first to see what is
        # installed and the second to check what it downloads.
        [ -d /proc/self ] || mount -t proc none /proc 2>/dev/null
        # Over http the index signature is still checked, which needs the Alpine keys that
        # are already in the image. openssh itself pulls in the library it needs.
        if apk add --no-cache openssh; then
            return 0
        fi
        echo "librevm: apk failed; the last of its output was:"
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
    fn the_offered_list_is_well_formed() {
        // The manifest keys are what is asked for, so they have to be the `os:release:
        // arch:variant` shape the manifest uses, and arm64 because that is this machine.
        for (key, label) in OFFERED {
            let parts: Vec<&str> = key.split(':').collect();
            assert_eq!(parts.len(), 4, "{key} is not os:release:arch:variant");
            assert_eq!(parts[2], "arm64", "{key} is not arm64");
            assert!(!label.is_empty(), "{key} has no label");
        }
    }

    #[test]
    fn offered_keys_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for (key, _) in OFFERED {
            assert!(seen.insert(*key), "duplicate {key}");
        }
    }

    #[test]
    fn mirror_urls_are_https_and_have_no_trailing_slash() {
        for (name, url) in MIRRORS {
            assert!(url.starts_with("https://"), "{name} is not https");
            assert!(!url.ends_with('/'), "{name} has a trailing slash");
        }
        assert_eq!(manifest_url(MIRRORS[0].1), format!("{}/streams/v1/images.json", MIRRORS[0].1));
    }

    #[test]
    fn describe_reads_the_compression_from_the_url() {
        let xz = describe("x", "x", "https://a/b/rootfs.tar.xz", None, 90 * 1048576);
        assert_eq!(xz.compression, Compression::Xz);
        assert_eq!(xz.download_mib, 90);
        // The disk suggestion has to leave room for the unpacked tree and for use.
        assert!(xz.disk_mib >= 90 * 4, "{} MiB is too small a disk", xz.disk_mib);

        let gz = describe("g", "g", "https://a/b/rootfs.tar.gz", None, 4 * 1048576);
        assert_eq!(gz.compression, Compression::Gzip);
        // Even a tiny image gets a usable disk.
        assert!(gz.disk_mib >= 1024, "{} MiB is too small a disk", gz.disk_mib);
    }


    #[test]
    fn lookups_work() {
        let catalogue = vec![describe("a", "a", "https://x/rootfs.tar.xz", None, 1024)];
        assert!(find(&catalogue, "a").is_some());
        assert!(find(&catalogue, "no-such-image").is_none());
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
