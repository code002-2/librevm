#!/bin/bash
#
# build-deb.sh -- assemble a .deb for this architecture.
#
# Run it where the binaries were built. It expects target/release to be populated;
# pass --build to make it run cargo first.
#
# Usage:
#   ./scripts/build-deb.sh                 use target/release as it is
#   ./scripts/build-deb.sh --build         run cargo build --release first
#   ./scripts/build-deb.sh --out DIR       where to leave the .deb (default: dist)
#   ./scripts/build-deb.sh --version X.Y.Z override the version
#
# The dependency list is taken from dpkg-shlibdeps when it is available, so the
# package declares what it actually links against rather than a hand-written guess.
#
set -u

VERSION=0.2.0
OUT=dist
BUILD=no

say()  { printf '%s\n' "$*"; }
step() { printf '\n== %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

root=$(cd "$(dirname "$0")/.." && pwd)

while [ $# -gt 0 ]; do
    case "$1" in
        --build)   BUILD=yes; shift ;;
        --out)     OUT="$2"; shift 2 ;;
        --version) VERSION="$2"; shift 2 ;;
        -h|--help) sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *)         die "unknown argument: $1" ;;
    esac
done

ARCH=$(dpkg --print-architecture 2>/dev/null) || die "dpkg is needed to find the architecture"
[ -n "$ARCH" ] || die "could not determine the architecture"

step "building the binaries"
if [ "$BUILD" = yes ]; then
    command -v cargo >/dev/null 2>&1 || die "cargo is not installed"
    ( cd "$root" && cargo build --release ) || die "the build failed"
fi
CORE=$root/target/release/librevm
GUI=$root/target/release/librevm-gui
DAEMON=$root/target/release/librevm-daemon
[ -x "$CORE" ]   || die "$CORE is missing; run with --build, or build first"
[ -x "$GUI" ]    || die "$GUI is missing; run with --build, or build first"
[ -x "$DAEMON" ] || die "$DAEMON is missing; run with --build, or build first"

pkgroot=$(mktemp -d)
trap 'rm -rf "$pkgroot"' EXIT
name="librevm_${VERSION}_${ARCH}"
say "  package: $name"

step "assembling the file tree"
# Standard locations, unlike the by-hand install which uses /usr/local.
install -d "$pkgroot/DEBIAN"
install -d "$pkgroot/usr/bin"
install -d "$pkgroot/usr/sbin"
install -d "$pkgroot/usr/libexec"
install -d "$pkgroot/usr/lib/udev/rules.d"
install -d "$pkgroot/usr/lib/systemd/system"
install -d "$pkgroot/usr/share/applications"
install -d "$pkgroot/usr/share/icons/hicolor/scalable/apps"
install -d "$pkgroot/usr/share/dbus-1/system.d"
install -d "$pkgroot/usr/share/doc/librevm"
install -d "$pkgroot/usr/share/librevm"

install -m 755 "$CORE"   "$pkgroot/usr/bin/librevm"
install -m 755 "$GUI"    "$pkgroot/usr/bin/librevm-gui"
# The privileged half. The window talks to it over the system bus instead of running
# as root itself.
install -m 755 "$DAEMON" "$pkgroot/usr/bin/librevm-daemon"
install -m 755 "$root/scripts/librevm-gui-session"      "$pkgroot/usr/bin/librevm-gui-session"
install -m 755 "$root/scripts/install-gunyah.sh"        "$pkgroot/usr/sbin/install-gunyah"
install -m 755 "$root/scripts/librevm-get-gunyah.sh"    "$pkgroot/usr/libexec/librevm-get-gunyah"
# Lets a desktop user launch the window, which needs root for Gunyah. The rule names an
# account, so it cannot be written at install time and is a separate command.
install -m 755 "$root/scripts/librevm-enable-desktop"   "$pkgroot/usr/bin/librevm-enable-desktop"

install -m 644 "$root/packaging/librevm.desktop" "$pkgroot/usr/share/applications/librevm.desktop"
install -m 644 "$root/packaging/librevm.svg"     "$pkgroot/usr/share/icons/hicolor/scalable/apps/librevm.svg"
install -m 644 "$root/packaging/60-gunyah.rules" "$pkgroot/usr/lib/udev/rules.d/60-gunyah.rules"
# The bus policy is what lets the librevm group reach the daemon, and the unit is what
# starts it at boot.
install -m 644 "$root/packaging/org.librevm.Daemon1.conf"  "$pkgroot/usr/share/dbus-1/system.d/org.librevm.Daemon1.conf"
install -m 644 "$root/packaging/librevm-daemon.service"    "$pkgroot/usr/lib/systemd/system/librevm-daemon.service"
# The window's log is chatty; without rotation it once reached 95 GB.
install -d "$pkgroot/etc/logrotate.d"
install -m 644 "$root/packaging/logrotate-librevm"         "$pkgroot/etc/logrotate.d/librevm"

