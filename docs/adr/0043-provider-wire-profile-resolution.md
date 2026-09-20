# ADR-0043: Provider wire options in CLI profiles and restart resolution

- Status: Accepted
- Date: 2026-09-21
- Scope: OpenAI-compatible provider options, CLI profiles, restart validation

## Decision

CLI integration of the provider modes defined in ADR-0042.

Keep all planning inside the existing XGENy provider → validated proposal → journal → capability
flow. Do not bypass the harness or infer behavior from an endpoint hostname or a model ID.

`model setup`, `model check`, `run`, and `resume` accept two non-secret options:

- `--response-format json_schema|json_object` (default `json_schema`)
- `--thinking default|disabled|enabled` (default `default`, which omits the provider extension)

Resolve each field in order: explicit option, `XGENY_OPENAI_RESPONSE_FORMAT` /
`XGENY_OPENAI_THINKING`, selected profile, default. Invalid values fail closed; there is no automatic
fallback after provider rejection. Profiles persist `responseFormat` and `thinking` with serde defaults,
so pre-existing format-v1 files still load with the original wire behavior and request digest.

JSON Object mode is **host-validated**, not proof that the server enforces a JSON Schema. It carries
the proposal schema in the request and retains the exact same host envelope, proposal, invocation,
policy and capability validation. Setup/check output states the weaker server guarantee explicitly.
Explicit thinking options are provider extensions, not a claim that every compatible server supports
them. `enabled` requests low reasoning effort and omits temperature and seed; `disabled` omits seed.
The default does not introduce any of these extensions. No reasoning content is shown as progress.

## Restart and credentials

Wire options and inference limits are inputs to the committed request profile digest. The manifest
remains the authority; profile settings cannot silently change an existing Run. Incomplete resume
resolves the currently selected profile plus explicit/environment overrides and compares the digest
before calling the model. A mismatch fails without a retry or relaxed-validation mode. The deferred
resolver now includes profile inference limits instead of silently replacing them with defaults.
Completed replay needs neither profile resolution nor credential/model access.

Credential precedence, exact-URL secure-store matching, HTTPS/loopback restrictions, budget limits,
model-call recovery, and capability permissions are unchanged. No token is stored in profile JSON.

## Usage

Use an ephemeral environment credential or `--token-stdin`, never an argument containing a key:

```sh
xgeny model setup --name fast --base-url https://api.deepseek.com/v1 \
  --model deepseek-chat --response-format json_object --thinking disabled
xgeny model check --profile fast --compatibility
xgeny run --profile fast --allow-dir . --allow-remote-model-egress 'Inspect this workspace'
xgeny resume RUN_ID --profile fast --allow-dir . --allow-remote-model-egress
```

The endpoint/model above is an example, not a default or a tested live-service claim. A caller must
select an advertised model and validate its compatibility. Catalog checks are one GET, optional
compatibility is one POST, and no automatic paid retries are added.

## Verification

Offline tests cover old profiles, enum rejection, precedence, both response transports, two thinking
settings, setup/run body parity, unchanged production/probe digests, resume with non-default limits,
and changed-profile rejection. Existing completed-replay tests protect the no-model-access path.
Local HTTP fixtures are not live DeepSeek quality or availability evidence.
