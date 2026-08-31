#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# Installer and packaging tests.
#
#   packaging/tests/run.sh [REPO_ROOT]
#
# These tests never require root, never write to the real /etc, /usr or /var,
# never create a user or group, never invoke systemd, and never read or modify
# the developer's own QuickFS state. Everything happens under one mktemp -d
# working directory with DESTDIR, and the commands the installer would reach for
# on a real system (curl, uname, systemctl, getent, useradd, groupadd) are
# replaced by stubs on PATH so results are deterministic.

set -eu

ROOT="${1:-}"
if [ -z "$ROOT" ]; then
    ROOT="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
fi
PACKAGING="$ROOT/packaging"
INSTALL_SH="$PACKAGING/install.sh"
UNINSTALL_SH="$PACKAGING/uninstall.sh"

[ -f "$INSTALL_SH" ] || { echo "cannot find $INSTALL_SH" >&2; exit 1; }

WORK="$(mktemp -d "${TMPDIR:-/tmp}/quickfs-packaging-tests.XXXXXXXX")"
cleanup() {
    status=$?
    case "$WORK" in
        */quickfs-packaging-tests.*)
            if [ -d "$WORK" ]; then
                rm -r -- "$WORK"
            fi
            ;;
    esac
    exit "$status"
}
trap cleanup EXIT HUP INT TERM

PASS=0
FAIL=0
CURRENT=""
MODES_SUPPORTED=1
SYMLINKS_SUPPORTED=1

start() { CURRENT="$1"; }
ok() { PASS=$((PASS + 1)); printf 'ok   %s\n' "$CURRENT"; }
# Report success only when every assertion in the case passed. Written as an
# `if` rather than `[ ... ] && ok` so a failing case does not abort the suite
# through `set -e`.
ok_if() { if [ "$1" = "0" ]; then ok; fi; }
bad() { FAIL=$((FAIL + 1)); printf 'FAIL %s: %s\n' "$CURRENT" "$1"; }
skip() { printf 'skip %s: %s\n' "$CURRENT" "$1"; }

assert_file() {
    if [ -f "$1" ]; then return 0; fi
    bad "expected file '$1'"
    return 1
}

assert_absent() {
    if [ ! -e "$1" ]; then return 0; fi
    bad "expected '$1' to be gone"
    return 1
}

file_mode() {
    if stat -c '%a' "$1" >/dev/null 2>&1; then
        stat -c '%a' "$1"
    else
        stat -f '%Lp' "$1"
    fi
}

assert_mode() {
    if [ "$MODES_SUPPORTED" != "1" ]; then return 0; fi
    actual="$(file_mode "$1")"
    if [ "$actual" = "$2" ]; then return 0; fi
    bad "'$1' has mode $actual, expected $2"
    return 1
}

# A filesystem that cannot store Unix modes (a Windows checkout under Git Bash,
# a FAT volume) reports 0755 for everything, which would fail every mode
# assertion for a reason that has nothing to do with the installer. Detect that
# once and say so, rather than either failing or quietly pretending to check.
detect_mode_support() {
    probe="$WORK/mode-probe"
    mkdir -p "$probe"
    chmod 0750 "$probe" 2>/dev/null || true
    if [ "$(file_mode "$probe")" = "750" ]; then
        MODES_SUPPORTED=1
    else
        MODES_SUPPORTED=0
    fi
}

# Run a command that is expected to fail, capturing its output.
expect_failure() {
    log="$1"
    shift
    if "$@" >"$log" 2>&1; then
        return 1
    fi
    return 0
}

# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

# Symlink-dependent cases need real symbolic links. A Windows checkout without
# developer mode cannot create one, and those cases say so rather than failing.
detect_symlink_support() {
    probe="$WORK/symlink-probe"
    if ln -s "$WORK" "$probe" 2>/dev/null && [ -L "$probe" ]; then
        SYMLINKS_SUPPORTED=1
    else
        SYMLINKS_SUPPORTED=0
    fi
    rm -f -- "$probe" 2>/dev/null || true
}

detect_mode_support
detect_symlink_support
if [ "$MODES_SUPPORTED" != "1" ]; then
    printf 'note: this filesystem cannot store Unix modes; mode assertions are skipped\n'
fi

FAKE_BIN="$WORK/fakebin"
FIXTURES="$WORK/fixtures"
mkdir -p "$FAKE_BIN" "$FIXTURES"

STUB_VERSION="9.9.9-test"

