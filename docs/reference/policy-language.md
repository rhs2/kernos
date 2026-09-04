# Policy language reference

A policy is a versioned text artefact evaluated against a proposed action and
its run context. It is declarative and total: no loops, no assignment, no side
effects, and every input produces a decision.

## Example

```
policy "finance-default"

require approval when
  action.kind == "payment.issue" and action.amount >= 5000
  -> approver: role("finance_admin"), sla: 4h, escalate_to: reporting_line

require approval when
  action.writes_to_system_of_record and run.remit.autonomy == "supervised"
  -> approver: run.requested_by.manager, sla: 24h

require approval when
  action.kind == "code.merge" and action.touches_path("infra/**")
  -> approver: role("platform_owner"), sla: 8h

deny when
  action.touches_data_class("personal") and not run.remit.grants("pii")

allow when
  action.kind == "invoice.read"
```

## Grammar

```
policy      := header? rule*
header      := 'policy' STRING
rule        := ('deny' | 'allow' | 'require' 'approval') 'when' expr ( '->' approval )?
approval    := 'approver' ':' approver ( ',' 'sla' ':' DURATION )? ( ',' 'escalate_to' ':' escalate )?
approver    := 'role' '(' STRING ')' | 'user' '(' STRING ')' | path
escalate    := 'reporting_line' | 'role' '(' STRING ')' | 'user' '(' STRING ')' | path
expr        := or
or          := and ( 'or' and )*
and         := not ( 'and' not )*
not         := 'not' not | cmp
cmp         := sum ( ( '==' | '!=' | '<' | '<=' | '>' | '>=' | 'in' ) sum )?
sum         := unary ( ( '+' | '-' ) unary )*
unary       := '-' unary | primary
primary     := NUMBER | STRING | 'true' | 'false' | 'null' | list | call | path | '(' expr ')'
list        := '[' ( expr ( ',' expr )* )? ']'
call        := path '(' ( expr ( ',' expr )* )? ')'
path        := IDENT ( '.' IDENT )*
DURATION    := NUMBER ('s' | 'm' | 'h' | 'd')
comment     := '#' to end of line
```

Reserved words: `policy allow deny require approval when approver sla
escalate_to and or not in true false null role user reporting_line`.

## Values

Types: number, string, bool, null, list. Comparisons across types are `false`
except `==` and `!=`. `in` tests list membership. Arithmetic on non-numbers is
`null`. A missing path is `null`. `and` and `or` short-circuit and treat `null`
as `false`.

## Context

```
action.kind                        string, e.g. "payment.issue"
action.amount, action.currency     number | null, string | null
action.writes_to_system_of_record  bool
action.target                      string | null
action.data_classes, action.paths  lists of string
action.summary, action.idempotency_key
action.touches_path(glob)          bool, with * and ** 
action.touches_data_class(name)    bool
run.id, run.department, run.workflow
run.bundle.name, run.bundle.version
run.remit.autonomy                 string
run.remit.grants(name)             bool
run.remit.tools, run.remit.scopes  lists of string
run.requested_by.id, .role, .manager
```

## Decision

Rules are evaluated in order. A matching `deny` wins. Otherwise the first
matching `require approval` rule supplies the approver, SLA and escalation.
Otherwise a matching `allow` allows. Otherwise the default: `allow` for an
`autonomous` remit; for any other remit, a write to a system of record requires
`run.requested_by.manager` within 24 hours escalating up the reporting line,
and a non-write is allowed.

Several policies on one remit are evaluated as if concatenated in order. The
recorded rule id is `<policy>@<version>#<rule index>` or `default`.

## Approvers

`role("x")` accepts any actor with that role. `user("id")` accepts that user.
`run.requested_by.manager` resolves to the manager's id, or to `role("admin")`
with `"fallback": true` when there is none. `reporting_line` escalates to the
manager of the current approver when it is a user, else to `role("admin")`.
The directory (`KERNOS_DATA/directory.json`) supplies roles and managers:

```json
{"users": {"u-tom": {"role": "finance_admin", "manager": "u-cfo"}}}
```

## Testing

`kernos policy test` and `POST /v1/policies/test` take a corpus of `{action,
run}` contexts and report every decision that flips between two versions,
with the rule that produced each. A change with unreviewed flips is not applied.

## Approval records

`approval.requested` (approver, SLA, escalation, due time), optionally
`approval.escalated`, and `approval.decided` (actor with id and role, decision,
reason of at least three characters). Everything needed to answer who allowed
what, when, and why.
