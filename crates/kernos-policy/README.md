# kernos-policy

The policy language of [Kernos](https://github.com/rhs2/kernos): a small, declarative
rule language evaluated against a proposed agent action and its run context.

```text
policy "finance-default"

require approval when
  action.kind == "payment.issue" and action.amount >= 5000
  -> approver: role("finance_admin"), sla: 4h, escalate_to: reporting_line

deny when
  action.touches_data_class("personal") and not run.remit.grants("pii")
```

The crate ships a hand-written lexer and recursive-descent parser (errors carry
line and column), a total evaluator over a JSON context, the decision procedure
(deny over approval over allow, then a default rule keyed on the remit's autonomy),
approver resolution with a reporting-line directory, and `test_corpus`, which
reports every decision that flips between two policy versions.

It has no dependency on the rest of Kernos and is usable standalone.

Licensed under Apache-2.0.