# A stand-in for the real daemon. The installer only executes the binary during
# a real root installation, but the packaging tests still assert that whatever
# was installed reports the expected version.
cat >"$WORK/stub-daemon" <<STUB
#!/bin/sh
case "\$1" in
    --version) echo "quickfs-server-daemon $STUB_VERSION" ;;
    --help) echo "usage: quickfs-server-daemon <COMMAND>" ;;
    config) echo "Configuration is valid." ;;
    *) echo "stub: \$*" ;;
esac
STUB
chmod 0755 "$WORK/stub-daemon"

# uname stub: only -m is faked, everything else defers to the real one.
REAL_UNAME="$(command -v uname)"
cat >"$FAKE_BIN/uname" <<STUB
#!/bin/sh
if [ "\${1:-}" = "-m" ]; then
    printf '%s\n' "\${FAKE_ARCH:-x86_64}"
    exit 0
fi
exec "$REAL_UNAME" "\$@"
STUB
chmod 0755 "$FAKE_BIN/uname"

# curl stub: serves files out of the fixture directory over a pretend HTTPS
# endpoint, and reports a 404-style failure for anything not published.
cat >"$FAKE_BIN/curl" <<STUB
#!/bin/sh
output=""
url=""
while [ \$# -gt 0 ]; do
    case "\$1" in
        --output) output="\$2"; shift 2 ;;
        --) shift ;;
        https://*) url="\$1"; shift ;;
        *) shift ;;
    esac
done
[ -n "\$url" ] || exit 2
name="\${url##*/}"
source="$FIXTURES/\$name"
if [ ! -f "\$source" ]; then
    echo "curl: (22) The requested URL returned error: 404" >&2
    exit 22
fi
if [ -n "\$output" ]; then
    cat "\$source" > "\$output"
else
    cat "\$source"
fi
STUB
chmod 0755 "$FAKE_BIN/curl"

# These must never be reached by a DESTDIR run. If one is, it fails loudly.
for forbidden in systemctl useradd groupadd userdel groupdel; do
    cat >"$FAKE_BIN/$forbidden" <<STUB
#!/bin/sh
echo "TEST FAILURE: '$forbidden' was invoked with: \$*" >&2
exit 97
STUB
    chmod 0755 "$FAKE_BIN/$forbidden"
done

cat >"$FAKE_BIN/getent" <<'STUB'
#!/bin/sh
echo "TEST FAILURE: 'getent' was invoked with: $*" >&2
exit 97
STUB
chmod 0755 "$FAKE_BIN/getent"

PATH="$FAKE_BIN:$PATH"
export PATH

# A complete, valid release payload directory.
PAYLOAD="$WORK/payload/quickfs-server-linux-x86_64"
mkdir -p "$PAYLOAD"
cp "$WORK/stub-daemon" "$PAYLOAD/quickfs-server-daemon"
cp "$PACKAGING/systemd/quickfs-server.service" "$PAYLOAD/quickfs-server.service"
cp "$PACKAGING/quickfs.toml.example" "$PAYLOAD/quickfs.toml.example"
cp "$INSTALL_SH" "$PAYLOAD/install.sh"
cp "$UNINSTALL_SH" "$PAYLOAD/uninstall.sh"
cp "$PACKAGING/README-install.md" "$PAYLOAD/README-install.md"
cp "$ROOT/LICENSE" "$PAYLOAD/LICENSE"
printf '%s\n' "$STUB_VERSION" >"$PAYLOAD/VERSION"
chmod 0755 "$PAYLOAD/install.sh" "$PAYLOAD/uninstall.sh" "$PAYLOAD/quickfs-server-daemon"

GOOD_ARCHIVE="$FIXTURES/quickfs-server-linux-x86_64.tar.gz"
(cd "$WORK/payload" && tar --sort=name --owner=0 --group=0 --numeric-owner \
    -czf "$GOOD_ARCHIVE" quickfs-server-linux-x86_64)
(cd "$FIXTURES" && sha256sum quickfs-server-linux-x86_64.tar.gz \
    >quickfs-server-linux-x86_64.tar.gz.sha256)

# ---------------------------------------------------------------------------
# 1. A fresh DESTDIR install produces the expected layout and modes
# ---------------------------------------------------------------------------

