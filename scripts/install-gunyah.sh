#!/bin/bash
#
# install-gunyah.sh -- build and install the Gunyah modules for the running kernel.
#
# This device boots under Qualcomm's hypervisor, which owns EL2. Linux talks to it
# through a set of Gunyah modules; without them there is no /dev/gunyah and crosvm
# cannot start a guest.
#
# Two things bite people here, and the script checks for both:
#
#   * vermagic. A module built against a different release string is refused with a
#     bare "Exec format error". The kernel release on this device is not the upstream
#     version number, so the only safe source is a tree configured for exactly this
#     release.
#
#   * BTI. The kernel is built with CONFIG_ARM64_BTI_KERNEL, so every module must
#     carry branch-target landing pads. GCC does not emit them by default, and the
#     result is an "Oops - BTI" the first time a page is called into. The build here
#     always passes -mbranch-protection=standard.
#
# Usage:
#   install-gunyah.sh --source DIR        build from a kernel tree and install
#   install-gunyah.sh --modules DIR       install .ko files that are already built
#   install-gunyah.sh --status            report what is installed and loaded
#   install-gunyah.sh --uninstall         remove the modules and the autoload list
#
# Options for --source:
#   --jobs N            parallel build jobs (default: all cores)
#   --cc gcc|clang      compiler to build with (default: gcc)
#   --keep              leave the modules in the tree instead of installing them
#
set -u

KREL=$(uname -r)
MODROOT=/lib/modules/$KREL
LOADCONF=/etc/modules-load.d/gunyah.conf

# The modules this installer owns. These are the Gunyah drivers, and they are what
# gets built, installed and removed.
GUNYAH_MODULES="gunyah_hypercall gunyah gunyah_platform_hooks gunyah_rsc_mgr \
gunyah_vcpu gunyah_irqfd gunyah_ioeventfd gunyah_qcom gunyah_qtvm"

# What has to be loaded before them. The order matters: the hypercall layer has to be
# in place before anything that depends on it.
#
# `timer` is in this list but not the one above: it is an ordinary kernel module that
# already exists on the system, it simply has to come first. Listing it as something
# we install would be wrong, and would make --status report a missing module that was
# never ours to provide.
LOAD_BEFORE="timer"
LOAD_ORDER="timer gunyah_hypercall gunyah gunyah_platform_hooks gunyah_rsc_mgr \
gunyah_vcpu gunyah_irqfd gunyah_ioeventfd gunyah_qcom gunyah_qtvm"

# Where each Gunyah module goes in the installed tree, matching the kernel's own
# layout: in-tree modules live under kernel/, beside the rest of the kernel's modules.
declare -A DEST=(
  [gunyah_hypercall]="kernel/arch/arm64/gunyah"
  [gunyah]="kernel/drivers/virt/gunyah"
  [gunyah_ioeventfd]="kernel/drivers/virt/gunyah"
  [gunyah_irqfd]="kernel/drivers/virt/gunyah"
  [gunyah_platform_hooks]="kernel/drivers/virt/gunyah"
  [gunyah_qcom]="kernel/drivers/virt/gunyah"
  [gunyah_qtvm]="kernel/drivers/virt/gunyah"
  [gunyah_rsc_mgr]="kernel/drivers/virt/gunyah"
  [gunyah_vcpu]="kernel/drivers/virt/gunyah"
)

# The two in-tree directories that hold the sources.
SRC_DIRS="drivers/virt/gunyah arch/arm64/gunyah"

