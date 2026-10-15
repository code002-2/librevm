#!/bin/bash
#
# install.sh -- put librevm on the machine.
#
# Builds the binaries if they are not there, then installs them along with the
# launcher, the desktop entry, the udev rule and the sudoers rule.
#
# Usage:
#   sudo ./scripts/install.sh                  build if needed, then install
#   sudo ./scripts/install.sh --no-build       install only what is already built
#   sudo ./scripts/install.sh --user NAME      the desktop user for the sudoers rule
#   sudo ./scripts/install.sh --uninstall      remove what this installed
#
set -u

PREFIX=/usr/local
SBINDIR=$PREFIX/sbin
BINDIR=$PREFIX/bin
LIBEXECDIR=$PREFIX/libexec
DESTDIR=${DESTDIR:-}

DESKTOP_USER=${SUDO_USER:-}
[ "$DESKTOP_USER" = root ] && DESKTOP_USER=
BUILD=yes
ACTION=install

say()  { printf '%s\n' "$*"; }
step() { printf '\n== %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/.." && pwd)

while [ $# -gt 0 ]; do
    case "$1" in
        --no-build)  BUILD=no; shift ;;
        --user)      DESKTOP_USER="$2"; shift 2 ;;
        --uninstall) ACTION=uninstall; shift ;;
        -h|--help)   sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *)           die "unknown argument: $1" ;;
    esac
done

[ "$(id -u)" = 0 ] || die "must run as root"

p() { printf '%s' "$DESTDIR$1"; }

# ---------------------------------------------------------------------------

do_uninstall() {
    step "removing programs"
    for f in \
        "$SBINDIR/install-gunyah" \
        "$BINDIR/librevm" \
        "$BINDIR/librevm-gui" \
        "$BINDIR/librevm-daemon" \
        "$BINDIR/librevm-gui-session" \
        "$LIBEXECDIR/librevm-get-gunyah"
    do
        if [ -e "$(p "$f")" ]; then
            rm -f "$(p "$f")" && say "  removed $f"
        fi
    done

    step "stopping the daemon"
    systemctl stop librevm-daemon 2>/dev/null || true
    systemctl disable librevm-daemon 2>/dev/null || true
    rm -f "$(p /etc/systemd/system/librevm-daemon.service)" && say "  removed the unit"
    rm -f "$(p /usr/share/dbus-1/system.d/org.librevm.Daemon1.conf)" && say "  removed the bus policy"
    systemctl daemon-reload 2>/dev/null || true

    step "removing integration"
    for f in \
        /usr/share/applications/librevm.desktop \
        /usr/share/icons/hicolor/scalable/apps/librevm.svg \
        /etc/udev/rules.d/60-gunyah.rules \
        /etc/sudoers.d/librevm-gui
    do
        if [ -e "$(p "$f")" ]; then
            rm -f "$(p "$f")" && say "  removed $f"
        fi
    done

    update-desktop-database /usr/share/applications 2>/dev/null || true
    udevadm control --reload-rules 2>/dev/null || true
    say ""
    say "Note: the Gunyah modules and /etc/modules-load.d/gunyah.conf are left alone."
    say "      Remove them with: install-gunyah --uninstall"
    say "      The store in /var/lib/librevm is left alone too."
}

# ---------------------------------------------------------------------------

