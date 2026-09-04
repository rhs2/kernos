# The Halcyon example

Every example, fixture and test in Kernos uses **Halcyon Provisions**, a fictional
food distributor. No real company, product, person or system name appears
anywhere in the repository, and every example is generated rather than
extracted. The reference bundle lives in `bundles/reference/halcyon/`.

## The bundle

`halcyon.finance.invoice_intake` takes a supplier invoice from text to a posted
ledger entry. Its main workflow, `intake`, has four steps:

| Step | Kind | What it does |
|---|---|---|
| `extract` | model, standard tier, low effort | Reads the invoice text into vendor, invoice id, total, currency and description, validated against a schema |
| `code` | model, cheap tier | Assigns a general-ledger account with a confidence; below 0.7 it escalates to the standard tier |
| `propose_payment` | action | Proposes `payment.issue` for the extracted total. This is the gate |
| `post` | tool | Calls `ledger.post_entry` with the invoice id as idempotency key, and declares `ledger.void_entry` as its compensation |

Four more workflows exist for the acceptance suite: `intake_slow` (a deliberate
four-second step before posting, to kill a worker inside it), `double_post_then_fail`
(two posts and then a failure, to prove compensation runs in reverse),
`http_first` (a call through the HTTP connector, to exercise a canary) and
`poison` (a step that always fails deterministically, to prove quarantine).

## The policy

`finance-default` requires a `finance_admin` to approve any payment at or above
5,000 within four hours, escalating up the reporting line; requires the
requester's manager to approve any write to a system of record when the remit
is `supervised`; requires a platform owner for code merges that touch
infrastructure; denies anything touching personal data unless the remit grants
`pii`; and allows plain invoice reads. `finance-test` is the same with a
five-second SLA so escalation can be tested, and `finance-default-10k` raises
the threshold, which the policy test uses to prove that exactly the actions
between 5,000 and 10,000 flip.

## The people

`directory.json` names three fictional users: `u-ana`, an accounts-payable
clerk who requests runs; `u-tom`, a finance admin and her manager; `u-cfo`,
Tom's manager, who receives escalations.

## The connectors

`gateway.json` configures a `ledger` connector over SQLite with three named
statements and a contract for each, a probe that looks up a vendor, and an
`http` connector limited to `127.0.0.1`. The database path and data directory
come from the environment; the config file holds no path or secret of its own.

## The golden set and the corpus

`golden/` holds six invoice cases with expected vendors and accounts, used by
`kernos-eval` to score a bundle version. `corpus/actions.jsonl` holds twelve
historical actions used by the policy test to report which decisions a policy
change would flip.
