#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# quicKFS Linux server uninstaller.
#
#   sudo ./uninstall.sh
#   DESTDIR=/tmp/quickfs-package ./uninstall.sh
#
# This removes the program. It does not remove your data.
#
# Preserved, always:
#   /etc/quickfs          configuration, including quickfs.toml
#   /var/lib/quickfs      server identity, accounts, pairing records, export epoch
#   the quickfs user and group   (files above are owned by them)
#   the export root       never read, never touched, never named here
#
# There is no --purge. A safe uninstall matters more than a convenient one:
# deleting /var/lib/quickfs destroys the server identity (every exact-pin client
# must be re-paired) and the export epoch (every client discards its cache and
# reconnects as if to a different server), and there is no recovery. If you
# genuinely want that, this script prints the exact commands so the decision and
# the paths are both yours.

set -eu

PROGRAM="quickfs-server-daemon"
SERVICE="quickfs-server.service"
SERVICE_NAME="quickfs-server"
SERVICE_USER="quickfs"
SERVICE_GROUP="quickfs"

BIN_DIR="/usr/local/bin"
CONFIG_DIR="/etc/quickfs"
STATE_DIR="/var/lib/quickfs"
UNIT_DIR="/etc/systemd/system"

DESTDIR="${DESTDIR:-}"

log() { printf '%s\n' "$*"; }
step() { printf '==> %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
fail() { printf 'error: %s\n' "$*" >&2; exit 1; }

usage() {
    cat <<'USAGE'
Usage: uninstall.sh [OPTIONS]

  --keep-unit   Leave the systemd unit file in place.
  -h, --help    Show this message.

Environment:
  DESTDIR   Operate on a staged layout under this directory instead of the real
            system. systemd is never contacted and no account is touched.
USAGE
}

KEEP_UNIT=0
while [ $# -gt 0 ]; do
    case "$1" in
        --keep-unit) KEEP_UNIT=1; shift ;;
        -h|--help) usage; exit 0 ;;
        --purge)
            usage >&2
            fail "--purge is not implemented on purpose; see the notes this script prints"
            ;;
        *) usage >&2; fail "unknown option '$1'" ;;
    esac
done

if [ -n "$DESTDIR" ]; then
    case "$DESTDIR" in
        /*) ;;
        *) fail "DESTDIR must be an absolute path" ;;
    esac
    REAL_UNINSTALL=0
else
    REAL_UNINSTALL=1
    [ "$(id -u)" = "0" ] || fail "run this uninstaller as root, or set DESTDIR"
fi

DEST_BINARY="$DESTDIR$BIN_DIR/$PROGRAM"
DEST_UNIT="$DESTDIR$UNIT_DIR/$SERVICE"

systemd_present() {
    [ "$REAL_UNINSTALL" = "1" ] && [ -d /run/systemd/system ] \
        && command -v systemctl >/dev/null 2>&1
}

# Remove one specific file at a path this script computed from constants. Never
# a glob, never a recursive removal, and never through a symbolic link: a link
# at an installed path is reported rather than followed, because following it
# would delete whatever it points at.
remove_known_file() {
    path="$1"
    label="$2"
    if [ -L "$path" ]; then
        warn "$label ('$path') is a symbolic link; leaving it alone. Inspect and remove it yourself."
        return 0
    fi
    if [ ! -e "$path" ]; then
        log "$label was not installed at '$path'."
        return 0
    fi
    if [ ! -f "$path" ]; then
        warn "$label ('$path') is not a regular file; leaving it alone."
        return 0
    fi
    step "Removing $path"
    rm -f -- "$path"
}

if systemd_present; then
    if systemctl list-unit-files "$SERVICE" >/dev/null 2>&1; then
        step "Stopping $SERVICE_NAME"
        systemctl stop "$SERVICE_NAME" 2>/dev/null || true
        step "Disabling $SERVICE_NAME"
        systemctl disable "$SERVICE_NAME" 2>/dev/null || true
    fi
fi

remove_known_file "$DEST_BINARY" "The daemon binary"

if [ "$KEEP_UNIT" = "0" ]; then
    remove_known_file "$DEST_UNIT" "The systemd unit"
else
    log "Keeping the systemd unit at '$DEST_UNIT'."
fi

if systemd_present; then
    step "Reloading systemd"
    systemctl daemon-reload
    systemctl reset-failed "$SERVICE_NAME" 2>/dev/null || true
fi

cat <<SUMMARY

Removed
-------
  $BIN_DIR/$PROGRAM
SUMMARY

if [ "$KEEP_UNIT" = "0" ]; then
    log "  $UNIT_DIR/$SERVICE"
fi

cat <<SUMMARY

Preserved
---------
  $CONFIG_DIR                  configuration and quickfs.toml.example
  $CONFIG_DIR/quickfs.toml     your edited configuration
  $STATE_DIR              server identity, accounts, pairing records, export epoch
  the '$SERVICE_USER' user and '$SERVICE_GROUP' group, which own the state above
  your export root - this script never reads the configuration and never
  touches exported files

Reinstalling later reuses all of it: identity, accounts, pairing records and the
server epoch survive, so clients do not need to be re-paired.

If you have decided you want the configuration and state gone, back them up
first and then remove them yourself, checking each path as you type it:

  sudo tar -czf quickfs-state-backup.tar.gz $CONFIG_DIR $STATE_DIR
  sudo rm -ri $STATE_DIR
  sudo rm -ri $CONFIG_DIR
  sudo userdel $SERVICE_USER
  sudo groupdel $SERVICE_GROUP

Deleting $STATE_DIR destroys the server identity and the export epoch. Every
exact-pin client must then be paired again, and every client discards its cache.
There is no recovery from that.

SUMMARY