start "fresh DESTDIR install creates the expected layout and modes"
DEST1="$WORK/dest1"
if DESTDIR="$DEST1" "$PAYLOAD/install.sh" --no-interactive >"$WORK/install1.log" 2>&1; then
    result=0
    assert_file "$DEST1/usr/local/bin/quickfs-server-daemon" || result=1
    assert_file "$DEST1/etc/quickfs/quickfs.toml" || result=1
    assert_file "$DEST1/etc/quickfs/quickfs.toml.example" || result=1
    assert_file "$DEST1/etc/systemd/system/quickfs-server.service" || result=1
    if [ ! -d "$DEST1/var/lib/quickfs" ]; then
        bad "expected state directory"
        result=1
    fi
    assert_mode "$DEST1/usr/local/bin/quickfs-server-daemon" 755 || result=1
    assert_mode "$DEST1/etc/quickfs" 750 || result=1
    assert_mode "$DEST1/etc/quickfs/quickfs.toml" 640 || result=1
    assert_mode "$DEST1/etc/quickfs/quickfs.toml.example" 640 || result=1
    assert_mode "$DEST1/var/lib/quickfs" 700 || result=1
    assert_mode "$DEST1/etc/systemd/system/quickfs-server.service" 644 || result=1
    ok_if "$result"
else
    bad "installer failed: $(tail -n 3 "$WORK/install1.log")"
fi

# ---------------------------------------------------------------------------
# 2. The installed binary reports the expected version
# ---------------------------------------------------------------------------

start "installed binary reports the expected version"
if "$DEST1/usr/local/bin/quickfs-server-daemon" --version | grep -q "$STUB_VERSION"; then
    ok
else
    bad "installed binary did not report $STUB_VERSION"
fi

# ---------------------------------------------------------------------------
# 3. Reinstalling preserves an edited configuration and existing state
# ---------------------------------------------------------------------------

start "reinstall preserves edited configuration and state"
printf '\n# operator edit that must survive\n' >>"$DEST1/etc/quickfs/quickfs.toml"
printf 'pretend server identity\n' >"$DEST1/var/lib/quickfs/server.crt"
printf 'pretend accounts\n' >"$DEST1/var/lib/quickfs/users.json"
before_config="$(cat "$DEST1/etc/quickfs/quickfs.toml")"
if DESTDIR="$DEST1" "$PAYLOAD/install.sh" --no-interactive >"$WORK/install2.log" 2>&1; then
    result=0
    if [ "$(cat "$DEST1/etc/quickfs/quickfs.toml")" != "$before_config" ]; then
        bad "the edited configuration was overwritten"
        result=1
    fi
    if [ "$(cat "$DEST1/var/lib/quickfs/server.crt")" != "pretend server identity" ]; then
        bad "the server identity was overwritten"
        result=1
    fi
    if [ "$(cat "$DEST1/var/lib/quickfs/users.json")" != "pretend accounts" ]; then
        bad "the account database was overwritten"
        result=1
    fi
    ok_if "$result"
else
    bad "reinstall failed: $(tail -n 3 "$WORK/install2.log")"
fi

# ---------------------------------------------------------------------------
# 4/5. Uninstall removes only program files and preserves configuration/state
# ---------------------------------------------------------------------------

start "uninstall removes only program and unit files"
if DESTDIR="$DEST1" "$PAYLOAD/uninstall.sh" >"$WORK/uninstall1.log" 2>&1; then
    result=0
    assert_absent "$DEST1/usr/local/bin/quickfs-server-daemon" || result=1
    assert_absent "$DEST1/etc/systemd/system/quickfs-server.service" || result=1
    ok_if "$result"
else
    bad "uninstall failed: $(tail -n 3 "$WORK/uninstall1.log")"
fi

start "uninstall preserves configuration and state"
result=0
assert_file "$DEST1/etc/quickfs/quickfs.toml" || result=1
assert_file "$DEST1/etc/quickfs/quickfs.toml.example" || result=1
assert_file "$DEST1/var/lib/quickfs/server.crt" || result=1
assert_file "$DEST1/var/lib/quickfs/users.json" || result=1
if ! grep -q "operator edit that must survive" "$DEST1/etc/quickfs/quickfs.toml"; then
    bad "the edited configuration did not survive uninstall"
    result=1
fi
ok_if "$result"

# ---------------------------------------------------------------------------
# 6. Uninstall never follows a symlink at an installed path
# ---------------------------------------------------------------------------