say()  { printf '%s\n' "$*"; }
step() { printf '\n== %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

need_root() {
    [ "$(id -u)" = 0 ] || die "must run as root (installs into $MODROOT)"
}

# ---------------------------------------------------------------------------
# status
# ---------------------------------------------------------------------------

do_status() {
    step "running kernel"
    say "  release : $KREL"
    say "  machine : $(uname -m)"

    step "kernel module features that constrain a build"
    if [ -r /proc/config.gz ]; then
        for opt in CONFIG_MODULES CONFIG_ARM64_BTI_KERNEL CONFIG_MODULE_SIG \
                   CONFIG_STRICT_MODULE_RWX CONFIG_MODULE_COMPRESS; do
            line=$(zcat /proc/config.gz 2>/dev/null | grep -E "^$opt=|^# $opt is not set" | head -1)
            say "  ${line:-$opt: not present}"
        done
    else
        warn "/proc/config.gz is unreadable, cannot check the kernel's options"
    fi

    step "modules on disk"
    local found=0 total=0
    for m in $GUNYAH_MODULES; do
        total=$((total + 1))
        local dir="${DEST[$m]}"
        local f="$MODROOT/$dir/$m.ko"
        if [ -f "$f" ]; then
            printf '  %-24s %s\n' "$m" "$f"
            found=$((found + 1))
        else
            printf '  %-24s MISSING\n' "$m"
        fi
    done
    say "  ($found of $total installed)"

    step "the modules that must be loaded first"
    for m in $LOAD_BEFORE; do
        # these belong to the kernel, not to us; just report whether they are there
        if modinfo "$m" >/dev/null 2>&1; then
            say "  $m: present"
        else
            warn "$m is not available, and the Gunyah modules need it first"
        fi
    done

    step "modules loaded now"
    if lsmod 2>/dev/null | grep -qE 'gunyah|^timer'; then
        lsmod | grep -E 'gunyah|^timer' | awk '{printf "  %-24s %s bytes, used %s\n", $1, $2, $3}'
    else
        say "  none loaded"
    fi

    step "the device node"
    if [ -c /dev/gunyah ]; then
        say "  /dev/gunyah exists: $(stat -c '%A %U:%G %t:%T' /dev/gunyah)"
    else
        say "  /dev/gunyah is absent"
    fi

    step "autoload"
    if [ -f "$LOADCONF" ]; then
        say "  $LOADCONF:"
        sed 's/^/    /' "$LOADCONF"
    else
        say "  $LOADCONF does not exist; the modules will not come back after a reboot"
    fi

    step "vermagic of what is installed"
    for m in $GUNYAH_MODULES; do
        local f="$MODROOT/${DEST[$m]}/$m.ko"
        [ -f "$f" ] || continue
        local got
        got=$(modinfo -F vermagic "$f" 2>/dev/null | awk '{print $1}')
        if [ "$got" = "$KREL" ]; then
            printf '  %-24s ok\n' "$m"
        else
            printf '  %-24s MISMATCH: %s\n' "$m" "${got:-unknown}"
        fi
    done
}

# ---------------------------------------------------------------------------
# checks
# ---------------------------------------------------------------------------

# Refuse to install something the kernel will not load. A vermagic mismatch shows up
# as "Exec format error", which tells the operator nothing useful.
verify_vermagic() {
    local dir="$1"
    local bad=0
    step "checking vermagic against $KREL"
    for m in $GUNYAH_MODULES; do
        local f
        f=$(find "$dir" -name "$m.ko" -print -quit 2>/dev/null)
        [ -n "$f" ] || { printf '  %-24s not found\n' "$m"; bad=$((bad + 1)); continue; }
        local got
        got=$(modinfo -F vermagic "$f" 2>/dev/null | awk '{print $1}')
        if [ "$got" = "$KREL" ]; then
            printf '  %-24s ok\n' "$m"
        else
            printf '  %-24s MISMATCH (%s)\n' "$m" "${got:-unknown}"
            bad=$((bad + 1))
        fi
    done
    if [ "$bad" -gt 0 ]; then
        die "$bad module(s) do not match this kernel; build them from a tree configured for $KREL"
    fi
}

# Warn when a module lacks BTI landing pads but the kernel expects them.
verify_bti() {
    local dir="$1"
    local wants_bti=no
    if zcat /proc/config.gz 2>/dev/null | grep -q '^CONFIG_ARM64_BTI_KERNEL=y'; then
        wants_bti=yes
    fi

    step "checking branch-target support"
    if [ "$wants_bti" = no ]; then
        say "  the kernel was not built with BTI, nothing to check"
        return
    fi
    say "  the kernel has CONFIG_ARM64_BTI_KERNEL=y, so modules need landing pads"

    local bad=0
    for m in $GUNYAH_MODULES; do
        local f
        f=$(find "$dir" -name "$m.ko" -print -quit 2>/dev/null)
        [ -n "$f" ] || continue
        # The property lives in .note.gnu.property and reads
        # "AArch64 feature: BTI, PAC". `objdump -p` does not print that note at all
        # on this toolchain -- it reports an empty private-flags line -- so the check
        # has to use readelf. Grepping for the token BTI is safe even under a
        # translated locale, because the token itself is never translated.
        if readelf -n "$f" 2>/dev/null | grep -q 'AArch64 feature:.*BTI'; then
            printf '  %-24s BTI present\n' "$m"
        else
            printf '  %-24s NO BTI\n' "$m"
            bad=$((bad + 1))
        fi
    done
    if [ "$bad" -gt 0 ]; then
        die "$bad module(s) have no BTI landing pads; the first call into one will fault. \
Rebuild with -mbranch-protection=standard (the --source path does this already)"
    fi
}

# ---------------------------------------------------------------------------
# build
# ---------------------------------------------------------------------------

do_build() {
    local src="$1" jobs="$2" cc="$3"

    [ -d "$src" ] || die "no such source tree: $src"
    [ -f "$src/Makefile" ] || die "$src does not look like a kernel tree (no Makefile)"

    step "source tree"
    say "  $src"

    for d in $SRC_DIRS; do
        [ -d "$src/$d" ] || warn "$src/$d is missing; that module will not be built"
    done

    # The tree must describe the same release, or vermagic will not match.
    if [ -f "$src/include/config/kernel.release" ]; then
        local tree_rel
        tree_rel=$(cat "$src/include/config/kernel.release")
        say "  tree release: $tree_rel"
        say "  this kernel : $KREL"
        if [ "$tree_rel" != "$KREL" ]; then
            warn "the tree is configured for $tree_rel but the running kernel is $KREL"
            warn "vermagic will not match and the modules will be refused"
            printf 'continue anyway? [y/N] '
            read -r answer
            case "$answer" in y|Y) ;; *) die "aborted" ;; esac
        fi
    else
        warn "no include/config/kernel.release; is the tree configured?"
        say "  if not, run: make ARCH=arm64 olddefconfig"
    fi

    [ -f "$src/.config" ] || die "$src/.config is missing; configure the tree first"
    [ -f "$src/Module.symvers" ] || warn "no Module.symvers; the build may fail on missing symbols"

    step "building"
    # -mbranch-protection=standard is what emits the BTI landing pads the kernel
    # needs; without it every module faults the first time it is called.
    local extra=""
    case "$cc" in
        gcc)   extra="KCFLAGS=-mbranch-protection=standard" ;;
        clang) extra="" ;;   # clang emits them under the tree's own config
        *)     die "unknown compiler: $cc" ;;
    esac

    say "  compiler: $cc"
    say "  KCFLAGS : ${extra:-<none>}"
    say "  jobs    : $jobs"

    local rc=0
    (
        cd "$src" || exit 1
        for d in $SRC_DIRS; do
            [ -d "$d" ] || continue
            echo "--- $d ---"
            # shellcheck disable=SC2086
            if ! make ARCH=arm64 CC="$cc" $extra -j"$jobs" M="$d" modules; then
                # M= needs a tree that has already been built once. A fresh tree has
                # no Module.symvers yet, so fall back to a normal module build, which
                # produces it.
                echo "--- M=$d failed, building the tree's modules instead ---"
                # shellcheck disable=SC2086
                make ARCH=arm64 CC="$cc" $extra -j"$jobs" modules || exit 1
                break
            fi
        done
    ) || rc=$?

    [ "$rc" = 0 ] || die "the build failed"

    step "artifacts"
    local n=0
    for m in $GUNYAH_MODULES; do
        local f
        f=$(find "$src" -name "$m.ko" -print -quit 2>/dev/null)
        if [ -n "$f" ]; then
            printf '  %-24s %s\n' "$m" "$f"
            n=$((n + 1))
        else
            printf '  %-24s not built\n' "$m"
        fi
    done
    [ "$n" -gt 0 ] || die "the build produced no modules"

    echo "$src" > /tmp/librevm-gunyah-built-from
}

