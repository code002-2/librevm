//! Disk images.
//!
//! Everything here is ordinary file work: create, size, grow, copy, inspect. Nothing
//! talks to a hypervisor, so none of it can disturb a running machine.
//!
//! Three formats are recognised:
//!
//!   * **raw** -- a flat file. The kernel can be given this directly.
//!   * **sparse** -- Android's sparse format, which is what `crosvm` gets from the
//!     `sparse=true` disk option. It stores only the blocks that were written, so a
//!     4 GiB image with 200 MiB of data occupies 200 MiB.
//!   * **qcow2** -- recognised so the tools can report it and refuse to touch it,
//!     because writing it needs qemu, which is not on this system.
//!
//! Sizes are bytes throughout. The CLI takes suffixes.

use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Android's sparse image magic, little endian.
const SPARSE_MAGIC: u32 = 0xed26_ff3a;
/// qcow2's magic.
const QCOW_MAGIC: &[u8; 4] = b"QFI\xfb";

/// The smallest image worth making, and the granularity resizes are rounded to.
pub const MIN_SIZE: u64 = 16 * 1024 * 1024;
pub const SIZE_STEP: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Raw,
    Sparse,
    Qcow2,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::Raw => "raw",
            Format::Sparse => "sparse",
            Format::Qcow2 => "qcow2",
        }
    }

    pub fn parse(s: &str) -> Result<Format> {
        match s.to_ascii_lowercase().as_str() {
            "raw" => Ok(Format::Raw),
            "sparse" | "android-sparse" => Ok(Format::Sparse),
            "qcow2" | "qcow" => Ok(Format::Qcow2),
            other => bail!("unknown disk format {other:?}; try raw, sparse or qcow2"),
        }
    }

    /// Can these tools write it?
    ///
    /// qcow2 is included because crosvm can create one. It is still not readable by the
    /// tools here -- `read` refuses it, and says so, rather than misreporting a header as
    /// a filesystem -- but creating it is useful, and it is what an overlay needs.
    pub fn writable(self) -> bool {
        true
    }

    /// Can the tools here read its contents without help?
    pub fn readable(self) -> bool {
        !matches!(self, Format::Qcow2)
    }
}

/// What is known about an image.
#[derive(Debug, Clone)]
pub struct Disk {
    pub path: PathBuf,
    pub format: Format,
    /// The size the guest sees.
    pub capacity: u64,
    /// How much of the host's disk it actually occupies.
    pub allocated: u64,
    /// Refuse to write?
    pub readonly: bool,
}

impl Disk {
    /// Space saved by sparseness, as a proportion.
    pub fn sparse_ratio(&self) -> f64 {
        if self.capacity == 0 {
            return 0.0;
        }
        1.0 - (self.allocated as f64 / self.capacity as f64)
    }
}

/// Parse a size with an optional suffix: `512M`, `8G`, `1T`, or plain bytes.
pub fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty size");
    }
    let (digits, multiplier) = match s.chars().last().unwrap().to_ascii_lowercase() {
        'k' => (&s[..s.len() - 1], 1024u64),
        'm' => (&s[..s.len() - 1], 1024 * 1024),
        'g' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        't' => (&s[..s.len() - 1], 1024u64 * 1024 * 1024 * 1024),
        'b' => (&s[..s.len() - 1], 1),
        _ => (s, 1),
    };
    let n: u64 = digits
        .trim()
        .parse()
        .with_context(|| format!("{digits:?} is not a number"))?;
    n.checked_mul(multiplier)
        .ok_or_else(|| anyhow::anyhow!("{s} is too large"))
}

/// A size in the largest unit that keeps it readable.
pub fn human(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("TiB", 1024 * 1024 * 1024 * 1024),
        ("GiB", 1024 * 1024 * 1024),
        ("MiB", 1024 * 1024),
        ("KiB", 1024),
    ];
    for (name, size) in UNITS {
        if bytes >= size {
            let value = bytes as f64 / size as f64;
            return if value >= 10.0 {
                format!("{value:.0} {name}")
            } else {
                format!("{value:.1} {name}")
            };
        }
    }
    format!("{bytes} B")
}

