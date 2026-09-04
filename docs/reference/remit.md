# Remit token reference

Format `krt1`.

```
krt1.<base64url(canonical JSON payload)>.<base64url(ed25519 signature)>.<key_id>
```

The signature is Ed25519 over the exact payload bytes that were encoded.
`key_id` names the control-plane key, published at `GET /v1/keys`. base64url
carries no padding. Unknown prefixes are refused.

## Payload

```json
{
  "rid": "rem_01j6zq…", "parent": "rem_01j6zp…", "run": "run_01j6zr…",
  "iss": "key_01j6z…", "iat": 1757000000, "nbf": 1757000000, "exp": 1757086400,
  "tools": ["ledger.*", "http.get"],
  "scopes": ["sql:table:invoices", "http:host:api.halcyon.example"],
  "grants": ["pii"],
  "spend": {"tokens": 200000, "usd": 2.0},
  "autonomy": "supervised",
  "policy_set": ["finance-default"],
  "requested_by": {"id": "u-ana", "role": "ap_clerk", "manager": "u-tom"}
}
```

`parent` and `run` are optional. A remit with `run` set is bound to that run;
it is what workers receive in a lease, and the gateway refuses a call whose
`run_id` differs (`remit_run_mismatch`).

## Patterns

Tool ids are `connector.operation`; a pattern is an exact id or `connector.*`.
Scopes are `system:kind:key`; a pattern may end in `*` to match a prefix.
Matching is exact-or-glob, case sensitive, without regular expressions.

## Autonomy

Ordered `observe` < `propose` < `supervised` < `autonomous`. The gateway
refuses any tool with `writes: true` under `observe` and `propose`: those levels
may read and may propose actions, never write. Policy decides the rest:
the default rule requires the requester's manager to approve a write to a
system of record unless the remit is `autonomous`.

## Verification order at the gateway

1. Prefix, four parts, base64url decodes.
2. Signature valid for `key_id`.
3. `nbf <= now < exp`.
4. `run`, if set, equals the call's `run_id`.
5. The tool matches a `tools` pattern.
6. The scope the connector derives from the arguments matches a `scopes`
   pattern (a connector with no derivation requires the literal `<connector>:*`).
7. The operation's `writes` flag is allowed by `autonomy`.

Any failure is a `403` refusal with the reason, a `tool.refused` event on the
run, and a metric. Spend is enforced by the kernel's budget, not the gateway.

## Delegation narrows

`derive(parent, request)` succeeds only when every field is a subset:

| Field | Rule |
|---|---|
| `tools`, `scopes` | Each child pattern must be matched by some parent pattern |
| `grants` | Subset |
| `spend.tokens`, `spend.usd` | Less than or equal |
| `autonomy` | Less than or equal in the order above |
| `exp` / `nbf` | Earlier / later |
| `policy_set` | May grow; removing a policy is a widening |

A violation is `422 remit_widens` naming the field. A run may spawn sub-runs
only through `derive`.

## Grants

Grants name data classes the run may see un-redacted. Policy reads them
(`run.remit.grants("pii")`) and the worker's data boundary redacts content whose
class is not granted before it reaches a model provider, recording the
redaction as a `note` event.