start "uninstall never follows symlinks"
if [ "$SYMLINKS_SUPPORTED" = "1" ]; then
DEST2="$WORK/dest2"
DESTDIR="$DEST2" "$PAYLOAD/install.sh" --no-interactive >/dev/null 2>&1
decoy="$WORK/decoy-target"
printf 'this file must not be deleted\n' >"$decoy"
rm -f "$DEST2/usr/local/bin/quickfs-server-daemon"
ln -s "$decoy" "$DEST2/usr/local/bin/quickfs-server-daemon"
DESTDIR="$DEST2" "$PAYLOAD/uninstall.sh" >"$WORK/uninstall2.log" 2>&1
if [ -f "$decoy" ] && [ -L "$DEST2/usr/local/bin/quickfs-server-daemon" ]; then
    ok
else
    bad "a symlinked install path was followed or removed"
fi
else
    skip "this filesystem cannot create symbolic links"
fi

# ---------------------------------------------------------------------------
# 7. Unsupported architectures fail
# ---------------------------------------------------------------------------

start "unsupported architectures fail"
LONE="$WORK/lone"
mkdir -p "$LONE"
cp "$INSTALL_SH" "$LONE/install.sh"
chmod 0755 "$LONE/install.sh"
if expect_failure "$WORK/arch.log" env FAKE_ARCH=riscv64 DESTDIR="$WORK/dest-arch" \
        "$LONE/install.sh" --no-interactive; then
    if grep -q "unsupported architecture" "$WORK/arch.log"; then
        ok
    else
        bad "failed, but not with an architecture message: $(tail -n 2 "$WORK/arch.log")"
    fi
else
    bad "an unsupported architecture was accepted"
fi

start "architecture aliases map to the published names"
result=0
for alias in amd64 arm64; do
    log="$WORK/alias-$alias.log"
    # No fixture is published for aarch64, so amd64 must reach the x86_64 asset
    # and succeed while arm64 must reach the aarch64 asset and 404.
    if [ "$alias" = "amd64" ]; then
        if ! env FAKE_ARCH="$alias" DESTDIR="$WORK/dest-$alias" \
                "$LONE/install.sh" --no-interactive >"$log" 2>&1; then
            bad "amd64 did not map to the x86_64 asset: $(tail -n 2 "$log")"
            result=1
        fi
    else
        if env FAKE_ARCH="$alias" DESTDIR="$WORK/dest-$alias" \
                "$LONE/install.sh" --no-interactive >"$log" 2>&1; then
            bad "arm64 unexpectedly succeeded with no aarch64 fixture"
            result=1
        elif ! grep -q "aarch64" "$log"; then
            bad "arm64 did not request the aarch64 asset"
            result=1
        fi
    fi
done
ok_if "$result"

# ---------------------------------------------------------------------------
# 8. A checksum mismatch fails before extraction
# ---------------------------------------------------------------------------

start "checksum mismatch fails before extraction"
BAD_FIXTURES="$WORK/fixtures-badsum"
mkdir -p "$BAD_FIXTURES"
cp "$GOOD_ARCHIVE" "$BAD_FIXTURES/"
printf '%s  quickfs-server-linux-x86_64.tar.gz\n' \
    "0000000000000000000000000000000000000000000000000000000000000000" \
    >"$BAD_FIXTURES/quickfs-server-linux-x86_64.tar.gz.sha256"
# Swap the fixture directory the curl stub serves by pointing it at the bad set.
sed "s|$FIXTURES|$BAD_FIXTURES|" "$FAKE_BIN/curl" >"$WORK/curl-badsum"
chmod 0755 "$WORK/curl-badsum"
BADSUM_BIN="$WORK/badsum-bin"
mkdir -p "$BADSUM_BIN"
cp "$WORK/curl-badsum" "$BADSUM_BIN/curl"
cp "$FAKE_BIN/uname" "$BADSUM_BIN/uname"
DEST_BADSUM="$WORK/dest-badsum"
if expect_failure "$WORK/badsum.log" env PATH="$BADSUM_BIN:$PATH" DESTDIR="$DEST_BADSUM" \
        "$LONE/install.sh" --no-interactive; then
    result=0
    if ! grep -q "checksum mismatch" "$WORK/badsum.log"; then
        bad "failed, but not on the checksum: $(tail -n 2 "$WORK/badsum.log")"
        result=1
    fi
    if [ -e "$DEST_BADSUM/usr/local/bin/quickfs-server-daemon" ]; then
        bad "something was installed despite the checksum mismatch"
        result=1
    fi
    ok_if "$result"
else
    bad "a mismatched checksum was accepted"
fi

# ---------------------------------------------------------------------------
# 9. Empty and malformed downloads fail closed
# ---------------------------------------------------------------------------