install -m 644 "$root/README.md"  "$pkgroot/usr/share/doc/librevm/README.md"
install -m 644 "$root/LICENSE"    "$pkgroot/usr/share/doc/librevm/copyright"
gzip -9n -c "$root/packaging/debian/changelog" > "$pkgroot/usr/share/doc/librevm/changelog.gz"
chmod 644 "$pkgroot/usr/share/doc/librevm/changelog.gz"

say "  files in place"

step "control file"
# The template is a source package's control: a source stanza followed by a binary
# one. dpkg-deb wants only the binary stanza, and only the fields that belong to a
# binary package -- carrying Source or Build-Depends across makes it warn.
sed -n '/^Package:/,$p' "$root/packaging/debian/control" \
    | grep -vE '^(Source|Build-Depends|Rules-Requires-Root|Vcs-[A-Za-z]+):' \
    > "$pkgroot/DEBIAN/control"
sed -i "s|^Architecture:.*|Architecture: $ARCH|" "$pkgroot/DEBIAN/control"
sed -i "/^Architecture:/i Version: $VERSION" "$pkgroot/DEBIAN/control"

# Maintainer belongs to the source stanza, so it was dropped along with it. A binary
# package needs its own; dpkg-deb warns without it and the archive tools reject it.
if ! grep -q '^Maintainer:' "$pkgroot/DEBIAN/control"; then
    maintainer=$(sed -n 's/^Maintainer:[[:space:]]*//p' "$root/packaging/debian/control" | head -1)
    [ -n "$maintainer" ] || maintainer="code002-2 <hiez832670@gmail.com>"
    sed -i "/^Version:/a Maintainer: $maintainer" "$pkgroot/DEBIAN/control"
fi

# dpkg-shlibdeps gives the real dependency list; fall back to the set this machine
# was built against, which is what the template would otherwise leave as a literal
# ${shlibs:Depends} for dpkg-deb to choke on.
deps=
if command -v dpkg-shlibdeps >/dev/null 2>&1; then
    # The daemon is included too: it links its own libc and whatever zbus pulls in, and
    # leaving it out would produce a package that installs a binary it cannot run.
    deps=$(cd "$pkgroot" && dpkg-shlibdeps -O usr/bin/librevm-gui usr/bin/librevm usr/bin/librevm-daemon 2>/dev/null \
        | sed -n 's/^shlibs:Depends=//p')
fi
if [ -n "$deps" ]; then
    say "  dependencies from dpkg-shlibdeps"
    sed -i "s|\${shlibs:Depends}|$deps|" "$pkgroot/DEBIAN/control"
else
    warn "dpkg-shlibdeps unavailable; using the known dependency set for trixie"
    sed -i "s|\${shlibs:Depends}|libadwaita-1-0 (>= 1.5~beta), libc6 (>= 2.39), libgcc-s1 (>= 4.2), libglib2.0-0t64 (>= 2.54.0), libgtk-4-1 (>= 4.12.0)|" \
        "$pkgroot/DEBIAN/control"
fi

sed -i "s|\${misc:Depends}||" "$pkgroot/DEBIAN/control"
# `Depends: a, b, ` and other trailing commas are not valid
sed -i 's/,[[:space:]]*$//' "$pkgroot/DEBIAN/control"
sed -i 's/[[:space:]]*$//' "$pkgroot/DEBIAN/control"

say "  --- control ---"
sed 's/^/    /' "$pkgroot/DEBIAN/control"

step "maintainer scripts"
for s in postinst prerm postrm; do
    install -m 755 "$root/packaging/debian/$s" "$pkgroot/DEBIAN/$s"
    say "  $s"
done

step "building the package"
install -d "$root/$OUT"
dpkg-deb --root-owner-group --build "$pkgroot" "$root/$OUT/$name.deb" \
    || die "dpkg-deb failed"

step "result"
ls -la "$root/$OUT/$name.deb" | sed 's/^/  /'
say ""
say "  --- contents ---"
dpkg-deb -c "$root/$OUT/$name.deb" 2>/dev/null | awk '{printf "    %s %s\n", $1, $6}'
say ""
say "  --- metadata ---"
dpkg-deb -f "$root/$OUT/$name.deb" 2>/dev/null | sed 's/^/    /'

# checksums, so the release notes can carry them
( cd "$root/$OUT" && sha256sum "$name.deb" > "$name.deb.sha256" )
say ""
say "  sha256:"
sed 's/^/    /' "$root/$OUT/$name.deb.sha256"
