#!/bin/bash
#
# librevm-get-gunyah.sh -- obtain and install the Gunyah modules.
#
# The GUI calls this on first start when it cannot find /dev/gunyah. It is also usable
# by hand.
#
# It is split from install-gunyah.sh on purpose:
#
#   * install-gunyah.sh knows how to build, verify and install modules, and assumes
#     the bits are already on the machine.
#   * this script knows how to get those bits -- from a local kernel tree, from a
#     directory of prebuilt modules, or by fetching them from the project's
#     repository -- and then hands over to install-gunyah.sh.
#
# Usage:
#   librevm-get-gunyah.sh --check              exit 0 if Gunyah is usable, else 1
#   librevm-get-gunyah.sh --install            fetch if needed, then install
#   librevm-get-gunyah.sh --fetch              only fetch, do not install
#   librevm-get-gunyah.sh --install --source DIR
#   librevm-get-gunyah.sh --install --modules DIR
#
# Options:
#   --repo URL       git URL to fetch from (default: the upstream repository)
#   --prefix DIR     where to keep fetched things (default: /var/lib/librevm/gunyah)
#   --branch NAME    branch to fetch (default: main)
#   --force          reinstall even if Gunyah already works
#   --quiet          only report problems
#
set -u

REPO_DEFAULT="https://github.com/code002-2/librevm.git"
BRANCH_DEFAULT="main"
PREFIX_DEFAULT="/var/lib/librevm/gunyah"

REPO="$REPO_DEFAULT"
BRANCH="$BRANCH_DEFAULT"
PREFIX="$PREFIX_DEFAULT"
FORCE=no
QUIET=no
MODE=
SOURCE=
MODULES=

