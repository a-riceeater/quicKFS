#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# quicKFS Linux server installer.
#
# Download it, read it, then run it. There is deliberately no
# "curl ... | sh" recipe anywhere in the documentation: a pipe hands root to
# whatever the server returned, with no chance to look at it first.
#
#   sudo ./install.sh
#   sudo ./install.sh --version v0.2.0
#   sudo ./install.sh --no-start
#   sudo ./install.sh --archive ./quickfs-server-linux-x86_64.tar.gz
#   DESTDIR=/tmp/quickfs-package ./install.sh --no-systemd
#
# DESTDIR stages the file layout into a directory without touching the real
# system: no service user is created, systemd is never contacted, and nothing
# under /etc, /usr or /var is written.

set -eu

REPO="quickfs/quickfs"
PROGRAM="quickfs-server-daemon"
SERVICE="quickfs-server.service"
SERVICE_NAME="quickfs-server"
SERVICE_USER="quickfs"
SERVICE_GROUP="quickfs"

BIN_DIR="/usr/local/bin"
CONFIG_DIR="/etc/quickfs"
STATE_DIR="/var/lib/quickfs"
UNIT_DIR="/etc/systemd/system"
CONFIG_FILE="quickfs.toml"
CONFIG_EXAMPLE="quickfs.toml.example"

# Exactly the members a release archive may contain. An archive holding
# anything else is rejected rather than partially trusted.
EXPECTED_MEMBERS="quickfs-server-daemon quickfs-server.service quickfs.toml.example install.sh uninstall.sh README-install.md LICENSE VERSION"

DESTDIR="${DESTDIR:-}"
VERSION=""
ARCHIVE=""
NO_START=0
NO_SYSTEMD=0
NO_INTERACTIVE=0

TMPDIR_CREATED=""
TMP_PARENT=""

# ---------------------------------------------------------------------------
# Output and cleanup
# ---------------------------------------------------------------------------

log() { printf '%s\n' "$*"; }
step() { printf '==> %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
fail() { printf 'error: %s\n' "$*" >&2; exit 1; }

# Remove only the directory this process created with mktemp -d, and only if it
# still looks like that directory. Never a variable-derived path from anywhere
# else, never a glob, never "rm -rf".
cleanup() {
    status=$?
    if [ -n "$TMPDIR_CREATED" ]; then
        case "$TMPDIR_CREATED" in
            "$TMP_PARENT"/quickfs-install.*)
                if [ -d "$TMPDIR_CREATED" ]; then
                    # -r is needed for the extracted tree; the target is a
                    # mktemp -d path under the expected parent, verified above,
                    # and never a user-supplied value.
                    rm -r -- "$TMPDIR_CREATED" 2>/dev/null || true
                fi
                ;;
            *)
                warn "refusing to remove unexpected temporary path '$TMPDIR_CREATED'"
                ;;
        esac
    fi
    exit "$status"
}

