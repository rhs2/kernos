# Remits

A remit is the signed capability set a run carries. The control plane issues it
before the run starts, the gateway verifies it on every tool call, and nothing
in a prompt can widen it. It is the most important decision in Kernos: every
prompt-injection defence that relies on the model behaving well eventually
fails, and a capability check does not care what the model was persuaded to
attempt.

## What a remit names

| Field | Meaning |
|---|---|
| `tools` | The tool identifiers the run may call, as exact ids or `connector.*` patterns |
| `scopes` | The data it may touch: `sql:table:invoices`, `http:host:api.example`, `fs:path:/exports`, with a trailing `*` for prefixes |
| `grants` | Data classes the run may see un-redacted, such as `pii` |
| `spend` | A ceiling in tokens and currency, enforced by the kernel's budget |
| `autonomy` | `observe`, `propose`, `supervised` or `autonomous` |
| `exp` | An expiry. Remits are short-lived by design |
| `policy_set` | The policies that gate this run's actions |
| `requested_by` | The person the run acts for, with role and manager, for approvals |

The token is `krt1.<payload>.<signature>.<key id>`, Ed25519-signed by the
control plane. The gateway fetches the public key once and verifies every call
locally, so a compromised worker cannot mint a remit and a persuaded model
cannot claim one.

## Scope comes from the arguments

A remit lists scopes; a call does not declare which scope it is using. The
connector derives the scope from the call's arguments (the tables a statement
touches, the host in a URL, the directory of a path) and the gateway checks the
derived scope against the remit. A model that asks to read `customers` when the
remit says `invoices` is refused, whatever it says about itself.

## Autonomy is enforced twice

The gateway enforces the mechanical part: an `observe` or `propose` remit
cannot call any tool that writes. Policy enforces the judgement part: the reference policies
require approval for writes under `supervised`, and the default rule requires
approval for any write to a system of record unless the remit is `autonomous`.

## Delegation narrows, never widens

A run may spawn sub-runs, and a sub-run gets a child remit derived from the
parent. Every field must be a subset: each child tool pattern must be matched
by a parent pattern, scopes and grants must be subsets, spend and expiry less
or equal, autonomy no higher, and the policy set may only grow. A request that
widens any field is refused with the field named. There is no other path to a
child remit, so fan-out cannot escalate privilege.

## Refusals are events

A call outside the remit is refused at the boundary with a stable reason
(`tool_not_in_remit`, `scope_not_granted`, `autonomy_too_low`, `remit_expired`,
`signature_invalid`, `remit_run_mismatch`) and the gateway records a
`tool.refused` event on the run. Refusals are counted in metrics and are
alertable like any other signal: a run that keeps trying to leave its remit is
worth a look, and the log says exactly what it tried.

See the [remit token reference](../reference/remit.md) for the exact payload,
verification order and narrowing rules.
