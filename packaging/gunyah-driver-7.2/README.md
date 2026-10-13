# Gunyah hypervisor driver for mainline 7.2 — SM8750 and other Gunyah SoCs

The standalone `/dev/gunyah` driver, extracted from the branch it was taken from and verified to
apply to a clean linux-7.2.6. Everything in `patches/` is SoC-agnostic: it depends on the Gunyah
hypervisor being present, not on any particular board.

## Where it came from

```
repository: https://github.com/SuperTurtleDev/iceland-mainline-kernel-linux
branch:     pr/gunyah
head:       fd503a492d381687c143b7085ea79ebce58dcdc7
            tools: virt: gunyah: add VM smoke test and record verification results
```

The driver originates in the Android common kernel, tag `android17-6.18-2026-06_r1`
(`30c0eaeef78d5c19f687ad8418c27b76353ab261`), and that branch brought it to mainline. Its
`Documentation/virt/gunyah/port_adaptations.md` is the best description of what changed, and it
is included in `patches/` and in `result/`. The driver logic is untouched; every edit is an
API-form or build-system adaptation.

The branch is for a different tablet, but only its devicetree commit is board-specific, and that
one is deliberately not included here — see below.

This is **not** the `[RFC PATCH] Running Qualcomm's Gunyah Guests via KVM in EL1` series. That
one rewires the driver into KVM and carries KVM tree changes with it. This is the standalone
driver, which is why it is portable.

## The three patches, in this order

| | commit | subject | size |
|---|---|---|---|
| 02 | `c4abca95007b` | virt: gunyah: import Gunyah hypervisor drivers from android17-6.18 | 270 KB |
| 03 | `dbe63e43847b` | virt: gunyah: adapt to mainline 7.2 | 9.5 KB |
| 04 | `fd503a492d38` | tools: virt: gunyah: add VM smoke test and record verification results | 19 KB |

The numbering keeps the upstream positions, so 01 is absent on purpose.

**The order is git's, not the dates'.** The branch's fourth commit carries a rewritten date and
looks oldest, but it comes last. Applying commits out of order fails with a message that reads
like a missing file rather than a wrong position:

```
can't find file to patch at input line 20
```

## The board-specific commit is not included

The branch has one further Gunyah-related commit:

```
ed9d01daed04  ICELAND: Reserve llcc_lpi_mem to fix Gunyah crash
```

It edits `arch/arm64/boot/dts/qcom/kaanapali-oneplus-iceland.dts` and exists to stop the
hypervisor tripping over a reserved-memory region on that specific board. It is a devicetree
change for hardware this repository has nothing to do with, so it is left out. Any board that
needs the same fix should make it in its own devicetree.

## What applies to a clean 7.2.6

```
OK    02-virt-gunyah-import-gunyah-hypervisor-drivers   (all 21 files)
OK    03-virt-gunyah-adapt-to-mainline-7-2
OK    04-tools-virt-gunyah-add-vm-smoke-test
```

All three, applied with `patch -p1 --fuzz=3`, in order, on a clean 7.2.6 from kernel.org.

## What you get

```
drivers/virt/gunyah/    18 files: gunyah.c  gunyah_vcpu.c  gunyah_qtvm.c
                                  gunyah_irqfd.c  gunyah_ioeventfd.c
                                  vm_mgr.c  vm_mgr.h  vm_mgr_mem.c  vm_mgr_cma_mem.c
                                  rsc_mgr.c  rsc_mgr.h  rsc_mgr_rpc.c
                                  gunyah_qcom.c  gunyah_platform_hooks.c
                                  gunyah_vendor_hooks.h  trace_vcpu.h  Kconfig  Makefile
include/linux/          gunyah.h (22 KB)  gunyah_qtvm.h
include/uapi/linux/     gunyah.h (16 KB)
arch/arm64/gunyah/      gunyah_hypercall.c  timer.c  Makefile
arch/arm64/include/asm/ gunyah.h
Documentation/virt/gunyah/                    index.rst  message-queue.rst
                                              port_adaptations.md
Documentation/devicetree/bindings/firmware/   gunyah-hypervisor.yaml  gunyah-cma-mem.yaml
tools/virt/gunyah/      gh_trace.sh  gh_vmtest.py
```

Three integration points in pre-existing files:

