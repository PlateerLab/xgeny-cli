# Native runtime notice provenance

This inventory records the exact native runtime notice inputs audited for the prototype binaries.
The build script verifies the embedded files by byte length and SHA-256. Any toolchain bump must
repeat the target-specific binary audit and update this inventory in the same reviewed change.

## Rust standard library

- Toolchain: `rustc 1.98.0 (88d9e12ae 2026-08-18)`
- Source: `$(rustc --print sysroot)/share/doc/rust/COPYRIGHT-library.html`
- Size: 1,512,520 bytes
- SHA-256: `68129500b616d5838629e68f55ff3aed5e096dacf60ce9eb41bbe599a563afa6`

## Linux musl runtime

- Upstream: `https://musl.libc.org/releases/musl-1.2.5.tar.gz`
- Archive SHA-256: `a9a118bbe84d8764da0ea0d28b3ab3fae8477fc7e4085d90102b8596fc7c75e4`
- Embedded file: `musl-1.2.5-COPYRIGHT`
- Size: 6,204 bytes
- SHA-256: `f9bc4423732350eb0b3f7ed7e91d530298476f8fec0c6c427a1c04ade22655af`

## Linux LLVM libunwind runtime

- Upstream: `https://raw.githubusercontent.com/llvm/llvm-project/52ed14fcd56afc30f9cccd8ca8ce237c2eef7e04/libunwind/LICENSE.TXT`
- LLVM commit: `52ed14fcd56afc30f9cccd8ca8ce237c2eef7e04`
- Embedded file: `llvm-libunwind-52ed14f-LICENSE.TXT`
- Size: 16,706 bytes
- SHA-256: `b5efebcaca80879234098e52d1725e6d9eb8fb96a19fce625d39184b705f7b6d`

## GCC startup objects

The audited musl target may include GCC startup/runtime objects covered by GPLv3 plus the GCC
Runtime Library Exception 3.1. The exception permits distribution of an eligible executable under
the executable's chosen terms and does not require bundling the GCC GPL text with that executable.
The audited exception source is GCC 9.2.0 `COPYING.RUNTIME` from
`https://raw.githubusercontent.com/gcc-mirror/gcc/releases/gcc-9.2.0/COPYING.RUNTIME`
(3,324 bytes), SHA-256
`9d6b43ce4d8de0c878bf16b54d8e7a10d9bd42b75178153e3af6a815bdc90f74`.
Every GCC or linker/toolchain bump must re-audit the final imported symbols and exception coverage.

## Windows and macOS platform runtimes

Windows builds use `/MT`. Release CI inspects PE imports and rejects dynamic Visual C++ or Universal
CRT dependencies; Microsoft platform DLLs are used under the applicable Windows SDK/runtime terms
and their license text is not embedded in the XGENy binary. Release CI selects the
`windows-2025-vs2026` runner label and records its concrete image and MSVC tool version during the
import audit.

macOS builds use the pinned Xcode 16.4 toolchain and macOS 15.5 SDK. Release CI inspects Mach-O
dependencies and permits only Apple system-library paths. Apple SDK/runtime license text is not
redistributed inside the XGENy binary.