make_temp_dir() {
    TMP_PARENT="${TMPDIR:-/tmp}"
    case "$TMP_PARENT" in
        /*) ;;
        *) fail "TMPDIR must be an absolute path" ;;
    esac
    [ -d "$TMP_PARENT" ] || fail "temporary directory '$TMP_PARENT' does not exist"
    TMPDIR_CREATED="$(mktemp -d "$TMP_PARENT/quickfs-install.XXXXXXXXXX")"
    [ -n "$TMPDIR_CREATED" ] || fail "failed to create a temporary directory"
    [ -d "$TMPDIR_CREATED" ] || fail "failed to create a temporary directory"
    trap cleanup EXIT HUP INT TERM
}

usage() {
    cat <<'USAGE'
Usage: install.sh [OPTIONS]

  --version <TAG>     Install this release tag instead of the latest.
  --archive <PATH>    Install from a local release archive; nothing is downloaded.
  --repo <OWNER/NAME> Download from another GitHub repository.
  --no-start          Install and configure, but do not enable or start the service.
  --no-systemd        Do not install the unit or contact systemd at all.
  --no-interactive    Never prompt; install files and print the next steps.
  -h, --help          Show this message.

Environment:
  DESTDIR   Stage the layout under this directory instead of the real system.
            Implies --no-systemd behaviour for daemon-reload and never creates
            a service user.
USAGE
}

# ---------------------------------------------------------------------------
# Arguments
# ---------------------------------------------------------------------------

while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            [ $# -ge 2 ] || fail "--version needs a value"
            VERSION="$2"
            shift 2
            ;;
        --version=*)
            VERSION="${1#--version=}"
            shift
            ;;
        --archive)
            [ $# -ge 2 ] || fail "--archive needs a value"
            ARCHIVE="$2"
            shift 2
            ;;
        --archive=*)
            ARCHIVE="${1#--archive=}"
            shift
            ;;
        --repo)
            [ $# -ge 2 ] || fail "--repo needs a value"
            REPO="$2"
            shift 2
            ;;
        --repo=*)
            REPO="${1#--repo=}"
            shift
            ;;
        --no-start) NO_START=1; shift ;;
        --no-systemd) NO_SYSTEMD=1; shift ;;
        --no-interactive) NO_INTERACTIVE=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; fail "unknown option '$1'" ;;
    esac
done

case "$VERSION" in
    ''|v[0-9]*|[0-9]*) ;;
    *) fail "--version must be a release tag such as v0.2.0" ;;
esac
case "$REPO" in
    */*) ;;
    *) fail "--repo must be OWNER/NAME" ;;
esac
case "$REPO" in
    *[!A-Za-z0-9._/-]*) fail "--repo contains an unexpected character" ;;
esac

# ---------------------------------------------------------------------------
# Mode
# ---------------------------------------------------------------------------

if [ -n "$DESTDIR" ]; then
    case "$DESTDIR" in
        /*) ;;
        *) fail "DESTDIR must be an absolute path" ;;
    esac
    REAL_INSTALL=0
else
    REAL_INSTALL=1
    [ "$(id -u)" = "0" ] || fail "run this installer as root, or set DESTDIR to stage the layout"
fi

if [ "$REAL_INSTALL" = "0" ]; then
    # The unit file is still staged into DESTDIR so a package build can see the
    # complete layout, but systemd itself is never contacted and no real user
    # or group is created.
    NO_START=1
    NO_INTERACTIVE=1
fi

# Absolute destinations, resolved here and never taken from archive contents.
DEST_BIN_DIR="$DESTDIR$BIN_DIR"
DEST_CONFIG_DIR="$DESTDIR$CONFIG_DIR"
DEST_STATE_DIR="$DESTDIR$STATE_DIR"
DEST_UNIT_DIR="$DESTDIR$UNIT_DIR"
DEST_BINARY="$DEST_BIN_DIR/$PROGRAM"
DEST_CONFIG="$DEST_CONFIG_DIR/$CONFIG_FILE"
DEST_EXAMPLE="$DEST_CONFIG_DIR/$CONFIG_EXAMPLE"
DEST_UNIT="$DEST_UNIT_DIR/$SERVICE"

SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"

# ---------------------------------------------------------------------------
# Payload: local directory, local archive, or verified download
# ---------------------------------------------------------------------------

detect_architecture() {
    machine="$(uname -m)"
    case "$machine" in
        x86_64|amd64) echo "x86_64" ;;
        aarch64|arm64) echo "aarch64" ;;
        *) fail "unsupported architecture '$machine'; quicKFS publishes x86_64 and aarch64 Linux server builds only" ;;
    esac
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || fail "required command '$1' was not found"
}

# Download over HTTPS only, refusing a redirect that leaves HTTPS and refusing
# an HTTP error status. TLS verification is never disabled.
download() {
    url="$1"
    output="$2"
    case "$url" in
        https://*) ;;
        *) fail "refusing to download from a non-HTTPS URL" ;;
    esac
    if command -v curl >/dev/null 2>&1; then
        curl --proto '=https' --proto-redir '=https' --tlsv1.2 \
             --fail --silent --show-error --location \
             --retry 2 --retry-connrefused \
             --output "$output" -- "$url"
    elif command -v wget >/dev/null 2>&1; then
        wget --https-only --secure-protocol=TLSv1_2 \
             --output-document="$output" -- "$url"
    else
        fail "neither curl nor wget is available to download the release"
    fi
    [ -s "$output" ] || fail "downloaded file '$url' is empty"
}

# Compute a lowercase hex SHA-256 of a file with whichever tool exists.
sha256_of() {
    file="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -- "$file" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 -- "$file" | cut -d' ' -f1
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$file" | sed 's/.*= *//'
    else
        fail "no SHA-256 tool (sha256sum, shasum, or openssl) is available"
    fi
}

