# ADR-0042: Explicit provider output dialects and thinking options

- Date: 2026-09-21
- Status: Proposed
- Extends: ADR-0017, ADR-0032
- Protocol / journal / SQLite schema changes: none

## Decision

Keep model access inside the native OpenAI-compatible planner adapter. A provider
accepting Chat Completions does not necessarily support server-enforced JSON
Schema. Add explicit immutable `ResponseFormat` (`json_schema`, `json_object`)
and `ThinkingMode` (`default`, `disabled`, `enabled`) request settings. Do not
infer capabilities from endpoint names or model names, automatically retry a
different dialect, or silently switch models.

The default remains `json_schema` with `default` thinking. Its wire body and
request-profile digest remain unchanged, including temperature 0 and seed 0, so
existing journal commitments and resume checks remain valid.

`json_object` requests JSON syntax and includes the entire production proposal
schema in the committed system prompt. It omits the schema-only wire fields and
seed. The host still runs the identical strict duplicate-key, unknown-field,
model-identity, size, depth, proposal, capability, and policy validation. JSON
syntax support does **not** mean provider-side schema enforcement or permission
to execute a model suggestion.

Thinking extensions are operator opt-ins for providers implementing them:

- `default`: no extra thinking parameters.
- `disabled`: `thinking: {"type":"disabled"}`, temperature 0, no seed.
- `enabled`: `thinking: {"type":"enabled"}`, `reasoning_effort: "low"`, no seed
  or temperature. Existing completion-token, timeout, response-size and call
  budgets remain unchanged; this is not an unlimited reasoning mode.

Every changed prompt, response dialect, sampling omission, and thinking option
is committed in the request-profile digest. Model profile persistence and CLI
resume must restore these options before verifying the committed digest.
Credentials and endpoint locations remain outside the digest and logs.

## Compatibility checks and lifecycle

The JSON Schema compatibility probe retains its adversarial extra-field request
to detect providers ignoring strict schema. JSON Object's probe instead asks for
a conforming completion and includes the schema in its prompt. Success means
one locally valid answer, **not** a claim that the server enforces the schema.

Both modes use the same native reservation, result validation, settlement and
Unknown lifecycle. Truncation does not accept a partial proposal; a timed-out
call remains Unknown and is not automatically retried. Raw model reasoning is
not exposed or retained as a tool result. No streaming, browser, network tool,
new host authority, or parallel direct-HTTP application runtime is introduced.

## Verification

Offline tests cover legacy golden commitments, explicit default wire identity,
JSON Object and both thinking options, distinct/restorable commitments, unknown
enum rejection, constrained prompts, valid and invalid compatibility answers,
and truncation. Loopback HTTP tests exercise native plan acceptance, invalid
proposal rejection, output limits, exact single-call settlement, reasoning
redaction and timeout-to-Unknown behavior. These are domain-independent cases;
no contest, customer dataset or paid endpoint is required.