say()  { [ "$QUIET" = yes ] || printf '%s\n' "$*"; }
note() { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# is Gunyah usable?
# ---------------------------------------------------------------------------

# The device is only useful if /dev/gunyah exists AND the modules behind it are
# loaded. A stray device node from a previous boot is not enough.
gunyah_ready() {
    [ -c /dev/gunyah ] || return 1
    lsmod 2>/dev/null | grep -q '^gunyah_rsc_mgr ' || return 1
    return 0
}

describe_state() {
    if [ -c /dev/gunyah ]; then
        say "  /dev/gunyah: $(stat -c '%A %U:%G' /dev/gunyah)"
    else
        say "  /dev/gunyah: absent"
    fi
    local n
    n=$(lsmod 2>/dev/null | grep -cE '^gunyah')
    say "  gunyah modules loaded: $n"
    if [ -r /proc/config.gz ]; then
        if zcat /proc/config.gz 2>/dev/null | grep -q '^CONFIG_ARM64_BTI_KERNEL=y'; then
            say "  kernel BTI: enabled (modules need landing pads)"
        fi
    fi
}

do_check() {
    if gunyah_ready; then
        say "Gunyah is available."
        describe_state
        return 0
    fi
    note "Gunyah is not available."
    describe_state
    return 1
}

# ---------------------------------------------------------------------------
# finding install-gunyah.sh
# ---------------------------------------------------------------------------

# Where an already installed copy might live.
find_installer() {
    for p in \
        /usr/local/sbin/install-gunyah \
        /usr/local/bin/install-gunyah \
        "$(dirname "$0")/install-gunyah.sh" \
        "$PREFIX/install-gunyah.sh"
    do
        [ -x "$p" ] && { echo "$p"; return 0; }
    done
    return 1
}

# ---------------------------------------------------------------------------
# fetching
# ---------------------------------------------------------------------------

have_git() { command -v git >/dev/null 2>&1; }
have_net() {
    # a cheap reachability probe; getent is in libc so it is always present
    getent hosts "$(printf '%s' "$REPO" | sed -E 's#^[a-z]+://([^/:]+).*#\1#')" \
        >/dev/null 2>&1
}

fetch_repo() {
    say "fetching $REPO ($BRANCH) into $PREFIX"

    if ! have_git; then
        die "git is not installed, and it is needed to fetch the repository. \
Install git, or pass --modules with modules you already have"
    fi
    if ! have_net; then
        die "cannot reach the host in $REPO. Check the network, or pass --modules"
    fi

    install -d "$PREFIX"

    if [ -d "$PREFIX/.git" ]; then
        say "  updating the existing checkout"
        git -C "$PREFIX" fetch --depth 1 origin "$BRANCH" \
            || die "git fetch failed"
        git -C "$PREFIX" checkout -q FETCH_HEAD || die "git checkout failed"
    else
        rm -rf "$PREFIX/.git" "$PREFIX/scripts" "$PREFIX/packaging" 2>/dev/null
        git clone --depth 1 --branch "$BRANCH" "$REPO" "$PREFIX" \
            || die "git clone failed"
    fi

    [ -d "$PREFIX/scripts" ] || die "the checkout has no scripts/ directory"
    say "  fetched into $PREFIX"
    chmod +x "$PREFIX"/scripts/*.sh 2>/dev/null

    # Make it available system-wide so the next run does not fetch again.
    if [ -x "$PREFIX/scripts/install-gunyah.sh" ] && [ -d /usr/local/sbin ]; then
        install -m 755 "$PREFIX/scripts/install-gunyah.sh" /usr/local/sbin/install-gunyah
        say "  installed the installer to /usr/local/sbin/install-gunyah"
    fi
}

# Modules built for this exact kernel release. A mismatch is refused by the kernel
# with a bare "Exec format error", so the release is part of the path.
prebuilt_dir() {
    echo "$PREFIX/modules/$(uname -r)"
}

fetch_prebuilt() {
    local dir
    dir=$(prebuilt_dir)
    if [ -d "$dir" ] && ls "$dir"/*.ko >/dev/null 2>&1; then
        say "  using prebuilt modules in $dir"
        echo "$dir"
        return 0
    fi
    return 1
}

# ---------------------------------------------------------------------------
# install
# ---------------------------------------------------------------------------

do_install() {
    local installer
    installer=$(find_installer) || installer=

    if [ -z "$installer" ]; then
        say "no installer on this machine; fetching one"
        fetch_repo
        installer=$(find_installer) || die "still no installer after fetching"
    fi
    say "installer: $installer"

    # Work out what to install from.
    if [ -n "$SOURCE" ]; then
        [ -d "$SOURCE" ] || die "no such source tree: $SOURCE"
        say "building from $SOURCE"
        "$installer" --source "$SOURCE"
        return $?
    fi

    if [ -n "$MODULES" ]; then
        [ -d "$MODULES" ] || die "no such directory: $MODULES"
        say "installing from $MODULES"
        "$installer" --modules "$MODULES"
        return $?
    fi

    # A tree already inside the checkout is the nicest case.
    if [ -d "$PREFIX/linux" ]; then
        say "found a kernel tree at $PREFIX/linux"
        "$installer" --source "$PREFIX/linux"
        return $?
    fi

    # Otherwise see whether modules for this kernel shipped with the checkout.
    local prebuilt
    if prebuilt=$(fetch_prebuilt); then
        "$installer" --modules "$prebuilt"
        return $?
    fi

    # Last resort: fetch the repository, which may carry a kernel tree or prebuilt
    # modules, and try again.
    if [ ! -d "$PREFIX/.git" ]; then
        say "nothing to install from yet; fetching the repository"
        fetch_repo
    fi

    if [ -d "$PREFIX/linux" ]; then
        say "found a kernel tree at $PREFIX/linux after fetching"
        "$installer" --source "$PREFIX/linux"
        return $?
    fi

    if prebuilt=$(fetch_prebuilt); then
        "$installer" --modules "$prebuilt"
        return $?
    fi

    cat >&2 <<EOF
error: nothing to install Gunyah from.

What is needed is either:

  * a kernel tree configured for this release ($(uname -r)), so the modules can be
    built against it, or
  * module files already built for it.

Neither was found, and the repository does not carry them. Provide one:

  $0 --install --source /path/to/linux-$(uname -r)
  $0 --install --modules /path/to/prebuilt

A tree has to be the one this kernel was built from: the release string is part of
the module's vermagic, and the kernel refuses anything else with a bare
"Exec format error".
EOF
    return 1
}

# ---------------------------------------------------------------------------

usage() {
    sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
}

while [ $# -gt 0 ]; do
    case "$1" in
        --check)   MODE=check;   shift ;;
        --install) MODE=install; shift ;;
        --fetch)   MODE=fetch;   shift ;;
        --source)  SOURCE="$2";  shift 2 ;;
        --modules) MODULES="$2"; shift 2 ;;
        --repo)    REPO="$2";    shift 2 ;;
        --branch)  BRANCH="$2";  shift 2 ;;
        --prefix)  PREFIX="$2";  shift 2 ;;
        --force)   FORCE=yes;    shift ;;
        --quiet)   QUIET=yes;    shift ;;
        -h|--help) usage ;;
        *)         die "unknown argument: $1 (try --help)" ;;
    esac
done

[ -n "$MODE" ] || usage

case "$MODE" in
    check)
        do_check
        exit $?
        ;;
    fetch)
        fetch_repo
        ;;
    install)
        if [ "$FORCE" = no ] && gunyah_ready; then
            say "Gunyah is already available; nothing to do (use --force to reinstall)"
            describe_state
            exit 0
        fi
        if [ "$(id -u)" != 0 ]; then
            die "installing needs root (it writes to /lib/modules and /etc)"
        fi
        do_install
        rc=$?
        [ "$rc" = 0 ] || exit "$rc"

        if gunyah_ready; then
            say ""
            say "Gunyah is now available."
            describe_state
            exit 0
        fi
        die "the installer finished but Gunyah is still not usable; check dmesg"
        ;;
esac