verify_checksum() {
    archive="$1"
    checksum_file="$2"
    expected="$(cut -d' ' -f1 <"$checksum_file" | tr 'A-F' 'a-f' | head -n 1)"
    case "$expected" in
        [0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]*)
            ;;
        *) fail "checksum file does not contain a SHA-256 digest" ;;
    esac
    [ "${#expected}" -eq 64 ] || fail "checksum file does not contain a SHA-256 digest"
    actual="$(sha256_of "$archive" | tr 'A-F' 'a-f')"
    if [ "$actual" != "$expected" ]; then
        fail "checksum mismatch for '$archive'; nothing was extracted or executed"
    fi
    step "SHA-256 verified: $actual"
}

# Reject anything that is not a plain file or directory, any absolute path, and
# any path containing a ".." component, before extraction. tar's own -v listing
# is the source for entry types, so a symlink, hardlink, device, FIFO or socket
# member is refused rather than written.
validate_archive() {
    archive="$1"
    listing="$TMPDIR_CREATED/listing.txt"
    tar -tvzf "$archive" >"$listing" 2>/dev/null || fail "'$archive' is not a readable gzip tar archive"
    [ -s "$listing" ] || fail "'$archive' contains no members"

    while IFS= read -r line; do
        [ -n "$line" ] || continue
        type_char="$(printf '%s' "$line" | cut -c1)"
        case "$type_char" in
            -|d) ;;
            l) fail "archive contains a symbolic link; refusing to extract" ;;
            h) fail "archive contains a hard link; refusing to extract" ;;
            c|b) fail "archive contains a device node; refusing to extract" ;;
            p) fail "archive contains a FIFO; refusing to extract" ;;
            s) fail "archive contains a socket; refusing to extract" ;;
            *) fail "archive contains an unsupported entry type '$type_char'" ;;
        esac
    done <"$listing"

    names="$TMPDIR_CREATED/names.txt"
    tar -tzf "$archive" >"$names" 2>/dev/null || fail "'$archive' is not a readable gzip tar archive"
    while IFS= read -r name; do
        [ -n "$name" ] || continue
        case "$name" in
            /*) fail "archive member '$name' is an absolute path" ;;
            ../*|*/../*|*/..|..) fail "archive member '$name' escapes the extraction directory" ;;
            *'
'*) fail "archive member name contains a newline" ;;
        esac
    done <"$names"
}

# Every expected file must be present after extraction, as a plain file.
validate_payload() {
    directory="$1"
    # Intentional word splitting over the fixed member list.
    # shellcheck disable=SC2086
    for member in $EXPECTED_MEMBERS; do
        path="$directory/$member"
        if [ -L "$path" ]; then
            fail "release payload member '$member' is a symbolic link"
        fi
        if [ ! -f "$path" ]; then
            fail "release payload is missing '$member'"
        fi
    done
    return 0
}

PAYLOAD=""

