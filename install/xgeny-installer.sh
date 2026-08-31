#!/bin/sh

set -eu

REPOSITORY="PlateerLab/xgeny-cli"
DEFAULT_DOWNLOAD_BASE="https://github.com/${REPOSITORY}/releases/download"

fail() {
    printf '%s\n' "xgeny installer: $*" >&2
    exit 1
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        fail "sha256sum or shasum is required"
    fi
}

usage() {
    cat <<'EOF'
Install the XGENy native binary without modifying shell profiles.

Usage: xgeny-installer.sh [--version vSEMVER] [--install-dir DIR]

Environment:
  XGENY_VERSION       Exact release tag, or "latest" (default)
  XGENY_INSTALL_DIR   Destination directory (default: $HOME/.local/bin)
EOF
}

version=${XGENY_VERSION:-latest}
install_dir=${XGENY_INSTALL_DIR:-}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            [ "$#" -ge 2 ] || fail "--version requires a value"
            version=$2
            shift 2
            ;;
        --install-dir)
            [ "$#" -ge 2 ] || fail "--install-dir requires a value"
            install_dir=$2
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            fail "unknown argument: $1"
            ;;
    esac
done

command -v curl >/dev/null 2>&1 || fail "curl is required"

if [ "$version" = "latest" ]; then
    [ -z "${XGENY_DOWNLOAD_BASE_URL:-}" ] \
        || fail "latest cannot be resolved with a custom download base"
    latest_url=$(curl -q \
        --proto '=https' \
        --proto-redir '=https' \
        --tlsv1.2 \
        --connect-timeout 15 \
        --max-time 60 \
        -fsSL \
        -o /dev/null \
        -w '%{url_effective}' \
        "https://github.com/${REPOSITORY}/releases/latest") \
        || fail "could not resolve the latest release"
    version=${latest_url##*/}
fi

semver_tag_regex='^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-((0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)(\.(0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*))?$'
printf '%s' "$version" \
    | grep -Eq "$semver_tag_regex" \
    || fail "release version must be an exact v-prefixed SemVer tag"

download_base=$DEFAULT_DOWNLOAD_BASE
if [ -n "${XGENY_DOWNLOAD_BASE_URL:-}" ]; then
    [ "${XGENY_INSTALLER_TESTING:-}" = "1" ] \
        || fail "custom download base is reserved for installer tests"
    download_base=${XGENY_DOWNLOAD_BASE_URL%/}
    printf '%s' "$download_base" \
        | grep -Eq '^http://127\.0\.0\.1:[1-9][0-9]{0,4}(/[A-Za-z0-9._~/-]*)?$' \
        || fail "loopback test download base URL is invalid"
fi

os=$(uname -s)
arch=$(uname -m)
case "$os:$arch" in
    Linux:x86_64|Linux:amd64)
        asset="xgeny-x86_64-unknown-linux-musl"
        ;;
    Linux:aarch64|Linux:arm64)
        asset="xgeny-aarch64-unknown-linux-musl"
        ;;
    Darwin:x86_64|Darwin:amd64)
        asset="xgeny-x86_64-apple-darwin"
        ;;
    Darwin:arm64|Darwin:aarch64)
        asset="xgeny-aarch64-apple-darwin"
        ;;
    *)
        fail "unsupported platform: ${os}/${arch}"
        ;;
esac

if [ -z "$install_dir" ]; then
    [ -n "${HOME:-}" ] || fail "HOME is required when --install-dir is omitted"
    install_dir="$HOME/.local/bin"
