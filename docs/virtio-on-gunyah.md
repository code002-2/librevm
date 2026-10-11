# virtio on Gunyah

A correction, kept because the wrong version of this was written down and acted on.

## The short version

**crosvm enables `virtio-balloon` by default, and on this device that device kills it
with `SIGBUS` as soon as the guest's driver probes it.**

Add `--no-balloon` and everything works: virtio-blk disks mount and read and write,
virtio-net comes up, the machine boots.

```sh
crosvm run --hypervisor gunyah --protected-vm-without-firmware \
  --mem 256 --swiotlb 64 --cpus 1 --disable-sandbox \
  --no-balloon \
  --block /path/to/rootfs.img,lock=false \
  --initrd /path/to/initramfs.cpio.gz \
  --params 'console=ttyS0 earlycon loglevel=7' \
  --serial type=stdout,hardware=serial,console,earlycon \
  /path/to/Image
```

## The evidence

One variable at a time, each run booting Alpine and waiting for the shell:

| flags | result | virtio devices the guest saw |
| --- | --- | --- |
| none | `SIGBUS` | `[1af4:1045]` balloon, xhci, host bridge |
| `--no-rng` | `SIGBUS` | `[1af4:1045]` still there |
| **`--no-balloon`** | **`exit=0`** | none but the host bridge and xhci |
| `--no-usb` | `SIGBUS` | `[1af4:1045]` |
| `--no-pmu` | `SIGBUS` | `[1af4:1045]` |

`[1af4:1045]` is virtio-balloon. virtio-rng is `[1af4:1005]`, which is why `--no-rng`
changed nothing -- the device that stayed behind was the balloon, not the RNG.

With `--no-balloon`, and a 512 MiB ext4 image attached:

```
virtio_blk virtio0: 1/0/0 default/read/poll queues
virtio_blk virtio0: [vda] 1048576 512-byte logical blocks (537 MB/512 MiB)
EXT4-fs (vda): mounted filesystem 0f912a50-... r/w with ordered data mode
MOUNT-OK
WRITE-OK
```

and with a tap device attached, the machine still exits cleanly and the guest enumerates
`[1af4:1041]`, which is virtio-net.

## What the earlier version of this file claimed, and why it was wrong

It said every virtio device was unusable on Gunyah, that lent memory made the host
unable to service a virtqueue, and that fixing it needed a kernel module plus a boot shim
of the kind DroidVM describes in its own plans.

That was inferred from two things: virtio-net appeared to crash when the guest enumerated
it, and a guest kernel *without* any virtio driver booted fine. Both observations were
real; the conclusion did not follow.

* The virtio-net runs were made before the balloon was identified, so the balloon was
  present in them.
* "No driver, no crash" was read as "the driver is what crashes" when it also fits "the
  balloon is what crashes, and a guest that cannot drive anything never probes it".

The decisive test was varying one flag at a time. It should have been the first thing
tried, not the last.

There is a second trap in the flags: crosvm's default set is wider than it looks.
`--no-balloon` is not enough on its own if something else in the default set is also
fatal on a given device -- here nothing else was, but assuming that is how the earlier
mistake got made in the first place.

## What this means for the project

Disks, networking and shared directories are all back on the table, and the block-device
and network definitions the rest of this tree already writes are usable as they stand,
once the balloon is off.

## Reproducing

A guest kernel with the virtio drivers built in is needed. On this tree:

```sh
cd /root/mini-src
export ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu-
make defconfig
./scripts/config --enable VIRTIO_PCI --enable VIRTIO_BLK --enable VIRTIO_NET \
                 --enable EXT4_FS --enable DEVTMPFS
yes "" | make olddefconfig
make -j2 Image
```

Note that the `.config` left in that tree is an x86 one, so a build without `ARCH=arm64`
fails with `No rule to make target 'Image'`, which is not an obvious message.

An initramfs is still the simplest way to get a root filesystem in, and an ext4 image
attached with `--block` is the way to get one that persists.