/// Round a size up to a whole number of megabytes, with a floor.
pub fn normalise_size(bytes: u64) -> u64 {
    let stepped = bytes.div_ceil(SIZE_STEP) * SIZE_STEP;
    stepped.max(MIN_SIZE)
}

/// Look at a file and work out what it is.
pub fn open(path: &Path) -> Result<Disk> {
    let meta = std::fs::metadata(path)
        .with_context(|| format!("cannot stat {}", path.display()))?;
    if !meta.is_file() {
        bail!("{} is not a regular file", path.display());
    }

    let format = detect_format(path)?;
    let allocated = meta.blocks() * 512;
    let capacity = logical_size(path, format, meta.len())?;

    Ok(Disk {
        path: path.to_path_buf(),
        format,
        capacity,
        allocated,
        readonly: meta.permissions().readonly(),
    })
}

fn detect_format(path: &Path) -> Result<Format> {
    let mut file = File::open(path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    let mut head = [0u8; 4];
    let n = file.read(&mut head).unwrap_or(0);
    if n >= 4 && head == *QCOW_MAGIC {
        return Ok(Format::Qcow2);
    }
    if n >= 4 && u32::from_le_bytes(head) == SPARSE_MAGIC {
        return Ok(Format::Sparse);
    }
    Ok(Format::Raw)
}

/// The size the guest would see, which is not the file's length for either format that
/// carries a header.
///
/// A qcow2 image is usually *smaller* than the disk it describes, and an overlay can be
/// tiny -- reporting the file length would say a 256 MiB disk is 320 KiB.
fn logical_size(path: &Path, format: Format, file_len: u64) -> Result<u64> {
    match format {
        Format::Sparse => {
            // Android sparse header: magic, major, minor, file_hdr_sz, chunk_hdr_sz,
            // blk_sz, total_blks, total_chunks, image_checksum
            let mut file = File::open(path)?;
            let mut header = [0u8; 28];
            file.read_exact(&mut header)
                .context("the sparse header is truncated")?;
            let blk_sz = u32::from_le_bytes(header[12..16].try_into().unwrap()) as u64;
            let total_blks = u32::from_le_bytes(header[16..20].try_into().unwrap()) as u64;
            Ok(blk_sz * total_blks)
        }
        Format::Qcow2 => Ok(qcow2_virtual_size(path).unwrap_or(file_len)),
        Format::Raw => Ok(file_len),
    }
}

/// Read a qcow2 header's virtual size: the size the guest sees.
///
/// The layout is fixed by the format: the magic, a version, then the backing file offset
/// and size, then the cluster size, the *virtual* disk size, and so on. Only the virtual
/// size is wanted, and it is a big-endian u64 at offset 24. A header that does not look
/// right returns `None` so the caller can fall back rather than report a wrong number.
fn qcow2_virtual_size(path: &Path) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let mut header = [0u8; 32];
    file.read_exact(&mut header).ok()?;
    if header[0..4] != *QCOW_MAGIC {
        return None;
    }
    let version = u32::from_be_bytes(header[4..8].try_into().ok()?);
    // Versions 2 and 3 both put the virtual size at the same offset.
    if version != 2 && version != 3 {
        return None;
    }
    let size = u64::from_be_bytes(header[24..32].try_into().ok()?);
    // A zero here means a header this does not understand; falling back to the file
    // length is better than reporting nothing.
    if size == 0 {
        None
    } else {
        Some(size)
    }
}

