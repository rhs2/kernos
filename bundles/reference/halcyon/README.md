# Halcyon Provisions: reference bundle `halcyon.finance.invoice_intake`

Halcyon Provisions is a fictional food distributor. It buys from three fictional
suppliers (Northwind Dairy, Harbor Greens and Millstone Bakery), pays them from
a small ledger, and has three fictional people in finance: Ana (`u-ana`, an
accounts payable clerk reporting to Tom), Tom (`u-tom`, a finance administrator
reporting to the CFO) and the CFO (`u-cfo`). Nothing in this directory is drawn
from a real company; every invoice, vendor and person is generated.

This bundle is the reference used by the documentation, the quickstart and the
acceptance suite (`tests/acceptance/run.py`). It exercises the finance pattern
of the finance pattern: approval thresholds, ledger writes and compensation.

## Files

| Path | What it is |
|---|---|
| `bundle.json` | The bundle (format `kernos.bundle/1`, version 1.0.0, department finance) |
| `policies/finance-default.policy` | The reference policy of https://rhs2.github.io/kernos/reference/policy-language/, unchanged |
| `policies/finance-test.policy` | Same rules, named `finance-test`, with a 5 second SLA on the payment rule (acceptance escalation scenario) |
| `policies/finance-default-10k.policy` | Same rules with the payment threshold at 10000 (acceptance policy flip scenario) |
| `directory.json` | Reporting lines for approver resolution; the kernel reads it from `KERNOS_DATA/directory.json` |
| `ledger.sql` | SQLite schema and seed vendors for the ledger the gateway's `ledger` connector writes to |
| `gateway.json` | Gateway configuration for the acceptance ports (17402 gateway, 17401 kernel, 17499 fake upstream) |
| `golden/` | An evaluation golden set of six invoices for the `intake` workflow (`kernos-eval run --golden golden/`) |
| `corpus/actions.jsonl` | Twelve historical payment actions for `kernos policy test` |

## Tools the bundle declares

| Tool | Writes | Used by |
|---|---|---|
| `ledger.post_entry` | yes | `post`, `post_a`, `post_b` |
| `ledger.void_entry` | yes | compensations of the posting steps |
| `ledger.lookup_vendor` | no | the gateway's ledger canary probe |
| `http.get` | no | `fetch` in `http_first` |
| `test.slow` | no | `wait` in `intake_slow` (exists only when the gateway runs with `KERNOS_GATEWAY_TEST_TOOLS=1`) |
| `test.fail` | no | `boom` in `double_post_then_fail` and `poison` (same condition) |

## Prompts

`extract` reads the vendor, invoice number, total and currency out of the
invoice text; `code` assigns a general-ledger account and reports a confidence.
Both system texts are frozen (no templates) so they form a stable cache prefix;
the user texts are templated from the run input and earlier outputs.

The `mock` section lets the bundle run end to end without a model: `extract`
answers with Northwind Dairy and the input's total, `code` answers account
`5100` with confidence `0.93`.

## Workflows

Every workflow takes the same input:

```json
{"invoice_id": "INV-1001", "text": "...the invoice text...", "total": 1250.0,
 "accounts": ["5100", "5200", "5300", "6100"], "probe_url": "http://127.0.0.1:17499/probe"}
```

`invoice_id`, `text` and `total` are required. `accounts` is optional in the
schema but the `code` prompt renders it, so pass it whenever the workflow reaches
the `code` step. `probe_url` is only read by `http_first`.

| Workflow | Steps (in order) | Purpose |
|---|---|---|
| `intake` | `extract` (model, standard, low), `code` (model, cheap, escalates to standard below confidence 0.7), `propose_payment` (action `payment.issue`), `post` (tool `ledger.post_entry`) | The real workflow: one invoice from text to posted entry |
| `intake_slow` | `extract`, `code`, `propose_payment`, `wait` (tool `test.slow`, 4000 ms), `post` | `intake` with a four second step before the write, so a worker can be killed inside it (acceptance A1) |
| `double_post_then_fail` | `extract`, `code`, `post_a` (key `{{input.invoice_id}}-a`), `post_b` (key `{{input.invoice_id}}-b`), `boom` (tool `test.fail`) | Two completed writes followed by a deterministic failure, to show abandon and compensation (A3) |
| `http_first` | `fetch` (tool `http.get` on `{{input.probe_url}}`), `extract`, `code`, `post` | A read through the http connector before the ledger write (A5, A8) |
| `poison` | `boom` (tool `test.fail`) | Deterministic failure, retried three times and quarantined (A9) |

The `propose_payment` action carries the extracted total; under
`finance-default` an amount of 5000 or more needs a `finance_admin` approval
with a four hour SLA that escalates up the reporting line. Every posting step
uses the invoice id as its idempotency key and declares a `ledger.void_entry`
compensation that voids its own entry if the run is abandoned.

## Running it

```
kernos keys generate --out publisher
kernos keys trust publisher.pub
kernos bundle sign bundle.json --key publisher.key --out bundle.sig.json
kernos bundle apply bundle.json --sig bundle.sig.json
kernos policy apply policies/finance-default.policy --name finance-default --version 1
kernos remit issue --tools "ledger.*,http.get,test.*" --scopes "sql:table:ledger_entries,sql:table:vendors,http:host:127.0.0.1,test:*" --usd 2 --autonomy autonomous --ttl 24h
kernos run start --bundle halcyon.finance.invoice_intake@1.0.0 --workflow intake --input input.json --remit rem_...
```

No signature file ships in the repository: the acceptance suite generates a
throwaway publisher key each run, trusts it, and signs the bundle with it.

## Validation

`python3 tests/acceptance/validate_bundle.py` checks `bundle.json` against the
rules of https://rhs2.github.io/kernos/reference/bundle-format/ and exits non-zero on any offence.
