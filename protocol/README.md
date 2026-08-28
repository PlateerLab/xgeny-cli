# XGENy Protocol v0.1

이 디렉터리는 XGENy core와 adapter 사이의 언어 중립 정본 계약이다. Rust, TypeScript, Python 구현은 제품 내부 타입을 직접 공유하지 않고 이 schema와 fixture로 conformance를 검증한다.

## 정본

- JSON Schema draft 2020-12
- schema ID namespace: `https://schemas.xgeny.dev/v1alpha1/`
- API version: `xgeny.io/v1alpha1`
- JSON field naming: camelCase
- timestamp: RFC 3339 UTC
- content digest: `sha256:<lowercase hex>`
- digest canonicalization: RFC 8785 JSON Canonicalization Scheme

## 파일

```text
schema/v1alpha1/
  common.schema.json
  capability-definition.schema.json
  capability-instance.schema.json
  permission-request.schema.json
  policy-decision.schema.json
  invocation-plan.schema.json
  work-graph.schema.json
  run-journal-event.schema.json
  execution-receipt.schema.json

fixtures/v1alpha1/
  manifest.json
  valid/
  invalid/
```

`fixtures/v1alpha1/manifest.json`의 `expectedValid`이 검증 기대값이다. valid fixture는 모두 통과하고 invalid fixture는 지정 schema에서 반드시 실패해야 한다.

## 호환 규칙

- core object의 unknown top-level field는 거부한다. 확장은 URI-keyed `extensions`에 넣고 reader는 이해하지 못한 optional extension을 보존한다.
- unknown required extension은 fail closed한다.
- 계약의 의미가 호환되지 않게 바뀌면 `contractVersion` major를 올린다.
- wire schema 변경은 `apiVersion`과 schema directory로 관리한다.
- XGEN legacy identifier는 metadata나 adapter binding에만 둔다.
- credential 원문은 canonical object, fixture, journal, receipt에 넣지 않는다. OAuth와 presigned URL 같은 transient transport credential은 core 계약 밖에서 수명과 scope를 제한한다.
- credential이 필요한 Capability input은 문자열 secret 대신 `{ "credentialRef": "..." }` 형태를 사용한다.
- `InvocationPlan.argumentsSizeBytes`는 runtime이 canonical JSON bytes에서 다시 계산하며 1 MiB를 넘으면 거부한다.
- validator는 CapabilityDefinition 자체뿐 아니라 내장된 `inputSchema`와 `outputSchema`에도 JSON Schema 2020-12 `check_schema`를 수행한다.
- schema `$ref`는 bundled 또는 digest-pinned registry에서만 해석하고 validation 중 임의 HTTP fetch를 하지 않는다.
- persisted `ArtifactRef`에는 presigned URL이나 storage credential을 넣지 않는다. 다운로드·업로드 URL은 별도 transient transfer 응답에서만 전달한다.

## Digest 규칙

- `RunJournalEvent.eventDigest`는 `eventDigest` 필드를 제외한 event를 RFC 8785로 canonicalize한 뒤 SHA-256한다.
- 첫 event의 `previousEventDigest`는 `null`, 이후에는 직전 event digest다.
- `ExecutionReceipt.receiptDigest`는 `receiptDigest`를 제외한 receipt를 같은 방식으로 계산한다.
- secret·PII는 canonicalization 전에 reference 또는 명시적 redaction marker로 대체한다.
- snapshot은 마지막 `journalSequence`와 `journalHeadDigest`를 함께 저장한다.

JSON Schema는 구조와 "정규화 완료" 선언을 검증한다. digest 재계산, argument byte 크기, sequence 연속성, timestamp 순서, 실제 path/symlink 정규화, secret reference 검사, policy 교집합, 선택 Instance의 eligible 여부, 필수 verification plan의 Receipt coverage, timeout 상한, effect 이후 fallback 금지는 runtime conformance test에서 검증한다.
