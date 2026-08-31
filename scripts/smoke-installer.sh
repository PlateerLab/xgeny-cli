#!/bin/sh

set -eu

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        printf '%s\n' "sha256sum or shasum is required" >&2
        exit 2
    fi
}

[ "$#" -eq 3 ] || {
    printf '%s\n' "usage: smoke-installer.sh BINARY ASSET INSTALLER" >&2
    exit 2
}

binary=$1
asset=$2
installer=$3
[ -f "$binary" ] || { printf '%s\n' "smoke binary is missing" >&2; exit 2; }
[ -f "$installer" ] || { printf '%s\n' "installer is missing" >&2; exit 2; }

reported_version=$("$binary" --version)
case "$reported_version" in
    "xgeny "*) package_version=${reported_version#xgeny } ;;
    *) printf '%s\n' "binary version output is invalid" >&2; exit 2 ;;
esac
tag="v$package_version"
semver_tag_regex='^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-((0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)(\.(0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*))?$'
printf '%s' "$tag" \
    | grep -Eq "$semver_tag_regex" \
    || { printf '%s\n' "binary version is not SemVer" >&2; exit 2; }

test_root=$(mktemp -d "${TMPDIR:-/tmp}/xgeny-installer-smoke.XXXXXX")
server_root="$test_root/server"
release_root="$server_root/$tag"
install_root="$test_root/install"
test_home="$test_root/home"
unexpected_state="$test_root/unexpected-state"
mkdir -p "$release_root" "$test_home"
cp "$binary" "$release_root/$asset"

digest=$(sha256_file "$release_root/$asset")
printf '%s  %s\n' "$digest" "$asset" > "$release_root/checksums.sha256"

port=38191
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$server_root" \
    >"$test_root/server.log" 2>&1 &
server_pid=$!
cleanup() {
    kill "$server_pid" >/dev/null 2>&1 || true
    wait "$server_pid" >/dev/null 2>&1 || true
    rm -rf "$test_root"
}
trap cleanup EXIT HUP INT TERM

ready=0
attempt=0
while [ "$attempt" -lt 100 ]; do
    if curl -q -fsS "http://127.0.0.1:$port/$tag/checksums.sha256" >/dev/null 2>&1; then
        ready=1
        break
    fi
    attempt=$((attempt + 1))
    sleep 0.1
done
[ "$ready" -eq 1 ] || { printf '%s\n' "loopback fixture server did not start" >&2; exit 1; }

run_installer() {
    HOME="$test_home" \
    XGENY_INSTALLER_TESTING=1 \
    XGENY_DOWNLOAD_BASE_URL="http://127.0.0.1:$port" \
    XGENY_INSTALL_DIR="$install_root" \
    XGENY_STATE_HOME="$unexpected_state" \
        sh "$installer" --version "$tag" >/dev/null
}

if HOME="$test_home" \
    XGENY_INSTALLER_TESTING=1 \
    XGENY_DOWNLOAD_BASE_URL="http://127.0.0.1:$port" \
    XGENY_INSTALL_DIR="$install_root" \
        sh "$installer" --version v1.2.3-01 >/dev/null 2>&1; then
    printf '%s\n' "installer accepted a non-SemVer numeric prerelease" >&2
    exit 1
fi

run_installer

installed="$install_root/xgeny"
if [ ! -f "$installed" ] || [ -L "$installed" ]; then
    printf '%s\n' "installer did not create one regular binary" >&2
    exit 1
fi
installed_digest=$(sha256_file "$installed")
printf 'corrupt' >> "$release_root/$asset"
if run_installer 2>/dev/null; then
    printf '%s\n' "installer accepted a checksum mismatch" >&2
    exit 1
fi
after_failed_install=$(sha256_file "$installed")
[ "$after_failed_install" = "$installed_digest" ] \
    || { printf '%s\n' "failed install changed the existing binary" >&2; exit 1; }
cp "$binary" "$release_root/$asset"

run_installer
[ "$("$installed" --version)" = "$reported_version" ] \
    || { printf '%s\n' "installed version is wrong" >&2; exit 1; }
XGENY_STATE_HOME="$unexpected_state" "$installed" protocol check >/dev/null \
    || { printf '%s\n' "installed protocol check failed" >&2; exit 1; }
licenses_output="$test_root/licenses.txt"
XGENY_STATE_HOME="$unexpected_state" "$installed" licenses > "$licenses_output" \
    || { printf '%s\n' "installed license notice command failed" >&2; exit 1; }
grep -Fq 'XGENy CLI Third-Party License Notices' "$licenses_output" \
    || { printf '%s\n' "installed binary is missing Cargo dependency notices" >&2; exit 1; }
grep -Fq 'Copyright notices for The Rust Standard Library' "$licenses_output" \
    || { printf '%s\n' "installed binary is missing Rust library notices" >&2; exit 1; }
grep -Fq '===== musl C runtime notices =====' "$licenses_output" \
    || { printf '%s\n' "installed binary is missing musl runtime notices" >&2; exit 1; }
grep -Fq '===== LLVM libunwind notices =====' "$licenses_output" \
    || { printf '%s\n' "installed binary is missing LLVM libunwind notices" >&2; exit 1; }
[ ! -e "$unexpected_state" ] \
    || { printf '%s\n' "installer smoke unexpectedly created runtime state" >&2; exit 1; }

rm -f "$installed"
[ ! -e "$installed" ] \
    || { printf '%s\n' "test-owned install cleanup failed" >&2; exit 1; }

sentinel="$test_root/symlink-target"
printf '%s' "unchanged" > "$sentinel"
ln -s "$sentinel" "$installed"
if run_installer 2>/dev/null; then
    printf '%s\n' "installer accepted a symbolic-link destination" >&2
    exit 1
fi
[ "$(cat "$sentinel")" = "unchanged" ] \
    || { printf '%s\n' "symbolic-link target was modified" >&2; exit 1; }
rm -f "$installed"

printf '%s\n' "installer smoke passed for $asset"
