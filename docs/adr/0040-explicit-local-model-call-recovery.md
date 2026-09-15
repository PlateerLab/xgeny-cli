# ADR-0040: Explicit local model-call recovery

- Date: 2026-09-15
- Status: Proposed
- Extends: ADR-0016, ADR-0023
- Protocol / journal / SQLite schema changes: none

## Problem

A killed CLI can leave a Reserved model call; ordinary resume classifies it as
Unknown/Interrupted and refuses implicit retry. Core already provides
`AgentLoop::abandon_model_call`, but an external harness cannot invoke that safe
operation through the public binary. Editing SQLite, declaring success from a
partial output, or starting a replacement Run would evade the durable boundary.

## Decision

Expose a separate offline command:

```text
xgeny recover RUN_ID
xgeny recover RUN_ID --discard-model-call EXACT_CALL_ID
```

The first form verifies the manifest and journal under the exclusive Run lease
and prints a bounded JSON report without changing journal state. It includes
the active call's Core-generated ID and Reserved/Unknown status, consumed and
maximum possible-send counters, settled/unknown counters, and whether effect
recovery is still required. It is not a transcript or provider diagnostic dump.

The second form is explicit operator/host authority to stop accepting the named
unresolved call's response. Validate exact active identity before opening the
store writable, recheck the verified state after reopening, and use the existing
Core API/CAS to append one RecoveryDiscarded settlement. Reserved and Unknown
are both eligible. Do not invent an Unknown reason for a directly discarded
reservation. A wrong/stale ID, a completed Run, or no active call fails closed.
Repeating a successful command must not settle a later call or append again.

Discard is NOT evidence that no request was sent or billed. No reserved slot,
accepted-turn budget, or effect record is refunded/reset. A late old response
cannot be applied. Neither form resolves credentials, reads the workspace,
contacts a provider, invokes tools, completes the Run, nor resumes it. A separate
ordinary resume requires the original workspace identity, catalogs, model
profile, approvals and remaining original budget. Unknown effects remain blocked.

JSON output acknowledges the inspected/committed state, not provider settlement.
If output delivery fails after commit, inspect again; do not blindly retry with a
different ID. There is no wildcard, automatic discard, budget override, or Run
replacement command. Default resume behavior and existing exit codes stay intact.

## Verification

Exercise the public binary against local fixture providers: inspection leaves the
journal unchanged, discard spends no new reservation, wrong/stale IDs and held
leases cannot mutate, a verified tool result is not replayed after discard and
explicit resume, consumed reservations remain consumed, and no credential/profile
configuration is needed for recovery. Retain Core's stale-response and exhausted
budget regression tests. Real-provider recovery and missing-workspace restoration
are separate harness acceptance work, not proven by this CLI change.