```
drivers/virt/Kconfig:53    source "drivers/virt/gunyah/Kconfig"
drivers/virt/Makefile:13   obj-y                         += gunyah/
arch/arm64/Kbuild:14       obj-$(CONFIG_GUNYAH_HYPERCALLS) += gunyah/
```

Eight Kconfig symbols, and nothing in them names a board:

```
GUNYAH_HYPERCALLS  GUNYAH_TIMER  GUNYAH  GUNYAH_PLATFORM_HOOKS
GUNYAH_QCOM_PLATFORM  GUNYAH_IRQFD  GUNYAH_IOEVENTFD  GUNYAH_QCOM_TRUSTED_VM

config GUNYAH
    tristate "Gunyah Virtualization drivers"
    depends on ARM64
    select GUNYAH_PLATFORM_HOOKS
    select AUXILIARY_BUS
    imply GUNYAH_QCOM_PLATFORM if ARCH_QCOM
```

`depends on ARM64` and `imply ... if ARCH_QCOM` is the whole of it. There is no SoC-specific
dependency, which is what makes this portable to any Qualcomm SoC with Gunyah in EL2.

## `result/`

The files as they exist after applying all three patches, so the tree can be read without running
patch. Paths under `result/` are relative to the kernel root. `result/INTEGRATION.txt` records the
three integration lines and the Kconfig symbols.

## What the driver's own notes say about 7.2

From `port_adaptations.md`, five adaptations, all API-form rather than logic:

1. `rsc_mgr.h` — `linux/android_kabi.h` and `ANDROID_KABI_ENUMERATOR_IGNORE()` become a
   `__has_include` guard with a no-op fallback. There is no mainline counterpart.
2. `gunyah_vcpu.c`, `vm_mgr.c` — `trace/hooks/gunyah.h` becomes `gunyah_vendor_hooks.h`, which
   forwards to the Android header under `CONFIG_ANDROID_VENDOR_HOOKS` and expands to empty inline
   stubs otherwise. The call sites are unchanged.
3. `vm_mgr.h` — open-coded `set_direct_map_{invalid,default}_noflush()` loops become
   `gunyah_folio_set_direct_map()`, compiled out under `MODULE` because mainline arm64 does not
   export those to modules. Lent memory stays protected by the hypervisor's stage-2 unmap; what
   is lost when built as a module is the host-side linear-map hardening.
4. `rsc_mgr.c` — `of_node_to_fwnode()` renamed to `of_fwnode_handle()`.
5. `drivers/virt/Kconfig`, `drivers/virt/Makefile`, `arch/arm64/Kbuild` — the same three lines,
   appended.

## Evidence it is the right driver for this class of hardware

The branch's verification section matches what an SM8750P device with this driver reports, line
for line:

```
gunyah: Running under Gunyah hypervisor 51/v1
RM rejected message 00000005. Error: -1
```

and the module set it names is the one loaded there:

```
gunyah_rsc_mgr  gunyah_vcpu  gunyah_irqfd  gunyah_ioeventfd  gunyah_qcom  gunyah_qtvm
```

The `RM rejected message 00000005` line is called out in the notes as the optional resource
manager log query, which this firmware does not implement, and it is harmless.

## Applying it

```sh
curl -LO https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-7.2.6.tar.xz
tar -xf linux-7.2.6.tar.xz
cd linux-7.2.6
for p in /path/to/patches/*.patch; do
    patch -p1 --forward --fuzz=3 < "$p" || echo "failed: $p"
done
```

All three should apply. Then build with `CONFIG_GUNYAH=y` or `=m`.

## Notes

* The `From: iceland-dev <dev@iceland.local>` line at the top of each patch is the upstream
  author metadata. It is left untouched so the patches stay byte-identical to what the branch
  served; changing it would change their hashes and make them no longer the same commits.
* `tools/virt/gunyah/gh_vmtest.py` builds a test DTB whose `image-name` is a string label. It
  names nothing real and needs no change per board.
* While the driver was being measured on an SM8750P device, its `dmesg | grep -c 'end trace'`
  went from 12 to 32. Every one of those is the same `mac80211`/`ath12k` warning from the WiFi
  receive path (`net/mac80211/rx.c:5691`, `ieee80211_rx_list`, `ath12k_wifi7_dp_rx_process`),
  and they appeared during several hundred megabytes of sustained downloading. They are
  unrelated to Gunyah.
