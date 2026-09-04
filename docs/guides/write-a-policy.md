# Write a policy

A policy decides which proposed actions are allowed, which need a human, and
who that human is. This guide writes one for a support department and tests it
against history before applying it. The grammar and every context field are in
the [policy language reference](../reference/policy-language.md).

## 1. Start from the questions

Ask three things of every action kind the department's bundles propose: is it
ever automatic, who must approve it when it is not, and how long may that take.

## 2. Write the rules in order

```
policy "support-default"

# Refunds above a small amount need a lead, quickly.
require approval when
  action.kind == "refund.issue" and action.amount >= 50
  -> approver: role("support_lead"), sla: 2h, escalate_to: reporting_line

# Routing is cheap to undo, so it is automatic unless the remit is only supervised.
require approval when
  action.kind == "ticket.route" and run.remit.autonomy == "supervised"
  -> approver: run.requested_by.manager, sla: 24h

# Nothing touches personal data without the grant.
deny when
  action.touches_data_class("personal") and not run.remit.grants("pii")

allow when
  action.kind in ["ticket.read", "ticket.comment"]
```

`deny` always wins, then the first matching `require approval`, then `allow`,
then the default (automatic only for an `autonomous` remit; otherwise a write
to a system of record needs the requester's manager).

## 3. Check it parses

```bash
kernos policy check support-default.policy
```

Errors carry a line and a column.

## 4. Test it against history

Export the actions the department's runs proposed recently, and see which
decisions the new policy would change:

```bash
kernos run actions --department support --since 30d > actions.jsonl
kernos policy test --a support-default@1 --b support-default.policy --corpus actions.jsonl
```

The report lists every row whose outcome changes, with both decisions and the
rule that produced each. Outcome means the decision and, for an approval, the
gate it creates: raising a threshold flips the rows between the old and new
amounts, and moving an approval from one role to another flips those rows too,
even though both still require approval. If a change flips anything you did not
intend, the policy is wrong, not the history.

## 5. Apply a new version

```bash
kernos policy apply support-default.policy --name support-default --version 2
```

Remits name the policy set; runs pick up version 2 when their remit is issued
after it is applied. Policy versions are never edited in place, and every
decision on every run records the version that made it, so replay months later
uses the rule that actually applied.

## Habits

- Put the amount thresholds in the policy, never in a prompt.
- Prefer `role("...")` approvers; `run.requested_by.manager` is right when the
  decision is about the requester's own work.
- Give every gate an SLA and an escalation. A gate without an SLA is a run that
  can wait forever.
- Keep `deny` rules few and absolute. Use grants for the exceptions.