# Release archives carry one top-level directory (quickfs-server-linux-<arch>/),
# so the extracted files sit one level down. Descend into it when the extraction
# directory holds exactly one directory and nothing else. The name is not
# trusted for anything - it is only followed inside a directory this process
# created with mktemp -d, and the member set is validated afterwards.
descend_single_directory() {
    directory="$1"
    entries=0
    only=""
    for entry in "$directory"/* "$directory"/.[!.]*; do
        [ -e "$entry" ] || continue
        entries=$((entries + 1))
        only="$entry"
    done
    if [ "$entries" = "1" ] && [ -d "$only" ] && [ ! -L "$only" ]; then
        printf '%s\n' "$only"
    else
        printf '%s\n' "$directory"
    fi
}

prepare_payload() {
    make_temp_dir

    if [ -n "$ARCHIVE" ]; then
        [ -f "$ARCHIVE" ] || fail "archive '$ARCHIVE' does not exist"
        if [ -f "$ARCHIVE.sha256" ]; then
            step "Verifying $ARCHIVE against $ARCHIVE.sha256"
            verify_checksum "$ARCHIVE" "$ARCHIVE.sha256"
        else
            warn "no '$ARCHIVE.sha256' beside the archive; its integrity is not verified here"
        fi
        step "Validating archive members"
        validate_archive "$ARCHIVE"
        mkdir -p "$TMPDIR_CREATED/payload"
        tar -xzf "$ARCHIVE" -C "$TMPDIR_CREATED/payload" --no-same-owner --no-same-permissions 2>/dev/null \
            || tar -xzf "$ARCHIVE" -C "$TMPDIR_CREATED/payload"
        PAYLOAD="$(descend_single_directory "$TMPDIR_CREATED/payload")"
    elif [ -f "$SCRIPT_DIR/$PROGRAM" ]; then
        # Running from an already-extracted release archive.
        step "Installing from the payload beside this script"
        PAYLOAD="$SCRIPT_DIR"
    else
        arch="$(detect_architecture)"
        asset="quickfs-server-linux-$arch.tar.gz"
        if [ -n "$VERSION" ]; then
            base="https://github.com/$REPO/releases/download/$VERSION"
        else
            base="https://github.com/$REPO/releases/latest/download"
        fi
        step "Downloading $asset"
        download "$base/$asset" "$TMPDIR_CREATED/$asset"
        step "Downloading $asset.sha256"
        download "$base/$asset.sha256" "$TMPDIR_CREATED/$asset.sha256"
        verify_checksum "$TMPDIR_CREATED/$asset" "$TMPDIR_CREATED/$asset.sha256"
        step "Validating archive members"
        validate_archive "$TMPDIR_CREATED/$asset"
        mkdir -p "$TMPDIR_CREATED/payload"
        tar -xzf "$TMPDIR_CREATED/$asset" -C "$TMPDIR_CREATED/payload" --no-same-owner --no-same-permissions 2>/dev/null \
            || tar -xzf "$TMPDIR_CREATED/$asset" -C "$TMPDIR_CREATED/payload"
        PAYLOAD="$(descend_single_directory "$TMPDIR_CREATED/payload")"
    fi

    if [ "$PAYLOAD" = "$SCRIPT_DIR" ]; then
        # A checkout keeps the unit under packaging/systemd/.
        [ -f "$PAYLOAD/$SERVICE" ] || [ -f "$PAYLOAD/systemd/$SERVICE" ] \
            || fail "cannot find '$SERVICE' beside this script"
        [ -f "$PAYLOAD/$CONFIG_EXAMPLE" ] || fail "cannot find '$CONFIG_EXAMPLE' beside this script"
    else
        validate_payload "$PAYLOAD"
    fi
}

payload_unit() {
    if [ -f "$PAYLOAD/$SERVICE" ]; then
        printf '%s\n' "$PAYLOAD/$SERVICE"
    else
        printf '%s\n' "$PAYLOAD/systemd/$SERVICE"
    fi
}

# ---------------------------------------------------------------------------
# Service identity
# ---------------------------------------------------------------------------

nologin_shell() {
    for candidate in /usr/sbin/nologin /sbin/nologin /usr/bin/false /bin/false; do
        if [ -x "$candidate" ]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    printf '%s\n' "/bin/false"
}

create_service_identity() {
    if [ "$REAL_INSTALL" != "1" ]; then
        return 0
    fi
    if command -v getent >/dev/null 2>&1 && getent group "$SERVICE_GROUP" >/dev/null 2>&1; then
        log "Group '$SERVICE_GROUP' already exists."
    elif command -v groupadd >/dev/null 2>&1; then
        step "Creating system group '$SERVICE_GROUP'"
        groupadd --system "$SERVICE_GROUP"
    else
        fail "groupadd is not available; create the '$SERVICE_GROUP' system group manually"
    fi

    if command -v getent >/dev/null 2>&1 && getent passwd "$SERVICE_USER" >/dev/null 2>&1; then
        log "User '$SERVICE_USER' already exists."
    elif command -v useradd >/dev/null 2>&1; then
        step "Creating system user '$SERVICE_USER'"
        # A system account with no password, no interactive shell, no created
        # home, and no supplementary groups. Its home is the state directory so
        # tools that need one have a defined answer; the directory is created
        # separately with mode 0700.
        useradd --system \
                --gid "$SERVICE_GROUP" \
                --home-dir "$STATE_DIR" \
                --no-create-home \
                --shell "$(nologin_shell)" \
                --comment "quicKFS server daemon" \
                "$SERVICE_USER"
    else
        fail "useradd is not available; create the '$SERVICE_USER' system user manually"
    fi
}

# ---------------------------------------------------------------------------
# File installation
# ---------------------------------------------------------------------------

# Install a file atomically: write a uniquely named temporary file in the target
# directory, then rename it over the destination. A running daemon keeps the
# inode it already opened, and a partially written binary is never reachable.
install_atomic() {
    source_file="$1"
    destination="$2"
    mode="$3"
    directory="$(dirname -- "$destination")"
    staged="$(mktemp "$directory/.quickfs-install.XXXXXXXX")"
    if ! cat -- "$source_file" >"$staged"; then
        rm -f -- "$staged"
        fail "failed to stage '$destination'"
    fi
    chmod "$mode" "$staged"
    if [ "$REAL_INSTALL" = "1" ]; then
        chown root:root "$staged" 2>/dev/null || true
    fi
    mv -f -- "$staged" "$destination"
}

# Create a directory if it is absent, then apply the mode unconditionally so an
# upgrade repairs a loosened one. Split from `install -d -m` because that form
# fails outright on filesystems that cannot represent the mode, which would stop
# an otherwise fine staging run.
make_directory() {
    directory="$1"
    mode="$2"
    [ -d "$directory" ] || mkdir -p -- "$directory"
    chmod "$mode" "$directory"
}

install_files() {
    step "Creating $DEST_BIN_DIR"
    [ -d "$DEST_BIN_DIR" ] || make_directory "$DEST_BIN_DIR" 0755

    step "Installing $BIN_DIR/$PROGRAM"
    install_atomic "$PAYLOAD/$PROGRAM" "$DEST_BINARY" 0755

    step "Creating $CONFIG_DIR (0750)"
    make_directory "$DEST_CONFIG_DIR" 0750

    step "Creating $STATE_DIR (0700)"
    make_directory "$DEST_STATE_DIR" 0700

    if [ "$REAL_INSTALL" = "1" ]; then
        chown "root:$SERVICE_GROUP" "$DEST_CONFIG_DIR"
        chown "$SERVICE_USER:$SERVICE_GROUP" "$DEST_STATE_DIR"
    fi

    step "Installing $CONFIG_DIR/$CONFIG_EXAMPLE"
    install_atomic "$PAYLOAD/$CONFIG_EXAMPLE" "$DEST_EXAMPLE" 0640

    if [ -e "$DEST_CONFIG" ]; then
        log "Keeping the existing $CONFIG_DIR/$CONFIG_FILE unchanged."
    else
        step "Creating $CONFIG_DIR/$CONFIG_FILE from the example"
        install_atomic "$PAYLOAD/$CONFIG_EXAMPLE" "$DEST_CONFIG" 0640
    fi

    if [ "$REAL_INSTALL" = "1" ]; then
        chown "root:$SERVICE_GROUP" "$DEST_EXAMPLE" "$DEST_CONFIG"
    fi

    if [ "$NO_SYSTEMD" = "0" ]; then
        if [ "$REAL_INSTALL" = "1" ] && [ ! -d "$UNIT_DIR" ]; then
            fail "system unit directory '$UNIT_DIR' does not exist"
        fi
        step "Installing $UNIT_DIR/$SERVICE"
        [ -d "$DEST_UNIT_DIR" ] || make_directory "$DEST_UNIT_DIR" 0755
        install_atomic "$(payload_unit)" "$DEST_UNIT" 0644
    fi
}

systemd_present() {
    [ "$REAL_INSTALL" = "1" ] && [ "$NO_SYSTEMD" = "0" ] \
        && [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1
}

# ---------------------------------------------------------------------------
# Interactive setup
# ---------------------------------------------------------------------------

# Values that reach the TOML editor must be free of anything the line-oriented
# rewriter or a TOML basic string cannot carry safely.
valid_path_value() {
    value="$1"
    case "$value" in
        /*) ;;
        *) return 1 ;;
    esac
    case "$value" in
        *'
'*) return 1 ;;
        *'"'*) return 1 ;;
        *\\*) return 1 ;;
        *) ;;
    esac
    return 0
}

valid_server_name() {
    value="$1"
    [ -n "$value" ] || return 1
    [ "${#value}" -le 253 ] || return 1
    case "$value" in
        *[!A-Za-z0-9.:-]*) return 1 ;;
        *) ;;
    esac
    return 0
}

# Replace exactly one known key inside the [server] table. Nothing else in the
# file is touched, and the value is already known to be free of quotes,
# backslashes and newlines.
set_toml_value() {
    file="$1"
    key="$2"
    value="$3"
    staged="$(mktemp "$(dirname -- "$file")/.quickfs-config.XXXXXXXX")"
    KEY="$key" VALUE="$value" awk '
        BEGIN { section = ""; key = ENVIRON["KEY"]; value = ENVIRON["VALUE"]; done = 0 }
        {
            line = $0
            trimmed = line
            sub(/^[ \t]+/, "", trimmed)
            if (trimmed ~ /^\[/) {
                section = trimmed
                sub(/[ \t]+$/, "", section)
            } else if (section == "[server]" && !done) {
                probe = trimmed
                sub(/^#[ \t]*/, "", probe)
                if (probe ~ ("^" key "[ \t]*=")) {
                    if (trimmed ~ /^#/) { print line; next }
                    print key " = \"" value "\""
                    done = 1
                    next
                }
            }
            print line
        }
    ' "$file" >"$staged"
    if ! grep -q "^$key = " "$staged"; then
        rm -f -- "$staged"
        fail "could not set '$key' in '$file'; edit it by hand with sudoedit"
    fi
    chmod 0640 "$staged"
    if [ "$REAL_INSTALL" = "1" ]; then
        chown "root:$SERVICE_GROUP" "$staged" 2>/dev/null || true
    fi
    mv -f -- "$staged" "$file"
}

