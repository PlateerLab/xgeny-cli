# Capability-confined filesystem read adapter

`xgeny-adapter-filesystem`은 한 user-selected workspace 안에서 bounded read/list/stat/search를 수행하는
제품 leaf adapter다. 이 문서는 그중 exact UTF-8 `read-text` 경계를 다루며 query 계약은
[workspace filesystem discovery](workspace-filesystem-discovery.md)에 정리한다. XGEN, Connector, DB
server, MinIO, daemon 또는 특정 model provider가 필요하지 않는다. SQLite와 마찬가지로 필요한 Rust
library code는 최종 binary에 link된다.

## Composition

Trusted host가 workspace root를 한 번 열고 같은 객체에서 resolver, root-bound Instance binding,
adapter와 verifier를 만든다.

```rust,no_run
use xgeny_adapter_filesystem::{ReadTextLimits, WorkspaceId, WorkspaceRoot};

let workspace = WorkspaceRoot::open_ambient(
    "/user-selected/project",
    WorkspaceId::new("primary")?,
)?;
let resolver = workspace.resolver();
let binding = workspace.binding();
let adapter = workspace.read_text_adapter(ReadTextLimits::default());
let verifier = adapter.verifier();

// CapabilityInstance.binding = binding
// CapabilityInstance.features = { sync: true, task: false,
//                                 cancellable: false, idempotencyQuery: false }
// EffectAdapterRegistry::register(&binding, adapter)
// EffectVerifierRegistry::register(&binding, verifier)
# Ok::<(), Box<dyn std::error::Error>>(())
```

Protocol fixture의 generic `builtin://core-os/filesystem` binding을 그대로 등록하면 제품 adapter와
dispatch되지 않는다. Composition은 반드시 `workspace.binding()`으로 현재 Instance를 root-bound
형태로 만든다. `WorkspaceId`와 실제 root의 매핑은 해당 durable Run을 재개하는 동안 바뀌면 안
된다. Public CLI는 이 mapping을 재시작 시 검증해야 한다.

`ReadTextAdapter`는 synchronous one-shot read만 구현한다. 따라서 해당 제품 Instance는 `sync=true`만
advertise하고 `task`, `cancellable`, `idempotencyQuery`는 모두 `false`여야 한다. Public Definition이
지원 가능한 상위 집합을 표현하더라도 실제 Instance가 구현하지 않은 기능을 claim하면 adapter와
verifier가 dispatch material을 거부한다.

## Path contract

모델 또는 caller는 `README.md`, `src/lib.rs` 같은 UTF-8 상대경로를 제안한다. Resolver가 이를
`workspace:primary/README.md` 형태로 만들며 이미 canonical인 token에 다시 적용해도 같은 값을
반환한다. Adapter에는 이 canonical token만 도달한다.

ADR-0024 이후 resolver는 list/stat/search를 위해 `.`을 `workspace:primary` root token으로 허용한다.
`read-text`는 regular file만 읽으므로 root token 실행은 실패한다. 그 외 다음 형태는 모든 OS에서
거부한다.

- absolute, drive, UNC, parent traversal, empty component
- backslash, colon/ADS, NUL/control과 Windows-forbidden punctuation
- trailing dot/space와 extension 앞 space를 포함한 reserved device spelling
- 4096-byte path, 255-byte component 또는 256-component 상한 초과
- 다른 Workspace ID의 canonical token

Case folding과 Unicode NFC/NFD normalization은 하지 않는다. 이 선택은 confinement와 portable
logical identity를 위한 것이며 case-insensitive filesystem에서 물리 alias가 생기지 않는다는
보장은 아니다.

## Read and verification contract

- Intermediate directory와 leaf를 각각 handle-relative no-follow로 연다.
- Leaf metadata와 bounded read는 같은 열린 handle에서 수행한다.
- Symlink는 workspace 내부를 향해도 거부한다.
- 일반 파일만 허용하며 Windows reparse point도 거부한다.
- Default와 hard maximum은 raw 64 KiB다.
- 최대 크기 read까지 다음 turn에 전달하려면 MVP host의 effective context budget은 512 KiB여야 한다.
  더 작은 budget은 durable output을 잃지 않고 planning 직전에 pause할 수 있다.
- `max + 1` read 뒤 strict UTF-8을 검사한다.
- output은 exact `{content, digest}`이고 digest는 content UTF-8 bytes의 SHA-256이다.
- verifier는 파일을 재open하지 않고 durable `ToolOutputRecord`의 exact shape, hard maximum과
  content/evidence digest만 검증한다.
- Artifact에는 fixed ID, `text/plain; charset=utf-8`, byte size와 content digest만 기록한다.

파일 내용은 schema 7/8 SQLite tool-output sidecar와 다음 PlanningContext에 의도적으로 존재한다.
Journal, projection, Receipt, Artifact name과 adapter의 `Debug`/error에는
내용·raw/canonical path·absolute root·OS error를 넣지 않는다. 승인 요청처럼 권한 검토에 필요한
기존 Core surface는 canonical resource identity를 보유한다. SQLite 자체의 at-rest encryption은
별도 과제다.

## 현재 검증 명령

```bash
cargo test -p xgeny-adapter-filesystem --all-targets --locked
cargo test -p xgeny-cli --test durable_driver \
  real_filesystem_adapter_reaches_next_turn_and_replays_after_sqlite_reopen \
  -- --exact
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
```

GitHub Actions의 Ubuntu, macOS와 Windows job이 workspace test를 실행한다. Windows junction test가
fixture를 만들지 못하면 조용히 skip하지 않고 실패한다.

## 해석 범위

이 adapter는 path traversal/symlink/junction confinement와 bounded observation을 제공한다. Hard
link, mount/FUSE/network filesystem, hostile same-UID process, concurrent write의 atomic snapshot,
filesystem timeout과 untrusted Rust plugin sandbox는 제공하지 않는다. Local read 승인은 remote model
egress 승인이 아니며 public CLI가 별도 결정해야 한다. 자세한 결정은
[ADR-0022](../adr/0022-capability-confined-filesystem-read-adapter.md)를 따른다.
