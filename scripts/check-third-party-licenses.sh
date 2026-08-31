#!/bin/sh

set -eu

version=0.9.2
host_os=$(uname -s)
host_arch=$(uname -m)
binary_name=cargo-about
case "$host_os:$host_arch" in
    Linux:x86_64|Linux:amd64)
        target=x86_64-unknown-linux-musl
        archive_digest=9099a59e820c38a68b9d65f300662a567d56562f9a10f6aa4c7e86c17c2566af
        ;;
    Linux:aarch64|Linux:arm64)
        target=aarch64-unknown-linux-musl
        archive_digest=af5169282fb6f84e13471493f405437e43ac517744c9ae12fbe2cdf0a6f0e5a8
        ;;
    Darwin:arm64|Darwin:aarch64)
        target=aarch64-apple-darwin
        archive_digest=ae72f0df0c399a1e96336f696fa55b1b28679fd725632eba8cf8e4568467cc3e
        ;;
    MINGW*:x86_64|MSYS*:x86_64|CYGWIN*:x86_64)
        target=x86_64-pc-windows-msvc
        archive_digest=1c03e5890238562497c2d89a3b75b02560af349c1fc3e713d3284f532a5cd748
        binary_name=cargo-about.exe
        ;;
    MINGW*:aarch64|MINGW*:arm64|MSYS*:aarch64|MSYS*:arm64|CYGWIN*:aarch64|CYGWIN*:arm64)
        target=aarch64-pc-windows-msvc
        archive_digest=1251a3cdf09538eb91a4f95af34ea719913db98c54574e1165052616e9ad93ec
        binary_name=cargo-about.exe
        ;;
    *)
        printf '%s\n' "unsupported cargo-about host: $host_os $host_arch" >&2
        printf '%s\n' "run this check on Linux x86-64/ARM64, macOS Apple Silicon, or Windows Git Bash" >&2
        exit 2
        ;;
esac
asset="cargo-about-${version}-${target}.tar.gz"
archive_path="cargo-about-${version}-${target}/${binary_name}"

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
repository_root=$(CDPATH='' cd -- "$script_dir/.." && pwd)
mode=${1:---check}
case "$mode" in
    --check|--write) ;;
    *)
        printf '%s\n' "usage: check-third-party-licenses.sh [--check|--write]" >&2
        exit 2
        ;;
esac

tool_root=$(mktemp -d "${TMPDIR:-/tmp}/xgeny-cargo-about.XXXXXX")
cleanup() {
    rm -rf "$tool_root"
}
trap cleanup EXIT HUP INT TERM

archive="$tool_root/$asset"
curl -q --proto '=https' --proto-redir '=https' --tlsv1.2 \
    --connect-timeout 15 --max-time 120 --max-filesize 52428800 \
    -fsSLo "$archive" \
    "https://github.com/EmbarkStudios/cargo-about/releases/download/${version}/${asset}"

if command -v sha256sum >/dev/null 2>&1; then
    observed_digest=$(sha256sum "$archive" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
    observed_digest=$(shasum -a 256 "$archive" | awk '{print $1}')
else
    printf '%s\n' "sha256sum or shasum is required" >&2
    exit 2
fi
[ "$observed_digest" = "$archive_digest" ] || {
    printf '%s\n' "cargo-about archive checksum mismatch" >&2
    exit 1
}

tar -xzf "$archive" -C "$tool_root" --strip-components=1 "$archive_path"
if [ ! -f "$tool_root/$binary_name" ] || [ -L "$tool_root/$binary_name" ]; then
    printf '%s\n' "cargo-about archive member is not a regular file" >&2
    exit 1
fi
chmod 0755 "$tool_root/$binary_name"

if [ "$mode" = "--write" ]; then
    output="$repository_root/THIRD_PARTY_LICENSES.txt"
else
    output="$tool_root/THIRD_PARTY_LICENSES.txt"
fi

(
    cd "$repository_root"
    "$tool_root/$binary_name" generate \
        --locked \
        --workspace \
        --fail \
        about.hbs \
        -o "$output"
)

if [ "$mode" = "--check" ] && ! cmp -s "$output" "$repository_root/THIRD_PARTY_LICENSES.txt"; then
    printf '%s\n' "THIRD_PARTY_LICENSES.txt is stale; run scripts/check-third-party-licenses.sh --write" >&2
    exit 1
fi

printf '%s\n' "third-party license notices are current"