# ---------------------------------------------------------------------------
# install
# ---------------------------------------------------------------------------

do_install() {
    local from="$1"

    verify_vermagic "$from"
    verify_bti "$from"

    step "installing into $MODROOT"
    local n=0
    for m in $GUNYAH_MODULES; do
        local f dir dest
        f=$(find "$from" -name "$m.ko" -print -quit 2>/dev/null)
        if [ -z "$f" ]; then
            warn "$m.ko not found, skipping"
            continue
        fi
        dir="${DEST[$m]}"
        dest="$MODROOT/$dir"
        install -d "$dest"
        install -m 644 "$f" "$dest/$m.ko"
        printf '  %-24s -> %s\n' "$m" "$dest/$m.ko"
        n=$((n + 1))
    done
    [ "$n" -gt 0 ] || die "nothing was installed"

    step "refreshing the module index"
    depmod -a "$KREL"
    say "  depmod done"

    step "arranging autoload"
    install -d "$(dirname "$LOADCONF")"
    {
        echo "# Gunyah: this device already boots under Qualcomm's hypervisor, which owns EL2."
        echo "# These modules give Linux the transport to talk to its resource manager."
        echo "# Order matters: the timer and the hypercall layer come first."
        for m in $LOAD_ORDER; do echo "$m"; done
    } > "$LOADCONF"
    say "  wrote $LOADCONF"

    step "loading"
    local failed=0
    for m in $LOAD_ORDER; do
        if lsmod | grep -q "^$m "; then
            printf '  %-24s already loaded\n' "$m"
            continue
        fi
        if modprobe "$m" 2>/dev/null; then
            printf '  %-24s loaded\n' "$m"
        else
            printf '  %-24s FAILED\n' "$m"
            failed=$((failed + 1))
        fi
    done

    step "verifying"
    if [ -c /dev/gunyah ]; then
        say "  /dev/gunyah exists: $(stat -c '%A %U:%G %t:%T' /dev/gunyah)"
    else
        warn "/dev/gunyah did not appear"
    fi
    if dmesg 2>/dev/null | tail -40 | grep -q 'Running under Gunyah hypervisor'; then
        say "  the kernel reports: $(dmesg | grep -m1 'Running under Gunyah hypervisor' | sed 's/.*] //')"
    fi

    if [ "$failed" -gt 0 ]; then
        die "$failed module(s) failed to load; check dmesg"
    fi
    step "done"
    say "  Gunyah is installed and running."
    say "  Survives a reboot via $LOADCONF."
}