/// Create a new image.
///
/// A raw image is made sparse, so a 4 GiB disk with nothing on it occupies almost
/// nothing. A sparse image is written with a single "fill with zeros" chunk, which is
/// the cheapest way to describe an empty one. A qcow2 image is handed to crosvm, which
/// has its own writer; crosvm is the one that has to read it, so it is the right thing to
/// have made it.
///
/// `backing` makes the new image an overlay: it starts empty and any block not written is
/// read from the backing file. That is how a snapshot layer is built, and it is the reason
/// qcow2 is worth having at all here.
pub fn create_with_backing(path: &Path, size: u64, format: Format, backing: Option<&Path>) -> Result<Disk> {
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    if !format.writable() {
        bail!("creating {} images needs qemu, which is not installed", format.name());
    }
    let size = normalise_size(size);

    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
    }

    match format {
        Format::Raw => {
            if backing.is_some() {
                bail!("a raw image cannot have a backing file; only qcow2 can");
            }
            // set_len makes a sparse file on every filesystem worth using here
            let file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(path)
                .with_context(|| format!("creating {}", path.display()))?;
            file.set_len(size)
                .with_context(|| format!("sizing {}", path.display()))?;
            file.sync_all().ok();
        }
        Format::Sparse => {
            if backing.is_some() {
                bail!("a sparse image cannot have a backing file; only qcow2 can");
            }
            write_empty_sparse(path, size)?;
        }
        Format::Qcow2 => create_qcow2(path, size, backing)?,
    }

    open(path)
}