start "empty or malformed download fails closed"
result=0
EMPTY_FIXTURES="$WORK/fixtures-empty"
mkdir -p "$EMPTY_FIXTURES"
: >"$EMPTY_FIXTURES/quickfs-server-linux-x86_64.tar.gz"
: >"$EMPTY_FIXTURES/quickfs-server-linux-x86_64.tar.gz.sha256"
EMPTY_BIN="$WORK/empty-bin"
mkdir -p "$EMPTY_BIN"
sed "s|$FIXTURES|$EMPTY_FIXTURES|" "$FAKE_BIN/curl" >"$EMPTY_BIN/curl"
chmod 0755 "$EMPTY_BIN/curl"
cp "$FAKE_BIN/uname" "$EMPTY_BIN/uname"
if ! expect_failure "$WORK/empty.log" env PATH="$EMPTY_BIN:$PATH" \
        DESTDIR="$WORK/dest-empty" "$LONE/install.sh" --no-interactive; then
    bad "an empty download was accepted"
    result=1
fi

GARBAGE_FIXTURES="$WORK/fixtures-garbage"
mkdir -p "$GARBAGE_FIXTURES"
printf 'this is not a gzip archive at all\n' \
    >"$GARBAGE_FIXTURES/quickfs-server-linux-x86_64.tar.gz"
(cd "$GARBAGE_FIXTURES" && sha256sum quickfs-server-linux-x86_64.tar.gz \
    >quickfs-server-linux-x86_64.tar.gz.sha256)
GARBAGE_BIN="$WORK/garbage-bin"
mkdir -p "$GARBAGE_BIN"
sed "s|$FIXTURES|$GARBAGE_FIXTURES|" "$FAKE_BIN/curl" >"$GARBAGE_BIN/curl"
chmod 0755 "$GARBAGE_BIN/curl"
cp "$FAKE_BIN/uname" "$GARBAGE_BIN/uname"
if ! expect_failure "$WORK/garbage.log" env PATH="$GARBAGE_BIN:$PATH" \
        DESTDIR="$WORK/dest-garbage" "$LONE/install.sh" --no-interactive; then
    bad "a malformed archive with a matching checksum was accepted"
    result=1
fi

MISSING_BIN="$WORK/missing-bin"
mkdir -p "$MISSING_BIN"
MISSING_FIXTURES="$WORK/fixtures-missing"
mkdir -p "$MISSING_FIXTURES"
sed "s|$FIXTURES|$MISSING_FIXTURES|" "$FAKE_BIN/curl" >"$MISSING_BIN/curl"
chmod 0755 "$MISSING_BIN/curl"
cp "$FAKE_BIN/uname" "$MISSING_BIN/uname"
if ! expect_failure "$WORK/missing.log" env PATH="$MISSING_BIN:$PATH" \
        DESTDIR="$WORK/dest-missing" "$LONE/install.sh" --no-interactive; then
    bad "a missing release asset was accepted"
    result=1
fi
ok_if "$result"

# ---------------------------------------------------------------------------
# 10. Malicious archive layouts are rejected
# ---------------------------------------------------------------------------

make_hostile_archive() {
    label="$1"
    build_dir="$WORK/hostile-$label"
    rm -r -- "$build_dir" 2>/dev/null || true
    mkdir -p "$build_dir/quickfs-server-linux-x86_64"
    cp -R "$PAYLOAD/." "$build_dir/quickfs-server-linux-x86_64/"
    printf '%s\n' "$build_dir"
}

start "malicious archive paths are rejected"
result=0

# An absolute member path.
abs_dir="$(make_hostile_archive absolute)"
abs_archive="$WORK/hostile-absolute.tar.gz"
printf 'payload\n' >"$WORK/absolute-payload"
(cd "$abs_dir" && tar -czf "$abs_archive" -P quickfs-server-linux-x86_64 "$WORK/absolute-payload")
if ! expect_failure "$WORK/hostile-absolute.log" \
        env DESTDIR="$WORK/dest-h1" "$LONE/install.sh" --no-interactive --archive "$abs_archive"; then
    bad "an archive with an absolute member path was accepted"
    result=1
elif ! grep -qi "absolute path" "$WORK/hostile-absolute.log"; then
    bad "absolute member rejected for the wrong reason: $(tail -n 2 "$WORK/hostile-absolute.log")"
    result=1
fi

