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

[ -n "${PREFIX:-}" ] || fail "PREFIX is not set; run this installer inside Termux"
case "$PREFIX" in
    /data/data/com.termux/files/usr) ;;
    *) fail "unsupported PREFIX: $PREFIX (the current release supports Termux only)" ;;
esac

missing_commands=""
missing_packages=""

add_missing_dependency() {
    dependency_command="$1"
    dependency_package="$2"
    if command -v "$dependency_command" >/dev/null 2>&1; then
        return
    fi
    case " $missing_commands " in
        *" $dependency_command "*) ;;
        *) missing_commands="${missing_commands}${missing_commands:+ }$dependency_command" ;;
    esac
    case " $missing_packages " in
        *" $dependency_package "*) ;;
        *) missing_packages="${missing_packages}${missing_packages:+ }$dependency_package" ;;
    esac
}

collect_missing_dependencies() {
    missing_commands=""
    missing_packages=""
    add_missing_dependency curl curl
    add_missing_dependency tar tar
    add_missing_dependency sed sed
    for dependency_command in sha256sum df wc tr tail mktemp cp mv rm chmod mkdir sort sleep uname; do
        add_missing_dependency "$dependency_command" coreutils
    done
}

install_dependencies() {
    collect_missing_dependencies
    [ -n "$missing_packages" ] || return 0

    say ""
    say "Bastion needs these Termux packages: $missing_packages"
    say "Missing commands: $missing_commands"
    install_choice="${BASTION_INSTALL_DEPS:-}"
    case "$install_choice" in
        1) ;;
        0) fail "required packages are missing; run: pkg install $missing_packages" ;;
        '')
            if [ -t 1 ] && [ -r /dev/tty ] && [ -w /dev/tty ]; then
                printf 'Install required packages now? [Y/n] ' > /dev/tty
                IFS= read -r install_answer < /dev/tty || install_answer="n"
                case "$install_answer" in
                    ''|y|Y|yes|YES|Yes) ;;
                    *) fail "required packages were not installed; run: pkg install $missing_packages" ;;
                esac
            else
                fail "required packages are missing; run: pkg install $missing_packages (or rerun with BASTION_INSTALL_DEPS=1)"
            fi
            ;;
        *) fail "BASTION_INSTALL_DEPS must be 1 or 0" ;;
    esac

    command -v pkg >/dev/null 2>&1 \
        || fail "Termux package manager not found; run: pkg install $missing_packages"
    say "Installing required packages…"
    # Word splitting is intentional: this is the deduplicated package list above.
    pkg install -y $missing_packages \
        || fail "could not install required packages; run: pkg install $missing_packages"
    collect_missing_dependencies
    [ -z "$missing_commands" ] \
        || fail "packages installed, but commands are still missing: $missing_commands"
    say "✓ Required packages installed"
}

version_relation() {
    relation_left="${1#v}"
    relation_right="${2#v}"
    relation_left_base="${relation_left%%-*}"
    relation_right_base="${relation_right%%-*}"
    relation_left_pre=""
    relation_right_pre=""
    [ "$relation_left" = "$relation_left_base" ] || relation_left_pre="${relation_left#*-}"
    [ "$relation_right" = "$relation_right_base" ] || relation_right_pre="${relation_right#*-}"

    old_ifs="$IFS"
    IFS=.
    set -- $relation_left_base
    left_major="${1:-0}"; left_minor="${2:-0}"; left_patch="${3:-0}"
    set -- $relation_right_base
    right_major="${1:-0}"; right_minor="${2:-0}"; right_patch="${3:-0}"
    IFS="$old_ifs"
    for pair in "$left_major:$right_major" "$left_minor:$right_minor" "$left_patch:$right_patch"; do
        left_number="${pair%%:*}"
        right_number="${pair#*:}"
        [ "$left_number" -eq "$right_number" ] || {
            if [ "$left_number" -lt "$right_number" ]; then printf 'lt'; else printf 'gt'; fi
            return
        }
    done
    if [ -z "$relation_left_pre" ] && [ -n "$relation_right_pre" ]; then printf 'gt'; return; fi
    if [ -n "$relation_left_pre" ] && [ -z "$relation_right_pre" ]; then printf 'lt'; return; fi
    if [ "$relation_left_pre" = "$relation_right_pre" ]; then printf 'eq'; return; fi
    first_pre="$(printf '%s\n%s\n' "$relation_left_pre" "$relation_right_pre" | sort -V | sed -n '1p')"
    if [ "$first_pre" = "$relation_left_pre" ]; then printf 'lt'; else printf 'gt'; fi
}

install_dependencies
[ "$(uname -m)" = "aarch64" ] || fail "the current release supports ARM64 Termux only"

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

if [ -x "$PREFIX/bin/bastion" ]; then
    installed_output="$($PREFIX/bin/bastion --version 2>/dev/null || true)"
    set -- $installed_output
    installed_version="${2:-}"
    if [ -n "$installed_version" ]; then
        version_state="$(version_relation "$installed_version" "${VERSION#v}")"
        case "$version_state" in
            eq)
                if [ "${BASTION_FORCE_INSTALL:-0}" != "1" ]; then
                    say ""
                    say "Bastion $installed_version is already installed."
                    if [ "${BASTION_SKIP_INTEGRATIONS:-0}" != "1" ]; then
                        "$PREFIX/bin/bastion" doctor --repair \
                            || say "Warning: one or more Bastion checks need attention; run: bastion doctor --repair"
                    else
                        "$PREFIX/bin/bastion" doctor
                    fi
                    exit 0
                fi
                say "Reinstalling Bastion $installed_version…"
                ;;
            lt) say "Upgrading Bastion $installed_version → ${VERSION#v}…" ;;
            gt)
                [ "${BASTION_ALLOW_DOWNGRADE:-0}" = "1" ] \
                    || fail "installed Bastion $installed_version is newer than ${VERSION#v}; set BASTION_ALLOW_DOWNGRADE=1 to continue"
                say "Downgrading Bastion $installed_version → ${VERSION#v}…"
                ;;
        esac
    fi
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