/// Create a qcow2 image with crosvm's own writer.
///
/// `qemu-img` would be the usual tool for this and is not installed, but crosvm can make a
/// qcow2 and can make one over a backing file, which is the whole of what is needed. Its
/// size argument is in bytes and has no suffix parsing, so the number is formatted here.
fn create_qcow2(path: &Path, size: u64, backing: Option<&Path>) -> Result<()> {
    let crosvm = crate::backend::find_crosvm(None)
        .context("creating a qcow2 image needs crosvm, which was not found")?;

    let mut cmd = std::process::Command::new(&crosvm);
    cmd.arg("create_qcow2").arg(path);
    match backing {
        // With a backing file the size is inherited, so crosvm takes none: passing one is
        // refused rather than ignored.
        Some(base) => {
            if !base.is_file() {
                bail!("backing file {} does not exist", base.display());
            }
            cmd.arg("--backing-file").arg(base);
        }
        None => {
            cmd.arg(size.to_string());
        }
    }

    let out = cmd.output().context("running crosvm create_qcow2")?;
    if !out.status.success() {
        // Do not leave a half-written image behind for something to try to mount.
        std::fs::remove_file(path).ok();
        bail!(
            "crosvm create_qcow2 failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// The old two-argument form, kept because most callers want it.
pub fn create(path: &Path, size: u64, format: Format) -> Result<Disk> {
    create_with_backing(path, size, format, None)
}

/// An Android sparse image holding one chunk that expands to all zeros.
///
/// The chunk header's `total_sz` is a count of bytes, not blocks -- the field name
/// says "total size" and the format means it. Writing the block count there produces an
/// image that expands to a fraction of its stated capacity.
fn write_empty_sparse(path: &Path, size: u64) -> Result<()> {
    const BLK: u32 = 4096;
    const CHUNK_FILL: u16 = 0xcac2;
    const FILE_HDR_SZ: u16 = 28;
    const CHUNK_HDR_SZ: u16 = 12;

    let total_blks = size.div_ceil(BLK as u64) as u32;
    let total_bytes = total_blks as u64 * BLK as u64;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;

    let mut header = Vec::with_capacity(28);
    header.extend_from_slice(&SPARSE_MAGIC.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes()); // major
    header.extend_from_slice(&0u16.to_le_bytes()); // minor
    header.extend_from_slice(&FILE_HDR_SZ.to_le_bytes());
    header.extend_from_slice(&CHUNK_HDR_SZ.to_le_bytes());
    header.extend_from_slice(&BLK.to_le_bytes());
    header.extend_from_slice(&total_blks.to_le_bytes());
    header.extend_from_slice(&1u32.to_le_bytes()); // one chunk
    header.extend_from_slice(&0u32.to_le_bytes()); // checksum, unchecked
    file.write_all(&header)?;

    let mut chunk = Vec::with_capacity(12);
    chunk.extend_from_slice(&CHUNK_FILL.to_le_bytes());
    chunk.extend_from_slice(&0u16.to_le_bytes()); // reserved
    chunk.extend_from_slice(&total_blks.to_le_bytes()); // chunk size, in blocks
    chunk.extend_from_slice(&(total_bytes as u32).to_le_bytes()); // total size, in bytes
    chunk.extend_from_slice(&0u32.to_le_bytes()); // the filler: four zero bytes
    file.write_all(&chunk)?;
    file.sync_all().ok();
    Ok(())
}

/// Grow or shrink an image.
///
/// Growing a raw image is free. Shrinking one only truncates the file; the guest's
/// filesystem has to be shrunk first, which is not something this can check, so a
/// shrink is refused unless `force` is given.
pub fn resize(path: &Path, new_size: u64, force: bool) -> Result<Disk> {
    let before = open(path)?;
    if !before.format.readable() {
        bail!(
            "resizing a {} image needs qemu-img, which is not installed; crosvm can create \
             one but not change its size",
            before.format.name()
        );
    }
    let new_size = normalise_size(new_size);

    if new_size == before.capacity {
        return Ok(before);
    }
    if new_size < before.capacity && !force {
        bail!(
            "shrinking {} from {} to {} would cut off whatever the guest has written. \
             Shrink the filesystem inside it first, then pass --force",
            path.display(),
            human(before.capacity),
            human(new_size)
        );
    }
    if new_size < MIN_SIZE {
        bail!("{} is below the minimum of {}", human(new_size), human(MIN_SIZE));
    }

    match before.format {
        Format::Raw => {
            let file = OpenOptions::new().write(true).open(path)?;
            file.set_len(new_size)
                .with_context(|| format!("resizing {}", path.display()))?;
            file.sync_all().ok();
        }
        Format::Sparse => {
            // rewriting is simplest and always correct; the data is copied out and
            // back in through a raw file of the right size
            let tmp = path.with_extension("resize-tmp");
            let data = read_all(path)?;
            write_raw(&tmp, &data, new_size)?;
            std::fs::rename(&tmp, path)
                .with_context(|| format!("replacing {}", path.display()))?;
        }
        Format::Qcow2 => unreachable!("rejected above"),
    }

    open(path)
}

/// Read an image's contents, expanding a sparse one as it goes.
fn read_all(path: &Path) -> Result<Vec<u8>> {
    let disk = open(path)?;
    match disk.format {
        Format::Raw => {
            let mut buf = Vec::new();
            File::open(path)?.read_to_end(&mut buf)?;
            Ok(buf)
        }
        Format::Sparse => expand_sparse(path),
        Format::Qcow2 => bail!("cannot read a qcow2 image without qemu"),
    }
}

/// Turn an Android sparse image into its flat contents.
///
/// The header is 28 bytes: magic, major, minor, file_hdr_sz, chunk_hdr_sz, blk_sz,
/// total_blks, total_chunks, checksum. Reading chunk_hdr_sz from the wrong offset gets
/// file_hdr_sz instead, which makes every chunk header read too long and the first one
/// come up truncated.
fn expand_sparse(path: &Path) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut header = [0u8; 28];
    file.read_exact(&mut header).context("truncated sparse header")?;
    if u32::from_le_bytes(header[0..4].try_into().unwrap()) != SPARSE_MAGIC {
        bail!("{} is not a sparse image", path.display());
    }
    let file_hdr_sz = u16::from_le_bytes(header[8..10].try_into().unwrap()) as usize;
    let chunk_hdr_sz = u16::from_le_bytes(header[10..12].try_into().unwrap()) as usize;
    let blk_sz = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
    let total_blks = u32::from_le_bytes(header[16..20].try_into().unwrap()) as usize;
    let total_chunks = u32::from_le_bytes(header[20..24].try_into().unwrap()) as usize;

    if chunk_hdr_sz < 12 {
        bail!(
            "{} claims a {chunk_hdr_sz}-byte chunk header, which is too small to be valid",
            path.display()
        );
    }
    // A file header longer than the one we know about carries fields we can skip.
    if file_hdr_sz > 28 {
        let mut skip = vec![0u8; file_hdr_sz - 28];
        file.read_exact(&mut skip)
            .context("truncated sparse file header")?;
    }

    let mut out: Vec<u8> = Vec::with_capacity(blk_sz * total_blks);
    let mut chunk_hdr = vec![0u8; chunk_hdr_sz];
    for _ in 0..total_chunks {
        file.read_exact(&mut chunk_hdr)
            .context("truncated sparse chunk header")?;
        let kind = u16::from_le_bytes(chunk_hdr[0..2].try_into().unwrap());
        // Both sizes are byte counts, despite one being named for chunks.
        let chunk_sz = u32::from_le_bytes(chunk_hdr[4..8].try_into().unwrap()) as usize;
        let total_sz = u32::from_le_bytes(chunk_hdr[8..12].try_into().unwrap()) as usize;
        let _ = chunk_sz;
        match kind {
            // raw: the data follows
            0xcac1 => {
                let start = out.len();
                out.resize(start + total_sz, 0);
                file.read_exact(&mut out[start..])?;
            }
            // fill: one 4-byte value repeated to fill total_sz bytes
            0xcac2 => {
                let mut v = [0u8; 4];
                file.read_exact(&mut v)?;
                let start = out.len();
                out.resize(start + total_sz, 0);
                for (i, slot) in out[start..].iter_mut().enumerate() {
                    *slot = v[i % 4];
                }
            }
            // skip: zeros, and nothing in the file
            0xcac3 => out.resize(out.len() + total_sz, 0),
            // crc32: a checksum, nothing to emit
            0xcac4 => {
                let mut skip = [0u8; 4];
                file.read_exact(&mut skip)?;
            }
            other => bail!("unknown sparse chunk type {other:#x}"),
        }
    }
    out.resize(blk_sz * total_blks, 0);
    Ok(out)
}

fn write_raw(path: &Path, data: &[u8], size: u64) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    file.write_all(data)?;
    file.set_len(size)?;
    file.sync_all().ok();
    Ok(())
}

/// Copy an image.
///
/// A raw image is copied sparsely, so an almost empty 4 GiB disk does not turn into
/// 4 GiB on the host.
pub fn clone_to(src: &Path, dst: &Path) -> Result<Disk> {
    if dst.exists() {
        bail!("{} already exists", dst.display());
    }
    let source = open(src)?;
    if !source.format.readable() {
        bail!(
            "copying a {} image needs qemu-img, which is not installed",
            source.format.name()
        );
    }

    if let Some(dir) = dst.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }

    match source.format {
        Format::Raw => {
            let mut from = File::open(src)?;
            let mut to = OpenOptions::new().create_new(true).write(true).open(dst)?;
            copy_holes(&mut from, &mut to)?;
            to.set_len(source.capacity)?;
            to.sync_all().ok();
        }
        Format::Sparse => {
            std::fs::copy(src, dst)
                .with_context(|| format!("copying {} to {}", src.display(), dst.display()))?;
        }
        Format::Qcow2 => unreachable!("rejected above"),
    }

    open(dst)
}

