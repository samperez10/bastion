#!/data/data/com.termux/files/usr/bin/sh
set -eu

REPOSITORY="${BASTION_REPOSITORY:-samperez10/bastion}"
VERSION="${BASTION_VERSION:-latest}"
ARCHIVE="bastion-termux-aarch64.tar.gz"
CHECKSUM="${ARCHIVE}.sha256"
BINARIES="bastion workspace-daemon termux-tui workspace-agent"

say() {
    printf '%s\n' "$*"
}

format_bytes() {
    bytes="${1:-0}"
    if [ "$bytes" -ge 1048576 ]; then
        tenths=$((bytes * 10 / 1048576))
        printf '%s.%s MB' "$((tenths / 10))" "$((tenths % 10))"
    elif [ "$bytes" -ge 1024 ]; then
        tenths=$((bytes * 10 / 1024))
        printf '%s.%s KB' "$((tenths / 10))" "$((tenths % 10))"
    else
        printf '%s B' "$bytes"
    fi
}

spinner_frame() {
    case $((${1:-0} % 10)) in
        0) printf '⠋' ;; 1) printf '⠙' ;; 2) printf '⠹' ;; 3) printf '⠸' ;;
        4) printf '⠼' ;; 5) printf '⠴' ;; 6) printf '⠦' ;; 7) printf '⠧' ;;
        8) printf '⠇' ;; *) printf '⠏' ;;
    esac
}

completed_label() {
    case "$1" in
        "Downloading Bastion") printf 'Download complete' ;;
        "Downloading checksum") printf 'Checksum received' ;;
        *) printf '%s' "$1" ;;
    esac
}

clear_progress() {
    if [ -t 1 ]; then
        printf '\r\033[2K'
    fi
}

download_file() {
    url="$1"
    destination="$2"
    label="$3"
    total="${4:-0}"
    partial="${destination}.partial"
    error_file="${destination}.error"
    rm -f "$partial" "$error_file"

    if [ ! -t 1 ]; then
        say "${label}…"
    fi
    curl -fsSL --remove-on-error --retry 3 --connect-timeout 15 \
        -o "$partial" "$url" 2>"$error_file" &
    DOWNLOAD_PID=$!
    frame=0
    while kill -0 "$DOWNLOAD_PID" 2>/dev/null; do
        if [ -t 1 ]; then
            downloaded=0
            [ ! -f "$partial" ] || downloaded="$(wc -c < "$partial" | tr -d ' ')"
            marker="$(spinner_frame "$frame")"
            if [ "$total" -gt 0 ]; then
                percent=$((downloaded * 100 / total))
                [ "$percent" -le 100 ] || percent=100
                printf '\r\033[2K%s %s  %3s%% · %s / %s' \
                    "$marker" "$label" "$percent" \
                    "$(format_bytes "$downloaded")" "$(format_bytes "$total")"
            else
                printf '\r\033[2K%s %s · %s' \
                    "$marker" "$label" "$(format_bytes "$downloaded")"
            fi
        fi
        frame=$((frame + 1))
        sleep 0.1
    done
    if wait "$DOWNLOAD_PID"; then
        status=0
    else
        status=$?
    fi
    DOWNLOAD_PID=""
    clear_progress
    if [ "$status" -ne 0 ]; then
        detail="$(sed -n '1p' "$error_file")"
        rm -f "$partial" "$error_file"
        [ -n "$detail" ] || detail="curl exited with status $status"
        fail "${label} failed: ${detail}"
    fi
    mv -f "$partial" "$destination"
    rm -f "$error_file"
    downloaded="$(wc -c < "$destination" | tr -d ' ')"
    say "✓ $(completed_label "$label") · $(format_bytes "$downloaded")"
}

fail() {
    printf 'Bastion installer: %s\n' "$*" >&2
    exit 1
}

command -v curl >/dev/null 2>&1 || fail "curl is required (pkg install curl)"
command -v tar >/dev/null 2>&1 || fail "tar is required (pkg install tar)"
command -v sha256sum >/dev/null 2>&1 || fail "sha256sum is required (pkg install coreutils)"
command -v sed >/dev/null 2>&1 || fail "sed is required (pkg install sed)"
command -v df >/dev/null 2>&1 || fail "df is required (pkg install coreutils)"

[ -n "${PREFIX:-}" ] || fail "PREFIX is not set; run this installer inside Termux"
[ "$(uname -m)" = "aarch64" ] || fail "the current release supports ARM64 Termux only"

case "$PREFIX" in
    /data/data/com.termux/files/usr) ;;
    *) fail "unsupported PREFIX: $PREFIX (the current release supports Termux only)" ;;
esac

mkdir -p "$PREFIX/bin" || fail "cannot create $PREFIX/bin"
[ -w "$PREFIX/bin" ] || fail "$PREFIX/bin is not writable"

set -- $(df -Pk "$PREFIX" | tail -n 1)
AVAILABLE_KB="${4:-0}"
case "$AVAILABLE_KB" in
    *[!0-9]*|'') fail "could not determine available storage" ;;
esac
[ "$AVAILABLE_KB" -ge 32768 ] || fail "at least 32 MB of free storage is required"