# A member escaping the extraction directory.
dot_dir="$(make_hostile_archive dotdot)"
dot_archive="$WORK/hostile-dotdot.tar.gz"
printf 'payload\n' >"$dot_dir/escape"
(cd "$dot_dir" && tar -czf "$dot_archive" quickfs-server-linux-x86_64 "quickfs-server-linux-x86_64/../escape" 2>/dev/null)
if ! expect_failure "$WORK/hostile-dotdot.log" \
        env DESTDIR="$WORK/dest-h2" "$LONE/install.sh" --no-interactive --archive "$dot_archive"; then
    bad "an archive with a '..' member was accepted"
    result=1
elif ! grep -qi "escapes the extraction directory" "$WORK/hostile-dotdot.log"; then
    bad "'..' member rejected for the wrong reason: $(tail -n 2 "$WORK/hostile-dotdot.log")"
    result=1
fi
ok_if "$result"

start "archive symlinks, hardlinks and special files are rejected"
result=0

if [ "$SYMLINKS_SUPPORTED" = "1" ]; then
    link_dir="$(make_hostile_archive symlink)"
    ln -s /etc/shadow "$link_dir/quickfs-server-linux-x86_64/secrets"
    link_archive="$WORK/hostile-symlink.tar.gz"
    (cd "$link_dir" && tar -czf "$link_archive" quickfs-server-linux-x86_64)
    if ! expect_failure "$WORK/hostile-symlink.log" \
            env DESTDIR="$WORK/dest-h3" "$LONE/install.sh" --no-interactive --archive "$link_archive"; then
        bad "an archive containing a symbolic link was accepted"
        result=1
    elif ! grep -qi "symbolic link" "$WORK/hostile-symlink.log"; then
        bad "symlink rejected for the wrong reason: $(tail -n 2 "$WORK/hostile-symlink.log")"
        result=1
    fi
fi

hard_dir="$(make_hostile_archive hardlink)"
ln "$hard_dir/quickfs-server-linux-x86_64/LICENSE" "$hard_dir/quickfs-server-linux-x86_64/LICENSE.hard"
hard_archive="$WORK/hostile-hardlink.tar.gz"
(cd "$hard_dir" && tar -czf "$hard_archive" quickfs-server-linux-x86_64)
if ! expect_failure "$WORK/hostile-hardlink.log" \
        env DESTDIR="$WORK/dest-h4" "$LONE/install.sh" --no-interactive --archive "$hard_archive"; then
    bad "an archive containing a hard link was accepted"
    result=1
fi

if command -v mkfifo >/dev/null 2>&1; then
    fifo_dir="$(make_hostile_archive fifo)"
    mkfifo "$fifo_dir/quickfs-server-linux-x86_64/pipe"
    fifo_archive="$WORK/hostile-fifo.tar.gz"
    (cd "$fifo_dir" && tar -czf "$fifo_archive" quickfs-server-linux-x86_64)
    if ! expect_failure "$WORK/hostile-fifo.log" \
            env DESTDIR="$WORK/dest-h5" "$LONE/install.sh" --no-interactive --archive "$fifo_archive"; then
        bad "an archive containing a FIFO was accepted"
        result=1
    elif ! grep -qi "FIFO" "$WORK/hostile-fifo.log"; then
        bad "FIFO rejected for the wrong reason: $(tail -n 2 "$WORK/hostile-fifo.log")"
        result=1
    fi
fi
ok_if "$result"

start "an archive missing an expected member is rejected"
short_dir="$(make_hostile_archive short)"
rm -f "$short_dir/quickfs-server-linux-x86_64/quickfs.toml.example"
short_archive="$WORK/hostile-short.tar.gz"
(cd "$short_dir" && tar -czf "$short_archive" quickfs-server-linux-x86_64)
if expect_failure "$WORK/hostile-short.log" \
        env DESTDIR="$WORK/dest-h6" "$LONE/install.sh" --no-interactive --archive "$short_archive"; then
    ok
else
    bad "an incomplete archive was accepted"
fi

# ---------------------------------------------------------------------------
# 11. Installing from a verified local archive works end to end
# ---------------------------------------------------------------------------

start "a local archive with a matching checksum installs"
LOCAL="$WORK/local"
mkdir -p "$LOCAL"
cp "$GOOD_ARCHIVE" "$LOCAL/"
cp "$FIXTURES/quickfs-server-linux-x86_64.tar.gz.sha256" "$LOCAL/"
DEST_LOCAL="$WORK/dest-local"
if DESTDIR="$DEST_LOCAL" "$LONE/install.sh" --no-interactive \
        --archive "$LOCAL/quickfs-server-linux-x86_64.tar.gz" >"$WORK/local.log" 2>&1; then
    if [ -x "$DEST_LOCAL/usr/local/bin/quickfs-server-daemon" ]; then
        ok
    else
        bad "the binary was not installed from the local archive"
    fi