/// Copy a file, skipping runs of zeros so the destination stays sparse.
fn copy_holes(from: &mut File, to: &mut File) -> Result<()> {
    const CHUNK: usize = 1 << 20;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = from.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let slice = &buf[..n];
        if slice.iter().all(|&b| b == 0) {
            // seek forward instead of writing: the hole is preserved
            to.seek(SeekFrom::Current(n as i64))?;
        } else {
            to.write_all(slice)?;
        }
    }
    Ok(())
}

/// Every image in a directory, sorted by name.
pub fn list(dir: &Path) -> Result<Vec<Disk>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        // only things that look like images
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !(name.ends_with(".img") || name.ends_with(".raw") || name.ends_with(".qcow2")) {
            continue;
        }
        match open(&path) {
            Ok(d) => out.push(d),
            Err(e) => log::warn!("skipping {}: {e:#}", path.display()),
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

pub fn delete(path: &Path) -> Result<()> {
    std::fs::remove_file(path)
        .with_context(|| format!("removing {}", path.display()))?;
    Ok(())
}

/// Put a filesystem on an image, using the host's `mkfs`.
///
/// The image is attached to a loop device so `mkfs` can write into it directly. That
/// needs root and a loop device; if either is missing the caller gets a clear message
/// rather than a half-written image.
pub fn make_filesystem(path: &Path, kind: &str) -> Result<()> {
    let disk = open(path)?;
    if disk.format != Format::Raw {
        bail!(
            "a filesystem can only be made directly inside a raw image, and this is {}",
            disk.format.name()
        );
    }
    if !matches!(kind, "ext4" | "ext2" | "vfat") {
        bail!("unsupported filesystem {kind:?}; try ext4, ext2 or vfat");
    }
    let mkfs = format!("mkfs.{kind}");
    if which(&mkfs).is_none() {
        bail!("{mkfs} is not installed");
    }

    let loopdev = attach_loop(path)?;
    let result = run(&mkfs, &["-F", &loopdev]);
    let detach = detach_loop(&loopdev);
    result?;
    detach?;
    Ok(())
}

/// Attach a file to a free loop device and return its path.
fn attach_loop(path: &Path) -> Result<String> {
    if which("losetup").is_none() {
        bail!("losetup is not installed");
    }
    let out = std::process::Command::new("losetup")
        .args(["--find", "--show", "--partscan"])
        .arg(path)
        .output()
        .context("running losetup")?;
    if !out.status.success() {
        bail!(
            "losetup failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let dev = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if dev.is_empty() {
        bail!("losetup did not report a device");
    }
    Ok(dev)
}

fn detach_loop(dev: &str) -> Result<()> {
    let out = std::process::Command::new("losetup")
        .args(["--detach", dev])
        .output()
        .context("running losetup --detach")?;
    if !out.status.success() {
        bail!(
            "could not detach {}: {}",
            dev,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        bail!(
            "{program} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Is a program on PATH?
pub fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("librevm-disk-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn size_parsing() {
        assert_eq!(parse_size("512").unwrap(), 512);
        assert_eq!(parse_size("1K").unwrap(), 1024);
        assert_eq!(parse_size("2M").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_size("3G").unwrap(), 3 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("1T").unwrap(), 1024u64 * 1024 * 1024 * 1024);
        assert_eq!(parse_size(" 4g ").unwrap(), 4 * 1024 * 1024 * 1024);
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("99999999999999999999T").is_err());
    }

    #[test]
    fn size_normalising() {
        // anything below the floor comes back at the floor
        assert_eq!(normalise_size(1), MIN_SIZE);
        assert_eq!(normalise_size(2 * 1024 * 1024), MIN_SIZE);
        assert_eq!(normalise_size(20 * 1024 * 1024), 20 * 1024 * 1024);
        // rounds up to a whole megabyte
        assert_eq!(
            normalise_size(32 * 1024 * 1024 + 1),
            33 * 1024 * 1024
        );
    }

    #[test]
    fn human_readable() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1024), "1.0 KiB");
        assert_eq!(human(1024 * 1024), "1.0 MiB");
        assert_eq!(human(4 * 1024 * 1024 * 1024), "4.0 GiB");
        assert_eq!(human(20 * 1024 * 1024 * 1024), "20 GiB");
    }

    #[test]
    fn format_parsing() {
        assert_eq!(Format::parse("raw").unwrap(), Format::Raw);
        assert_eq!(Format::parse("SPARSE").unwrap(), Format::Sparse);
        assert_eq!(Format::parse("qcow2").unwrap(), Format::Qcow2);
        assert!(Format::parse("vmdk").is_err());
        assert!(Format::Qcow2.writable(), "crosvm can create one");
        assert!(!Format::Qcow2.readable(), "nothing here can read one");
    }

    #[test]
    fn create_a_raw_image_is_sparse_on_disk() {
        let dir = tmpdir("raw");
        let img = dir.join("a.img");
        let disk = create(&img, 64 * 1024 * 1024, Format::Raw).unwrap();
        assert_eq!(disk.capacity, 64 * 1024 * 1024);
        assert_eq!(disk.format, Format::Raw);
        // the file is 64 MiB long but should occupy far less
        assert!(
            disk.allocated < 4 * 1024 * 1024,
            "allocated {} for an empty image",
            disk.allocated
        );
        assert!(disk.sparse_ratio() > 0.9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_refuses_to_overwrite() {
        let dir = tmpdir("exists");
        let img = dir.join("a.img");
        create(&img, 32 * 1024 * 1024, Format::Raw).unwrap();
        assert!(create(&img, 32 * 1024 * 1024, Format::Raw).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_a_sparse_image_and_read_its_capacity() {
        let dir = tmpdir("sparse");
        let img = dir.join("a.img");
        let disk = create(&img, 128 * 1024 * 1024, Format::Sparse).unwrap();
        assert_eq!(disk.format, Format::Sparse);
        // the capacity comes from the header, not the file length
        assert_eq!(disk.capacity, 128 * 1024 * 1024);
        assert!(disk.allocated < 1024 * 1024, "allocated {}", disk.allocated);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn growing() {
        let dir = tmpdir("grow");
        let img = dir.join("a.img");
        create(&img, 32 * 1024 * 1024, Format::Raw).unwrap();
        let disk = resize(&img, 96 * 1024 * 1024, false).unwrap();
        assert_eq!(disk.capacity, 96 * 1024 * 1024);
        assert_eq!(std::fs::metadata(&img).unwrap().len(), 96 * 1024 * 1024);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shrinking_needs_force() {
        let dir = tmpdir("shrink");
        let img = dir.join("a.img");
        create(&img, 96 * 1024 * 1024, Format::Raw).unwrap();
        assert!(resize(&img, 32 * 1024 * 1024, false).is_err());
        let disk = resize(&img, 32 * 1024 * 1024, true).unwrap();
        assert_eq!(disk.capacity, 32 * 1024 * 1024);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cloning_a_raw_image_stays_sparse() {
        let dir = tmpdir("clone");
        let src = dir.join("a.img");
        let dst = dir.join("b.img");
        create(&src, 64 * 1024 * 1024, Format::Raw).unwrap();
        let copy = clone_to(&src, &dst).unwrap();
        assert_eq!(copy.capacity, 64 * 1024 * 1024);
        assert!(
            copy.allocated < 4 * 1024 * 1024,
            "a copy of an empty image occupies {}",
            copy.allocated
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cloning_carries_data_across() {
        let dir = tmpdir("clonedata");
        let src = dir.join("a.img");
        let dst = dir.join("b.img");
        create(&src, 32 * 1024 * 1024, Format::Raw).unwrap();
        {
            let mut f = OpenOptions::new().write(true).open(&src).unwrap();
            f.seek(SeekFrom::Start(4096)).unwrap();
            f.write_all(b"payload").unwrap();
        }
        clone_to(&src, &dst).unwrap();
        let mut f = File::open(&dst).unwrap();
        let mut buf = [0u8; 7];
        f.seek(SeekFrom::Start(4096)).unwrap();
        f.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"payload");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cloning_refuses_to_overwrite() {
        let dir = tmpdir("cloneexists");
        let src = dir.join("a.img");
        let dst = dir.join("b.img");
        create(&src, 32 * 1024 * 1024, Format::Raw).unwrap();
        clone_to(&src, &dst).unwrap();
        assert!(clone_to(&src, &dst).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn listing_only_picks_up_images() {
        let dir = tmpdir("list");
        create(&dir.join("one.img"), 16 * 1024 * 1024, Format::Raw).unwrap();
        create(&dir.join("two.img"), 16 * 1024 * 1024, Format::Sparse).unwrap();
        std::fs::write(dir.join("notes.txt"), b"not an image").unwrap();
        let all = list(&dir).unwrap();
        assert_eq!(all.len(), 2, "got {:?}", all.iter().map(|d| &d.path).collect::<Vec<_>>());
        assert!(all[0].path.ends_with("one.img"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn listing_a_missing_directory_is_empty_not_an_error() {
        let missing = std::env::temp_dir().join("librevm-definitely-not-here");
        let _ = std::fs::remove_dir_all(&missing);
        assert!(list(&missing).unwrap().is_empty());
    }

    #[test]
    fn opening_a_missing_file_fails() {
        let missing = std::env::temp_dir().join("librevm-no-such-image.img");
        let _ = std::fs::remove_file(&missing);
        assert!(open(&missing).is_err());
    }

    #[test]
    fn a_qcow2_header_is_recognised() {
        let dir = tmpdir("qcow");
        let img = dir.join("a.qcow2");
        // just enough of a header to be identified
        let mut header = Vec::new();
        header.extend_from_slice(QCOW_MAGIC);
        header.extend_from_slice(&[0u8; 60]);
        std::fs::write(&img, &header).unwrap();
        let disk = open(&img).unwrap();
        assert_eq!(disk.format, Format::Qcow2);
        // Recognised and creatable, but not readable here: there is no qcow2 parser and no
        // qemu-img, so a resize has to refuse rather than corrupt it.
        assert!(disk.format.writable());
        assert!(!disk.format.readable());
        assert!(resize(&img, 64 * 1024 * 1024, true).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_qcow2_header_is_read_for_its_virtual_size() {
        // A real qcow2 header, built the way the format defines it: magic, version, the
        // backing file offset and size, the cluster bits, then the virtual size as a
        // big-endian u64 at offset 24. The file itself is far smaller than the disk it
        // describes, which is the whole point -- reporting the file length would say a
        // 256 MiB disk is a few hundred bytes.
        let dir = tmpdir("qcow2size");
        let img = dir.join("a.qcow2");
        let mut header = Vec::new();
        header.extend_from_slice(QCOW_MAGIC);
        header.extend_from_slice(&3u32.to_be_bytes()); // version 3
        header.extend_from_slice(&0u64.to_be_bytes()); // backing file offset
        header.extend_from_slice(&0u32.to_be_bytes()); // backing file size
        header.extend_from_slice(&16u32.to_be_bytes()); // cluster bits
        header.extend_from_slice(&(256u64 * 1024 * 1024).to_be_bytes()); // virtual size
        header.extend_from_slice(&[0u8; 40]);
        std::fs::write(&img, &header).unwrap();

        let disk = open(&img).unwrap();
        assert_eq!(disk.format, Format::Qcow2);
        assert_eq!(
            disk.capacity,
            256 * 1024 * 1024,
            "the virtual size should be reported, not the file's {} bytes",
            std::fs::metadata(&img).unwrap().len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_qcow2_header_this_does_not_understand_falls_back() {
        // A truncated or nonsense header must not make the size come out as zero.
        let dir = tmpdir("qcow2bad");
        let img = dir.join("b.qcow2");
        let mut header = Vec::new();
        header.extend_from_slice(QCOW_MAGIC);
        header.extend_from_slice(&99u32.to_be_bytes()); // a version that does not exist
        header.extend_from_slice(&[0u8; 52]);
        std::fs::write(&img, &header).unwrap();

        let disk = open(&img).unwrap();
        assert_eq!(disk.format, Format::Qcow2);
        assert_eq!(disk.capacity, std::fs::metadata(&img).unwrap().len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting() {
        let dir = tmpdir("delete");
        let img = dir.join("a.img");
        create(&img, 16 * 1024 * 1024, Format::Raw).unwrap();
        assert!(img.exists());
        delete(&img).unwrap();
        assert!(!img.exists());
        assert!(delete(&img).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sparse_round_trip_through_expand() {
        let dir = tmpdir("expand");
        let img = dir.join("a.img");
        create(&img, 32 * 1024 * 1024, Format::Sparse).unwrap();
        let flat = expand_sparse(&img).unwrap();
        assert_eq!(flat.len(), 32 * 1024 * 1024);
        assert!(flat.iter().all(|&b| b == 0));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