do_install() {
    step "binaries"
    local core=$root/target/release/librevm
    local gui=$root/target/release/librevm-gui
    local daemon=$root/target/release/librevm-daemon

    if [ "$BUILD" = yes ] && { [ ! -x "$core" ] || [ ! -x "$gui" ] || [ ! -x "$daemon" ]; }; then
        say "  building (this takes a few minutes the first time)"
        command -v cargo >/dev/null 2>&1 || die "cargo is not installed"
        # Limited on purpose. An unqualified `cargo build` on this class of device takes
        # every core and leaves the desktop unable to answer, which is a poor trade for
        # finishing a few minutes sooner.
        ( cd "$root" && nice -n 10 cargo build --release -j2 ) || die "the build failed"
    fi

    [ -x "$core" ]   || die "$core is missing; run without --no-build"
    [ -x "$gui" ]    || die "$gui is missing; run without --no-build"
    [ -x "$daemon" ] || die "$daemon is missing; run without --no-build"

    install -d "$(p "$BINDIR")" "$(p "$SBINDIR")" "$(p "$LIBEXECDIR")"
    install -m 755 "$core"   "$(p "$BINDIR/librevm")"
    install -m 755 "$gui"    "$(p "$BINDIR/librevm-gui")"
    install -m 755 "$daemon" "$(p "$BINDIR/librevm-daemon")"
    say "  $BINDIR/librevm"
    say "  $BINDIR/librevm-gui"
    say "  $BINDIR/librevm-daemon"

    step "scripts"
    install -m 755 "$here/librevm-gui-session"   "$(p "$BINDIR/librevm-gui-session")"
    install -m 755 "$here/install-gunyah.sh"     "$(p "$SBINDIR/install-gunyah")"
    install -m 755 "$here/librevm-get-gunyah.sh" "$(p "$LIBEXECDIR/librevm-get-gunyah")"
    say "  $BINDIR/librevm-gui-session"
    say "  $SBINDIR/install-gunyah"
    say "  $LIBEXECDIR/librevm-get-gunyah"

    step "daemon integration"
    install -d "$(p /usr/lib/systemd/system)" "$(p /usr/share/dbus-1/system.d)"
    # The unit names /usr/bin, which is where a package puts it; re-point it at wherever
    # this script is installing.
    sed "s|^ExecStart=/usr/bin/|ExecStart=$BINDIR/|" \
        "$root/packaging/librevm-daemon.service" \
        > "$(p /usr/lib/systemd/system/librevm-daemon.service)"
    install -m 644 "$root/packaging/org.librevm.Daemon1.conf" \
        "$(p /usr/share/dbus-1/system.d/org.librevm.Daemon1.conf)"
    say "  /usr/lib/systemd/system/librevm-daemon.service"
    say "  /usr/share/dbus-1/system.d/org.librevm.Daemon1.conf"

    # A unit left in /etc by an older install would shadow this one and name a path
    # that may no longer exist.
    if [ -z "$DESTDIR" ] && [ -e /etc/systemd/system/librevm-daemon.service ]; then
        rm -f /etc/systemd/system/librevm-daemon.service
        say "  removed a stale /etc/systemd/system/librevm-daemon.service"
    fi

    step "state directory"
    install -d -m 2775 "$(p /var/lib/librevm)" "$(p /var/lib/librevm/kernels)" \
        "$(p /var/lib/librevm/images)" "$(p /run/librevm)"
    # The window reads the store directly, so the group has to be able to.
    chgrp -R librevm "$(p /var/lib/librevm)" "$(p /run/librevm)" 2>/dev/null || true
    chmod -R g+rX "$(p /var/lib/librevm)" 2>/dev/null || true
    say "  /var/lib/librevm and /run/librevm (group librevm)"

    step "gunyah access group"
    if getent group librevm >/dev/null 2>&1; then
        say "  group librevm already exists"
    else
        groupadd -r librevm && say "  created group librevm"
    fi
    if [ -n "$DESKTOP_USER" ]; then
        if id "$DESKTOP_USER" >/dev/null 2>&1; then
            usermod -aG librevm "$DESKTOP_USER" \
                && say "  added $DESKTOP_USER to librevm"
        else
            warn "no such user: $DESKTOP_USER"
        fi
    else
        warn "no desktop user given; add one later with: usermod -aG librevm USER"
        warn "or re-run with: $0 --user NAME"
    fi

    step "desktop entry and icon"
    install -d "$(p /usr/share/applications)" "$(p /usr/share/icons/hicolor/scalable/apps)"
    install -m 644 "$root/packaging/librevm.desktop" \
        "$(p /usr/share/applications/librevm.desktop)"
    if [ -f "$root/packaging/librevm.svg" ]; then
        install -m 644 "$root/packaging/librevm.svg" \
            "$(p /usr/share/icons/hicolor/scalable/apps/librevm.svg)"
        gtk-update-icon-cache -f -t "$(p /usr/share/icons/hicolor)" 2>/dev/null || true
    fi
    update-desktop-database "$(p /usr/share/applications)" 2>/dev/null || true
    say "  /usr/share/applications/librevm.desktop"

    step "udev rule"
    install -d "$(p /etc/udev/rules.d)"
    install -m 644 "$root/packaging/60-gunyah.rules" \
        "$(p /etc/udev/rules.d/60-gunyah.rules)"
    say "  /etc/udev/rules.d/60-gunyah.rules"

    step "log rotation"
    # The window's output is chatty. Without a ceiling one loop took the log to 95 GB and
    # filled the root filesystem.
    install -d "$(p /etc/logrotate.d)"
    install -m 644 "$root/packaging/logrotate-librevm" "$(p /etc/logrotate.d/librevm)"
    say "  /etc/logrotate.d/librevm"

    step "group membership"
    if [ -z "$DESTDIR" ] && [ -n "$DESKTOP_USER" ]; then
        # Being in the group is the whole privilege story now: it lets the account reach
        # the daemon on the system bus. There is no sudoers rule to write.
        if id "$DESKTOP_USER" >/dev/null 2>&1; then
            if id -nG "$DESKTOP_USER" 2>/dev/null | tr ' ' '\n' | grep -qx librevm; then
                say "  $DESKTOP_USER is already in librevm"
            else
                usermod -aG librevm "$DESKTOP_USER" 2>/dev/null \
                    && say "  added $DESKTOP_USER to librevm (log out and back in)" \
                    || warn "could not add $DESKTOP_USER to librevm"
            fi
        else
            warn "no such user: $DESKTOP_USER"
        fi
    fi

    step "starting the daemon"
    if [ -z "$DESTDIR" ]; then
        systemctl daemon-reload 2>/dev/null || true
        if systemctl enable --now librevm-daemon 2>/dev/null; then
            say "  librevm-daemon is running"
        else
            warn "could not start librevm-daemon; the window will manage the store directly"
            warn "which needs it to be run as root"
        fi
        # A rule from an older install names a helper this version does not ship.
        rm -f /etc/sudoers.d/librevm-gui 2>/dev/null || true
    fi

    # Both halves log to the same place. If the file is root-only, the launcher's own
    # write fails and the redirection kills the launch before the window appears.
    for f in /var/log/librevm-gui.log /tmp/librevm-gui.log; do
        [ -n "$DESTDIR" ] && continue
        touch "$f" 2>/dev/null && chmod 666 "$f" 2>/dev/null
    done
    say "  /var/log/librevm-gui.log is world-writable (the launcher needs it)"

    step "gunyah"
    if [ -c /dev/gunyah ] && lsmod 2>/dev/null | grep -Eq '^(gunyah_rsc_mgr|gh_rm_drv) '; then
        say "  Gunyah is already available"
    else
        say "  Gunyah is not available yet."
        say "  The GUI will offer to install it on first start, or run:"
        say "      install-gunyah --source /path/to/linux-tree"
        say "      install-gunyah --modules /path/to/prebuilt"
    fi

    step "done"
    say "  Run the GUI with:      librevm-gui-session"
    say "  Or the command line:   librevm list"
}

case "$ACTION" in
    install)   do_install ;;
    uninstall) do_uninstall ;;
esac
