# ADR-0041: Bind recovery approval to an expected journal head

- Date: 2026-09-16
- Status: Proposed
- Extends: ADR-0040
- Protocol / journal / SQLite schema changes: none

## Decision

An external host can approve discarding an unresolved call using an inspected
journal head. A call ID alone does not bind that decision to the inspected state:
ordinary recovery can mark Reserved as Unknown while keeping the same call ID.

Add optional `--expected-journal-head sha256:HEX` to
`recover RUN_ID --discard-model-call CALL_ID`. Compare it with the verified
journal head **inside the exclusive Run lease, before opening writable state**.
The existing writable-reopen verification and Core append CAS remain in place.
A mismatch, including malformed input, returns the redacted configuration error
(exit 64) without appending or returning the supplied value. The flag without a
discard action is rejected. Never fall back to an unconditional discard.

Keep the existing manual exact-call-ID form compatible. External hosts binding
approval to a snapshot must use the guarded form. An older binary rejecting the
flag is an unsupported capability, not permission to omit the condition.

This is a local compare-and-set guard, not authentication, an approval store, or
a resume operation. No budget is refunded and no provider/tool is invoked.
Successful output retains format version 1 and reports the committed head.
Output loss after commit still requires inspection; a repeated old-head discard
fails rather than creating a second settlement.

## Verification

Public process tests must prove same call ID plus changed head is rejected,
correct current head succeeds exactly once, malformed heads and held leases do
not mutate, and consumed reservations and verified effects remain unchanged.
These tests use loopback fixtures, not an actual ML task or production recovery.