fi
case "$install_dir" in
    /*) ;;
    *) fail "install directory must be absolute" ;;
esac

umask 077
[ ! -L "$install_dir" ] || fail "install directory must not be a symbolic link"
mkdir -p "$install_dir" || fail "could not create the install directory"
[ -d "$install_dir" ] || fail "install destination is not a directory"
[ ! -L "$install_dir" ] || fail "install directory must not be a symbolic link"

if directory_owner=$(stat -c '%u' "$install_dir" 2>/dev/null); then
    directory_mode=$(stat -c '%a' "$install_dir" 2>/dev/null) \
        || fail "could not inspect install directory permissions"
elif directory_owner=$(stat -f '%u' "$install_dir" 2>/dev/null); then
    directory_mode=$(stat -f '%Lp' "$install_dir" 2>/dev/null) \
        || fail "could not inspect install directory permissions"
else
    fail "could not inspect install directory ownership"
fi
[ "$directory_owner" = "$(id -u)" ] \
    || fail "install directory must be owned by the current user"
permission_tail=$(printf '%s' "$directory_mode" | sed 's/.*\(..\)$/\1/')
case "$permission_tail" in
    [2367]?|?[2367]) fail "install directory must not be group- or world-writable" ;;
esac

target="$install_dir/xgeny"
if [ -e "$target" ] || [ -L "$target" ]; then
    if [ ! -f "$target" ] || [ -L "$target" ]; then
        fail "existing destination is not a regular file"
    fi
fi

tmp_binary=$(mktemp "$install_dir/.xgeny-binary.XXXXXX") \
    || fail "could not create a temporary binary"
tmp_checksums=$(mktemp "$install_dir/.xgeny-checksums.XXXXXX") \
    || {
        rm -f "$tmp_binary"
        fail "could not create a temporary checksum file"
    }
cleanup() {
    rm -f "$tmp_binary" "$tmp_checksums"
}
trap cleanup EXIT HUP INT TERM

asset_url="$download_base/$version/$asset"
checksums_url="$download_base/$version/checksums.sha256"
curl -q \
    --proto '=https,http' \
    --proto-redir '=https' \
    --tlsv1.2 \
    --connect-timeout 15 \
    --max-time 300 \
    --max-filesize 1048576 \
    -fsSL \
    "$checksums_url" \
    -o "$tmp_checksums" \
    || fail "could not download release checksums"
curl -q \
    --proto '=https,http' \
    --proto-redir '=https' \
    --tlsv1.2 \
    --connect-timeout 15 \
    --max-time 300 \
    --max-filesize 268435456 \
    -fsSL \
    "$asset_url" \
    -o "$tmp_binary" \
    || fail "could not download the XGENy binary"

checksums_size=$(wc -c < "$tmp_checksums" | tr -d '[:space:]')
binary_size=$(wc -c < "$tmp_binary" | tr -d '[:space:]')
[ "$checksums_size" -le 1048576 ] \
    || fail "release checksum file exceeds the installer limit"
[ "$binary_size" -le 268435456 ] \
    || fail "release binary exceeds the installer limit"

expected=$(awk -v name="$asset" '
    NF == 2 && $2 == name { count += 1; digest = $1 }
    END { if (count == 1) print digest }
' "$tmp_checksums")
printf '%s' "$expected" | grep -Eq '^[0-9A-Fa-f]{64}$' \
    || fail "release checksum entry is missing, duplicated, or invalid"

actual=$(sha256_file "$tmp_binary")
expected=$(printf '%s' "$expected" | tr 'A-F' 'a-f')
actual=$(printf '%s' "$actual" | tr 'A-F' 'a-f')
[ "$actual" = "$expected" ] || fail "binary checksum verification failed"

chmod 0755 "$tmp_binary" || fail "could not mark the binary executable"
observed_version=$("$tmp_binary" --version) \
    || fail "downloaded binary did not report its version"
[ "$observed_version" = "xgeny ${version#v}" ] \
    || fail "downloaded binary version does not match the requested release"
"$tmp_binary" protocol check >/dev/null \
    || fail "downloaded binary failed its offline protocol check"

mv -f "$tmp_binary" "$target" || fail "could not install the verified binary"
if [ ! -f "$target" ] || [ -L "$target" ]; then
    fail "installed destination is not a regular file"
fi
installed_digest=$(sha256_file "$target")
installed_digest=$(printf '%s' "$installed_digest" | tr 'A-F' 'a-f')
[ "$installed_digest" = "$expected" ] \
    || fail "installed binary digest changed during replacement"
[ "$("$target" --version)" = "xgeny ${version#v}" ] \
    || fail "installed binary version changed during replacement"
"$target" protocol check >/dev/null \
    || fail "installed binary failed its final offline protocol check"
trap - EXIT HUP INT TERM
rm -f "$tmp_checksums"

printf '%s\n' "XGENy ${version#v} installed at $target"
case ":${PATH:-}:" in
    *:"$install_dir":*) ;;
    *) printf '%s\n' "Add $install_dir to PATH to run xgeny from any directory." ;;
esac
