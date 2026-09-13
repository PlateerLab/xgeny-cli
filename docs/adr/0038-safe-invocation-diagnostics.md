# ADR-0038: Safe invocation rejection diagnostics

Status: Accepted

## Problem

Distinct argument-schema and resource-resolution failures currently collapse into
`proposal_rejected.invocation_invalid`. Hosts cannot safely infer which field the
model should correct. Raw validation errors may contain input values, paths,
schema content and dynamically supplied property names, so printing them is unsafe.

## Decision

Retain the coarse verdict, exit code, journal settlement and no-retry behavior.
For a newly rejected proposal in headless `run`/`resume`, optionally emit one
additional bounded stderr line (the REPL display is unchanged):

```text
XGENY_INVOCATION_DIAGNOSTIC run_id=RUN version=1 category=CATEGORY field=FIELD
```

Both CATEGORY and FIELD are fixed allowlisted constants, never error formatting.
Categories: `schema_required`, `schema_type`, `schema_additional_property`,
`schema_min_length`, `schema_pattern`, `schema_one_of`, `schema_other`,
`resource_resolution`. Fields: `path`, `content`, `expectedDigest`, `other`.
Only exact top-level pointers and root-level required properties can disclose the
three known field names; nested/dynamic names become `other`. Return one finding,
not the raw validator error, input, schema, actual path, exception or model text.

This diagnostic is ephemeral terminal metadata, not new durable evidence or
execution authority. Resume cannot reconstruct missing old diagnostics. Hosts
must bind the line to the same native terminal run, preserve their request/binary
identity, and independently verify the journal before considering any recovery.
Unknown categories/versions must fail closed. No automatic retry or instruction
to the model is added by this change. Journal schema 8/protocol v0.1 is unchanged.
The first schema finding only is projected; this is not a complete error list.
Normalization-instability rejections retain the generic verdict without a diagnostic.
The provider's nested planner-arguments grammar is not changed by this decision.

## Verification

Runtime tests cover redaction and category/field selection. Public CLI loopback
tests must distinguish malformed arguments from resource failures while retaining
the original coarse verdict and zero effects. Host integration checks are separate
from live-model competence and ML training; no live provider is needed.