run_as_service_user() {
    if command -v runuser >/dev/null 2>&1; then
        runuser -u "$SERVICE_USER" -- "$@"
    elif command -v setpriv >/dev/null 2>&1; then
        setpriv --reuid "$SERVICE_USER" --regid "$SERVICE_GROUP" --init-groups -- "$@"
    else
        fail "neither runuser nor setpriv is available; run the command as '$SERVICE_USER' yourself"
    fi
}

ask() {
    prompt="$1"
    default="$2"
    printf '%s' "$prompt" >&2
    IFS= read -r reply || reply=""
    if [ -z "$reply" ]; then
        printf '%s\n' "$default"
    else
        printf '%s\n' "$reply"
    fi
}

confirm() {
    printf '%s [y/N] ' "$1" >&2
    IFS= read -r reply || reply=""
    case "$reply" in
        y|Y|yes|YES) return 0 ;;
        *) return 1 ;;
    esac
}

identity_initialized() {
    [ -f "$DEST_STATE_DIR/server.crt" ]
}

validate_config() {
    "$DEST_BINARY" config validate --config "$DEST_CONFIG"
}

interactive_setup() {
    log ""
    step "Interactive setup"
    log "Writes stay disabled. Nothing here creates a password or a pairing code."
    log ""

    export_root="$(ask "Absolute export root to publish: " "")"
    if ! valid_path_value "$export_root"; then
        warn "not an acceptable absolute path; leaving the configuration for manual editing"
        return 1
    fi
    if [ ! -d "$export_root" ]; then
        if confirm "'$export_root' does not exist. Create it (mode 0755, owned by root)?"; then
            make_directory "$export_root" 0755
        else
            warn "leaving the configuration for manual editing"
            return 1
        fi
    fi
    # Ownership of an administrator's export is never changed here, and never
    # recursively. See docs/setup.md for the four supported ways to grant the
    # service read (and optionally write) access to it.

    server_name="$(ask "Server DNS name or IP clients will use: " "")"
    if ! valid_server_name "$server_name"; then
        warn "not an acceptable server name; leaving the configuration for manual editing"
        return 1
    fi

    step "Updating $CONFIG_DIR/$CONFIG_FILE"
    set_toml_value "$DEST_CONFIG" "export_root" "$export_root"
    set_toml_value "$DEST_CONFIG" "state_dir" "$STATE_DIR"

    step "Validating the configuration"
    validate_config || return 1

    if identity_initialized; then
        log "Server identity already exists in $STATE_DIR; leaving it untouched."
    else
        step "Initializing server identity as '$SERVICE_USER'"
        run_as_service_user "$DEST_BINARY" init \
            --config "$DEST_CONFIG" \
            --server-name "$server_name" || return 1
    fi

    if systemd_present && [ "$NO_START" = "0" ]; then
        if confirm "Enable and start $SERVICE_NAME now?"; then
            systemctl enable --now "$SERVICE_NAME"
            systemctl --no-pager --full status "$SERVICE_NAME" || true
        fi
    fi
    return 0
}

