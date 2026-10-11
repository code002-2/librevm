# The virtio problem on Gunyah

Recorded so nobody has to rediscover it.

## What happens

Any virtio device whose driver probes in the guest kills crosvm with `SIGBUS`
(exit 135) once the driver programs the queue.

Confirmed twice, with a control:

| guest driver | device present | result |
| --- | --- | --- |
| none | virtio-blk on PCI | boots, `exit=0` |
| virtio-net | virtio-net on PCI | `SIGBUS`, `exit=135` |
| virtio-blk | virtio-blk on PCI | `SIGBUS`, `exit=135` |

The third row is the one that matters. A guest kernel built with
`CONFIG_VIRTIO_BLK=y` and `CONFIG_EXT4_FS=y` dies as soon as the block driver
touches the device, and the second row shows that merely *offering* a virtio
device is harmless. So the fault is in what the driver does -- programming a
virtqueue and then reading it back -- not in device creation.

## Where it dies

A gdb backtrace from the virtio-net case:

```
Thread 7 "v_net:0" received signal SIGBUS
#0  SplitQueue::peek ()
#1  ...   (roughly 18000 frames of Queue::peek with no symbols)
```

and the guest reaches this point:

```
pci 0000:00:02.0: [1af4:1041] type 00 class 0x020000   <- virtio-net
```

so the device enumerates and the driver starts, then the host faults reading a
queue the guest owns.

## Why

crosvm has to read guest memory to service a virtqueue. On Gunyah the memory is
lent to the VM, and a lent region is one the host cannot touch. Reading it faults.

Under Gunyah's resource manager the choice is not free. `SET_USER_MEM_REGION` is
refused outright with `ENODEV`, so a VM can only be started through the lend
path, which is what `--protected-vm-without-firmware` selects. There is no
share-back ioctl in this kernel's driver, so once memory is lent the host cannot
get it back.

DroidVM solves this with a *pseudo-unprotected* mode: lend so the resource
manager accepts, then map the same window again with `GUNYAH_MEM_FORCE_SHARE`
before the payload runs, which returns host access. That is a patch to crosvm. I
wrote it -- `ProtectionType::PseudoUnprotected` plus the
`--protected-vm-pseudo-unprotected` flag -- and it builds and parses, but the
resource manager still answers `ENODEV`, because the only ioctl that can carry
`GUNYAH_MEM_FORCE_SHARE` is `GH_VM_ANDROID_MAP_CMA_MEM`, which applies to a
file-backed region and not to ordinary guest RAM.

Attempting it also trips a fault in the driver:

```
WARNING: drivers/virt/gunyah/vm_mgr.c:1160 at _gunyah_vm_put+0x508/0x5b8
         [gunyah_rsc_mgr], CPU#6: crosvm
```

After enough of those the resource manager stops accepting new VMs until the
device is rebooted. That work has been reverted; the shipped crosvm is
unpatched.

## What this means

* The console works, because serial is a Unix socket and never touches guest
  memory.
* Anything behind virtio does not: networking, disks, shared directories,
  ballooning, vsock, GPU.
* Working around it needs a change in Qualcomm's Gunyah driver, not in this
  project. The pieces here -- disk images, share definitions -- are in place and
  will start working the moment the host can read guest memory again.

## Reproducing

A guest kernel with a virtio driver is needed; one without it cannot trigger
this. On this tree:

```sh
cd /root/mini-src
export ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu-
make defconfig
./scripts/config --enable VIRTIO_PCI --enable VIRTIO_BLK \
                 --enable EXT4_FS --enable DEVTMPFS
yes "" | make olddefconfig
make -j"$(nproc)" Image
```

Note that the `.config` left in that tree is an x86 one, so a build without
`ARCH=arm64` fails with `No rule to make target 'Image'`, which is not an obvious
message.

Then:

```sh
crosvm run --hypervisor gunyah --protected-vm-without-firmware \
  --mem 256 --swiotlb 64 --cpus 1 --disable-sandbox \
  --block /path/to/disk.img,lock=false \
  --params 'console=ttyS0 earlycon loglevel=7' \
  --serial type=stdout,hardware=serial,console,earlycon \
  /root/mini-blk-Image
```

and it exits 135.