else
    bad "installing from a local archive failed: $(tail -n 3 "$WORK/local.log")"
fi

# ---------------------------------------------------------------------------
# 12. Cleanup targets only the installer's own temporary directory
# ---------------------------------------------------------------------------

start "installer cleanup targets only its own temporary directory"
PRIVATE_TMP="$WORK/private-tmp"
mkdir -p "$PRIVATE_TMP"
printf 'unrelated\n' >"$PRIVATE_TMP/unrelated-file"
mkdir -p "$PRIVATE_TMP/unrelated-dir"
printf 'nested\n' >"$PRIVATE_TMP/unrelated-dir/nested"
# A run that fails partway through still has to clean up after itself.
env TMPDIR="$PRIVATE_TMP" FAKE_ARCH=riscv64 DESTDIR="$WORK/dest-cleanup" \
    "$LONE/install.sh" --no-interactive >/dev/null 2>&1 || true
env TMPDIR="$PRIVATE_TMP" DESTDIR="$WORK/dest-cleanup" \
    "$LONE/install.sh" --no-interactive >/dev/null 2>&1 || true
result=0
if [ ! -f "$PRIVATE_TMP/unrelated-file" ] || [ ! -f "$PRIVATE_TMP/unrelated-dir/nested" ]; then
    bad "unrelated temporary files were removed"
    result=1
fi
leftovers="$(find "$PRIVATE_TMP" -maxdepth 1 -name 'quickfs-install.*' | wc -l)"
if [ "$leftovers" != "0" ]; then
    bad "the installer left $leftovers temporary directories behind"
    result=1
fi
ok_if "$result"

# ---------------------------------------------------------------------------
# 13. Static guarantees about the scripts themselves
# ---------------------------------------------------------------------------

