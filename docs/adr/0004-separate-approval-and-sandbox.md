# 0004. Separate approval and sandbox, no silent escalation

- Status: Accepted
- Date: 2026-10-12

## Context

Providers expose permission controls differently. Some separate "when to ask" from "what may be touched"; others mix them into a single mode. Failover will move work between providers, and a careless mapping could run the next provider with more permission than the user granted the first one. Not every provider can enforce every restriction.

## Decision

- `PermissionPolicy` has independent dimensions: `ApprovalMode` (when to ask) and `SandboxPolicy` (`FilesystemAccess`, `NetworkAccess`). Each dimension is totally ordered from restrictive to permissive with an explicit rank.
- A `Session` has a user-chosen `permission_ceiling`. Every run must be within it in every dimension; being stricter in one dimension never compensates for being looser in another.
- Adapters report the *effective* policy honestly via `PermissionMapping`; unenforceable restrictions are reported as their permissive value.
- `validate_mapping` produces `ValidatedPermissions` (private fields) only when the effective policy is within the requested one. `LaunchRequest` requires `ValidatedPermissions`.
- Automatic decisions (`PermissionResolution::by_policy`) may only reject or cancel. Allowing requires `by_user`.
- There is no API to raise a ceiling.

## Consequences

- Silent escalation, including during future provider switching, is a type or validation error rather than a review finding.
- A provider that cannot enforce a requested restriction cannot be launched with that restriction; the user must explicitly accept a wider ceiling for a new session.
- Provider-internal approvals ("allow always") do not carry across providers and are listed as non-transferable.
- An explicit, audited escalation flow (user consent recorded in the session) can be added later without weakening these rules.