next_steps() {
    cat <<STEPS

Next steps
----------

  1. Review the configuration (export_root, bind, limits):

       sudoedit $CONFIG_DIR/$CONFIG_FILE
       $BIN_DIR/$PROGRAM config validate --config $CONFIG_DIR/$CONFIG_FILE
       $BIN_DIR/$PROGRAM config print-effective --config $CONFIG_DIR/$CONFIG_FILE

  2. Give '$SERVICE_USER' access to the export without changing its ownership
     recursively - make '$SERVICE_GROUP' the owning group of a dedicated export,
     use an export owned by '$SERVICE_USER', set a POSIX ACL, or add
     '$SERVICE_USER' to a storage group you chose deliberately. See docs/setup.md.

  3. Create the server identity once, as the service user:

       sudo -u $SERVICE_USER $BIN_DIR/$PROGRAM init \\
         --config $CONFIG_DIR/$CONFIG_FILE \\
         --server-name files.example.net

  4. Add an account (it prompts twice for a hidden password):

       sudo -u $SERVICE_USER $BIN_DIR/$PROGRAM user add \\
         --config $CONFIG_DIR/$CONFIG_FILE alice

  5. Start the service:

       sudo systemctl enable --now $SERVICE_NAME
       systemctl status $SERVICE_NAME
       journalctl -u $SERVICE_NAME -f

  6. Pair each client with a single-use code, transferred over a trusted channel:

       sudo -u $SERVICE_USER $BIN_DIR/$PROGRAM pair create \\
         --config $CONFIG_DIR/$CONFIG_FILE

  Writes are disabled. To enable them, set allow_writes = true in the
  configuration, restart the service, and grant each account separately with
  '$PROGRAM user grant-write'.

  QUIC is UDP. Allow the configured UDP port through your firewall yourself;
  this installer never changes firewall rules.

STEPS
}

# ---------------------------------------------------------------------------
# Run
# ---------------------------------------------------------------------------

require_command mktemp
require_command tar

prepare_payload
create_service_identity
install_files

if systemd_present; then
    step "Reloading systemd"
    systemctl daemon-reload
fi

log ""
step "Installed"
log "  binary:        $BIN_DIR/$PROGRAM"
log "  configuration: $CONFIG_DIR/$CONFIG_FILE (0640 root:$SERVICE_GROUP)"
log "  example:       $CONFIG_DIR/$CONFIG_EXAMPLE"
log "  state:         $STATE_DIR (0700 $SERVICE_USER:$SERVICE_GROUP)"
if [ "$NO_SYSTEMD" = "0" ]; then
    log "  unit:          $UNIT_DIR/$SERVICE"
fi
if [ "$REAL_INSTALL" = "1" ]; then
    "$DEST_BINARY" --version || true
fi

if [ "$NO_INTERACTIVE" = "0" ] && [ -t 0 ] && [ -t 1 ]; then
    if ! interactive_setup; then
        next_steps
    fi
else
    next_steps
fi