# ---------------------------------------------------------------------------
# uninstall
# ---------------------------------------------------------------------------

do_uninstall() {
    need_root
    step "unloading"
    # reverse order, so nothing is removed before its users
    for m in $(echo "$LOAD_ORDER" | tr ' ' '\n' | tac); do
        if lsmod | grep -q "^$m "; then
            modprobe -r "$m" 2>/dev/null && printf '  %-24s unloaded\n' "$m" \
                || printf '  %-24s in use, left alone\n' "$m"
        fi
    done

    step "removing files"
    for m in $GUNYAH_MODULES; do
        local f="$MODROOT/${DEST[$m]}/$m.ko"
        if [ -f "$f" ]; then
            rm -f "$f" && printf '  removed %s\n' "$f"
        fi
    done
    rmdir "$MODROOT/kernel/drivers/virt/gunyah" 2>/dev/null
    rmdir "$MODROOT/kernel/arch/arm64/gunyah" 2>/dev/null

    step "removing autoload"
    rm -f "$LOADCONF" && say "  removed $LOADCONF"

    depmod -a "$KREL"
    say "  depmod done"
}

# ---------------------------------------------------------------------------

usage() {
    sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
}

MODE=
SOURCE=
MODULES=
JOBS=$(nproc 2>/dev/null || echo 4)
CC=gcc

while [ $# -gt 0 ]; do
    case "$1" in
        --source)    MODE=build;   SOURCE="$2";  shift 2 ;;
        --modules)   MODE=installdir; MODULES="$2"; shift 2 ;;
        --status)    MODE=status;  shift ;;
        --uninstall) MODE=uninstall; shift ;;
        --jobs)      JOBS="$2";    shift 2 ;;
        --cc)        CC="$2";      shift 2 ;;
        -h|--help)   usage ;;
        *)           die "unknown argument: $1 (try --help)" ;;
    esac
done

[ -n "$MODE" ] || usage

case "$MODE" in
    status)     do_status ;;
    build)
        need_root
        do_build "$SOURCE" "$JOBS" "$CC"
        do_install "$SOURCE"
        ;;
    installdir)
        need_root
        [ -d "$MODULES" ] || die "no such directory: $MODULES"
        do_install "$MODULES"
        ;;
    uninstall)  do_uninstall ;;
esac
