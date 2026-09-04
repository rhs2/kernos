# Policy and approvals

Which actions need a human is a declarative rule over the action and its
context, evaluated by the engine. Approval is a typed record with an actor, a
reason and a timestamp, not a message in a chat channel.

## A policy

```
policy "finance-default"

require approval when
  action.kind == "payment.issue" and action.amount >= 5000
  -> approver: role("finance_admin"), sla: 4h, escalate_to: reporting_line

require approval when
  action.writes_to_system_of_record and run.remit.autonomy == "supervised"
  -> approver: run.requested_by.manager, sla: 24h

deny when
  action.touches_data_class("personal") and not run.remit.grants("pii")

allow when
  action.kind == "invoice.read"
```

Rules see the proposed action (kind, amount, target, data classes, paths, and
whether it writes to a system of record) and the run (its remit, its bundle,
who requested it). The language has comparisons, arithmetic, `and`, `or`,
`not`, `in`, lists and a few functions such as `touches_path("infra/**")`. It
has no loops, no assignment and no side effects, and every input produces a
decision.

## The decision

Rules are evaluated in order. A matching `deny` wins. Otherwise the first
matching `require approval` rule decides who approves, by when, and where the
request escalates. Otherwise a matching `allow` allows. With no match, the
default applies: an `autonomous` remit is allowed, and any other remit needs
the requester's manager to approve a write to a system of record.

Several policies on one remit are evaluated as if concatenated in order.

## What happens at a gate

When a step proposes an action and the decision is `approval_required`, the
kernel records the decision and the request, parks the run, and releases the
worker. A run waiting three days for a decision costs nothing. The approver
decides in the console, through the API, or from the command line; the
decision is recorded with actor, timestamp and reason, and the step resumes
from its start with the approval attached, so the same proposal is now allowed.
A rejection fails the run and nothing is written.

Timeouts escalate rather than expire: when the SLA passes, the request moves up
the reporting line and the log says so. A second expiry parks the run for a
human with a clear cause.

## Policies are tested artefacts

A policy is versioned. Before a new version is applied it is run against a
corpus of historical actions, and every decision that would flip is reported.
Raising a payment threshold from 5,000 to 10,000 should flip exactly the
actions between those amounts; if it flips anything else, the change is wrong.

## Reconstructing an approval

Every approval leaves three events on the run: `approval.requested` (who was
asked, by when, where it escalates), optionally `approval.escalated`, and
`approval.decided` (who decided, when, why). "Who allowed this payment, and on
what basis" is a query, not an investigation.

See the [policy language reference](../reference/policy-language.md) for the
grammar, the context fields and the approver resolution rules.