if [ -n "${BASTION_BASE_URL:-}" ]; then
    BASE_URL="$BASTION_BASE_URL"
    ARCHIVE_SIZE=0
elif [ "$VERSION" = "latest" ]; then
    RELEASES="$(curl -fsSL --retry 3 --connect-timeout 15 \
        "https://api.github.com/repos/${REPOSITORY}/releases?per_page=1")" \
        || fail "cannot reach GitHub; check your connection and try again"
    VERSION="$(printf '%s' "$RELEASES" \
        | sed -n 's/.*"tag_name": "\([^"]*\)".*/\1/p' \
        | sed -n '1p')"
    [ -n "$VERSION" ] || fail "no published Bastion release was found"
    ARCHIVE_SIZE="$(printf '%s' "$RELEASES" \
        | sed -n "/\"name\": \"${ARCHIVE}\"/,/\"size\":/s/.*\"size\": \([0-9][0-9]*\).*/\1/p" \
        | sed -n '1p')"
    ARCHIVE_SIZE="${ARCHIVE_SIZE:-0}"
    BASE_URL="https://github.com/${REPOSITORY}/releases/download/${VERSION}"
else
    case "$VERSION" in
        v*) ;;
        *) VERSION="v${VERSION}" ;;
    esac
    ARCHIVE_SIZE=0
    BASE_URL="https://github.com/${REPOSITORY}/releases/download/${VERSION}"
fi

TMP_ROOT="${TMPDIR:-$PREFIX/tmp}"
WORK_DIR="$(mktemp -d "$TMP_ROOT/bastion-install.XXXXXX")"
BACKUP_DIR="$WORK_DIR/backup"
PAYLOAD_DIR="$WORK_DIR/payload"
mkdir -p "$BACKUP_DIR" "$PAYLOAD_DIR"

rollback() {
    for binary in $BINARIES; do
        backup="$BACKUP_DIR/$binary"
        target="$PREFIX/bin/$binary"
        rm -f "$PREFIX/bin/.$binary.bastion-new.$$"
        if [ -f "$backup" ]; then
            mv -f "$backup" "$target"
        elif [ -f "$BACKUP_DIR/$binary.missing" ]; then
            rm -f "$target"
        fi
    done
}

cleanup() {
    rm -rf "$WORK_DIR"
}

interrupted() {
    if [ -n "${DOWNLOAD_PID:-}" ]; then
        kill "$DOWNLOAD_PID" 2>/dev/null || true
        wait "$DOWNLOAD_PID" 2>/dev/null || true
        clear_progress
    fi
    if [ "${ACTIVATING:-0}" = "1" ]; then
        rollback
    fi
    cleanup
    trap - EXIT
    exit 130
}

trap cleanup EXIT
trap interrupted HUP INT TERM

say ""
say "BASTION INSTALLER · ${VERSION}"
say "Termux ARM64 · verified release"
say ""
download_file "$BASE_URL/$ARCHIVE" "$WORK_DIR/$ARCHIVE" "Downloading Bastion" "$ARCHIVE_SIZE"
download_file "$BASE_URL/$CHECKSUM" "$WORK_DIR/$CHECKSUM" "Downloading checksum" 96

if (
    cd "$WORK_DIR"
    sha256sum -c "$CHECKSUM" >/dev/null
); then
    say "✓ Release checksum verified"
else
    fail "release checksum verification failed"
fi

say "Extracting release…"
tar -xzf "$WORK_DIR/$ARCHIVE" -C "$PAYLOAD_DIR"
PACKAGE_DIR="$PAYLOAD_DIR/bastion-termux-aarch64"
[ -d "$PACKAGE_DIR" ] || fail "release archive has an unexpected layout"

for binary in $BINARIES; do
    [ -f "$PACKAGE_DIR/bin/$binary" ] || fail "release is missing $binary"
    chmod 755 "$PACKAGE_DIR/bin/$binary"
done
say "✓ Release extracted"

mkdir -p "$PREFIX/bin"
for binary in $BINARIES; do
    target="$PREFIX/bin/$binary"
    if [ -f "$target" ]; then
        cp -p "$target" "$BACKUP_DIR/$binary"
    else
        : > "$BACKUP_DIR/$binary.missing"
    fi
done

ACTIVATING=1
say "Installing Bastion…"
for binary in $BINARIES; do
    target="$PREFIX/bin/$binary"
    staged="$PREFIX/bin/.$binary.bastion-new.$$"
    if ! cp "$PACKAGE_DIR/bin/$binary" "$staged" || ! mv -f "$staged" "$target"; then
        rm -f "$staged"
        rollback
        fail "could not activate $binary; previous executables were restored"
    fi
done

if ! "$PREFIX/bin/bastion" --version >/dev/null 2>&1; then
    rollback
    fail "the installed executable did not start; previous executables were restored"
fi
ACTIVATING=0
say "✓ Bastion binaries installed"

say ""
"$PREFIX/bin/bastion" --version
if [ "${BASTION_SKIP_INTEGRATIONS:-0}" != "1" ]; then
    "$PREFIX/bin/bastion" doctor --repair \
        || say "Warning: one or more Bastion checks need attention; run: bastion doctor --repair"
else
    "$PREFIX/bin/bastion" doctor
fi
say ""
say "Installed successfully. Run: bastion"