# Both scripts talk about what they refuse to do, in comments and in the
# guidance the uninstaller prints. Those are documentation, not behaviour, so
# the static checks below run against executable lines only: comments and
# here-document bodies are stripped first.
cat >"$WORK/code-only.awk" <<'AWK'
BEGIN { heredoc = "" }
heredoc != "" {
    line = $0
    sub(/^[ \t]+/, "", line)
    sub(/[ \t]+$/, "", line)
    if (line == heredoc) { heredoc = "" }
    next
}
{
    if (match($0, /<<-?[ \t]*'?[A-Za-z_][A-Za-z0-9_]*'?/)) {
        tag = substr($0, RSTART, RLENGTH)
        sub(/^<<-?[ \t]*/, "", tag)
        gsub(/'/, "", tag)
        heredoc = tag
    }
    stripped = $0
    sub(/^[ \t]+/, "", stripped)
    if (stripped ~ /^#/) next
    print $0
}
AWK

code_only() {
    awk -f "$WORK/code-only.awk" "$1"
}

start "no recursive delete can target an export or a broad system path"
result=0
for script in "$INSTALL_SH" "$UNINSTALL_SH"; do
    code_only "$script" >"$WORK/code.txt"
    if grep -n 'rm -rf' "$WORK/code.txt" >/dev/null 2>&1; then
        bad "$script uses 'rm -rf'"
        result=1
    fi
    if grep -nE 'chown[^|]*-R|chmod[^|]*-R' "$WORK/code.txt" >/dev/null 2>&1; then
        bad "$script uses a recursive chown or chmod"
        result=1
    fi
    if grep -nE '(^|[^_[:alnum:]])eval[[:space:]]' "$WORK/code.txt" >/dev/null 2>&1; then
        bad "$script uses eval"
        result=1
    fi
    # Deletions must name one file, never a glob or an unbounded variable.
    if grep -nE 'rm[[:space:]]+[^|;&]*\*' "$WORK/code.txt" >/dev/null 2>&1; then
        bad "$script deletes through a glob"
        result=1
    fi
done

code_only "$UNINSTALL_SH" >"$WORK/uninstall-code.txt"
if grep -nE 'rm[[:space:]]+-[a-zA-Z]*r' "$WORK/uninstall-code.txt" >/dev/null 2>&1; then
    bad "the uninstaller performs a recursive delete"
    result=1
fi
if grep -nE 'export_root|EXPORT_ROOT' "$WORK/uninstall-code.txt" >/dev/null 2>&1; then
    bad "the uninstaller reads the export root"
    result=1
fi
# The only recursive delete anywhere is the installer's own mktemp -d cleanup.
code_only "$INSTALL_SH" >"$WORK/install-code.txt"
recursive="$(grep -cE 'rm[[:space:]]+-r' "$WORK/install-code.txt" || true)"
if [ "$recursive" != "1" ]; then
    bad "the installer has $recursive recursive deletes; exactly one (its own temporary directory) is expected"
    result=1
fi
# The literal dollar sign is the point: this asserts on the script's text.
# shellcheck disable=SC2016
if ! grep -qE 'rm -r -- "\$TMPDIR_CREATED"' "$WORK/install-code.txt"; then
    bad "the installer's only recursive delete does not target its own temporary directory"
    result=1
fi
ok_if "$result"

start "the uninstaller never sources or interprets the TOML configuration"
result=0
if grep -nE '^[[:space:]]*(\.|source)[[:space:]]' "$WORK/uninstall-code.txt" >/dev/null 2>&1; then
    bad "the uninstaller sources a file"
    result=1
fi
if grep -nE 'quickfs\.toml' "$WORK/uninstall-code.txt" >/dev/null 2>&1; then
    bad "the uninstaller reads or writes quickfs.toml"
    result=1
fi
ok_if "$result"

# ---------------------------------------------------------------------------
# 14. External linters, when they are available
# ---------------------------------------------------------------------------

start "shell scripts pass ShellCheck"
if command -v shellcheck >/dev/null 2>&1; then
    if shellcheck -s sh "$INSTALL_SH" "$UNINSTALL_SH" "$0" >"$WORK/shellcheck.log" 2>&1; then
        ok
    else
        bad "ShellCheck reported issues: $(head -n 20 "$WORK/shellcheck.log")"
    fi
else
    skip "shellcheck is not installed"
fi

start "the systemd unit passes systemd-analyze verify"
if command -v systemd-analyze >/dev/null 2>&1; then
    # `verify` resolves User=, Group= and the ExecStart program. This suite must
    # not create a system account or write to /usr/local/bin, so it verifies a
    # copy whose two program paths point at the stub daemon and whose identity
    # is the account running the tests. That checks every directive, spelling
    # and value in the unit; that the real ExecStart path is the one the
    # installer writes is asserted by the layout cases above.
    UNIT_COPY="$WORK/quickfs-server.service"
    TEST_USER="$(id -un)"
    TEST_GROUP="$(id -gn)"
    sed -e "s|/usr/local/bin/quickfs-server-daemon|$PAYLOAD/quickfs-server-daemon|g" \
        -e "s|^User=quickfs$|User=$TEST_USER|" \
        -e "s|^Group=quickfs$|Group=$TEST_GROUP|" \
        "$PACKAGING/systemd/quickfs-server.service" >"$UNIT_COPY"
    if systemd-analyze verify "$UNIT_COPY" >"$WORK/analyze.log" 2>&1; then
        ok
    else
        bad "systemd-analyze reported issues: $(head -n 20 "$WORK/analyze.log")"
    fi
else
    skip "systemd-analyze is not installed"
fi

# ---------------------------------------------------------------------------
# 15. The release archive holds exactly the intended files
# ---------------------------------------------------------------------------

start "release archive includes exactly the intended files"
expected="LICENSE README-install.md VERSION install.sh quickfs-server-daemon quickfs-server.service quickfs.toml.example uninstall.sh"
actual="$(tar -tzf "$GOOD_ARCHIVE" \
    | sed 's|^quickfs-server-linux-x86_64/||' \
    | grep -v '^$' \
    | sort \
    | tr '\n' ' ' \
    | sed 's/ $//')"
normalized_expected="$(printf '%s' "$expected" | tr ' ' '\n' | sort | tr '\n' ' ' | sed 's/ $//')"
if [ "$actual" = "$normalized_expected" ]; then
    ok
else
    bad "archive contains '$actual', expected '$normalized_expected'"
fi

start "the example configuration contains no secret-bearing field"
if grep -nEi '^[[:space:]]*(password|passphrase|secret|pairing_code|private_key|token)[[:space:]]*=' \
        "$PACKAGING/quickfs.toml.example" >/dev/null 2>&1; then
    bad "the example configuration declares a secret-bearing field"
else
    ok
fi

start "writes are disabled in the example configuration"
if grep -q '^allow_writes = false' "$PACKAGING/quickfs.toml.example"; then
    ok
else
    bad "allow_writes is not false in the example configuration"
fi

# ---------------------------------------------------------------------------
# Result
# ---------------------------------------------------------------------------

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" = "0" ]
