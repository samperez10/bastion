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

fail() {
    printf 'Bastion installer: %s\n' "$*" >&2
    exit 1
}

command -v curl >/dev/null 2>&1 || fail "curl is required (pkg install curl)"
command -v tar >/dev/null 2>&1 || fail "tar is required (pkg install tar)"
command -v sha256sum >/dev/null 2>&1 || fail "sha256sum is required (pkg install coreutils)"

[ -n "${PREFIX:-}" ] || fail "PREFIX is not set; run this installer inside Termux"
[ "$(uname -m)" = "aarch64" ] || fail "the current release supports ARM64 Termux only"

case "$PREFIX" in
    /data/data/com.termux/files/usr) ;;
    *) fail "unsupported PREFIX: $PREFIX (the current release supports Termux only)" ;;
esac

if [ -n "${BASTION_BASE_URL:-}" ]; then
    BASE_URL="$BASTION_BASE_URL"
elif [ "$VERSION" = "latest" ]; then
    VERSION="$(curl -fsSL --retry 3 --connect-timeout 15 \
        "https://api.github.com/repos/${REPOSITORY}/releases?per_page=1" \
        | sed -n 's/.*"tag_name": "\([^"]*\)".*/\1/p' \
        | sed -n '1p')"
    [ -n "$VERSION" ] || fail "no published Bastion release was found"
    BASE_URL="https://github.com/${REPOSITORY}/releases/download/${VERSION}"
else
    case "$VERSION" in
        v*) ;;
        *) VERSION="v${VERSION}" ;;
    esac
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
    if [ "${ACTIVATING:-0}" = "1" ]; then
        rollback
    fi
    cleanup
    trap - EXIT
    exit 130
}

trap cleanup EXIT
trap interrupted HUP INT TERM

say "Downloading Bastion ${VERSION}…"
curl -fL --retry 3 --connect-timeout 15 \
    -o "$WORK_DIR/$ARCHIVE" "$BASE_URL/$ARCHIVE"
curl -fL --retry 3 --connect-timeout 15 \
    -o "$WORK_DIR/$CHECKSUM" "$BASE_URL/$CHECKSUM"

(
    cd "$WORK_DIR"
    sha256sum -c "$CHECKSUM"
) || fail "release checksum verification failed"

tar -xzf "$WORK_DIR/$ARCHIVE" -C "$PAYLOAD_DIR"
PACKAGE_DIR="$PAYLOAD_DIR/bastion-termux-aarch64"
[ -d "$PACKAGE_DIR" ] || fail "release archive has an unexpected layout"

for binary in $BINARIES; do
    [ -f "$PACKAGE_DIR/bin/$binary" ] || fail "release is missing $binary"
    chmod 755 "$PACKAGE_DIR/bin/$binary"
done

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

if [ "${BASTION_SKIP_INTEGRATIONS:-0}" != "1" ]; then
    if command -v claude >/dev/null 2>&1; then
        "$PREFIX/bin/workspace-agent" install claude || say "Warning: Claude integration was not installed."
    fi
    if command -v codex >/dev/null 2>&1; then
        "$PREFIX/bin/workspace-agent" install codex || say "Warning: Codex integration was not installed."
    fi
    if command -v agy >/dev/null 2>&1; then
        "$PREFIX/bin/workspace-agent" install antigravity || say "Warning: Antigravity integration was not installed."
    fi
fi

say ""
"$PREFIX/bin/bastion" --version
"$PREFIX/bin/bastion" doctor
say ""
say "Installed successfully. Run: bastion"
