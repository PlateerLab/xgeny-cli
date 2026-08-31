#!/bin/sh

set -eu

[ "$#" -eq 2 ] || {
    printf '%s\n' "usage: audit-native-binary.sh BINARY TARGET" >&2
    exit 2
}

binary=$1
target=$2
if [ ! -f "$binary" ] || [ -L "$binary" ]; then
    printf '%s\n' "native audit input must be one regular file" >&2
    exit 2
fi

case "$target" in
    *-unknown-linux-musl)
        command -v readelf >/dev/null 2>&1 || {
            printf '%s\n' "readelf is required for the Linux native audit" >&2
            exit 2
        }
        target_libdir=$(rustc --print target-libdir --target "$target")
        [ -f "$target_libdir/self-contained/libc.a" ] || {
            printf '%s\n' "audited Rust musl libc archive is missing" >&2
            exit 1
        }
        [ -f "$target_libdir/self-contained/libunwind.a" ] || {
            printf '%s\n' "audited Rust musl libunwind archive is missing" >&2
            exit 1
        }
        case "$target" in
            x86_64-*) expected_machine='Advanced Micro Devices X86-64' ;;
            aarch64-*) expected_machine='AArch64' ;;
        esac
        readelf -hW "$binary" | grep -F "Machine:" | grep -Fq "$expected_machine" || {
            printf '%s\n' "Linux release binary architecture does not match its target" >&2
            exit 1
        }
        if readelf -lW "$binary" | grep -Fq ' INTERP '; then
            printf '%s\n' "Linux release binary has a dynamic interpreter" >&2
            exit 1
        fi
        if readelf -dW "$binary" | grep -Fq '(NEEDED)'; then
            printf '%s\n' "Linux release binary has a dynamic shared-library dependency" >&2
            exit 1
        fi
        if readelf -dW "$binary" | grep -Eq '\((RPATH|RUNPATH)\)'; then
            printf '%s\n' "Linux release binary has a runtime library search path" >&2
            exit 1
        fi
        ;;
    *-apple-darwin)
        command -v otool >/dev/null 2>&1 || {
            printf '%s\n' "otool is required for the macOS native audit" >&2
            exit 2
        }
        command -v lipo >/dev/null 2>&1 || {
            printf '%s\n' "lipo is required for the macOS native audit" >&2
            exit 2
        }
        case "$target" in
            x86_64-*) expected_arch='x86_64' ;;
            aarch64-*) expected_arch='arm64' ;;
        esac
        [ "$(lipo -archs "$binary")" = "$expected_arch" ] || {
            printf '%s\n' "macOS release binary architecture does not match its target" >&2
            exit 1
        }
        if otool -l "$binary" | grep -Fq 'cmd LC_RPATH'; then
            printf '%s\n' "macOS release binary contains LC_RPATH" >&2
            exit 1
        fi
        dependencies=$(otool -L "$binary" | sed '1d' | awk '{print $1}')
        for dependency in $dependencies; do
            case "$dependency" in
                /usr/lib/*|/System/Library/*) ;;
                *)
                    printf '%s\n' "macOS release binary has a non-system dependency" >&2
                    exit 1
                    ;;
            esac
        done
        ;;
    *)
        printf '%s\n' "unsupported native audit target: $target" >&2
        exit 2
        ;;
esac

printf '%s\n' "native binary audit passed for $target"
