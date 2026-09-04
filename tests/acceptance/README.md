# Kernos acceptance suite

The end-to-end suite: thirteen scenarios, A1 to
A13, run against a real kernel, gateway and mock-provider workers on test ports,
using the Halcyon Provisions reference bundle in `bundles/reference/halcyon/`.
No API key and no network are needed.

## What is here

| Path | Purpose |
|---|---|
| `run.py` | The orchestrator: starts every component, loads the bundle and policies, runs the scenarios, reports |
| `fixtures/fake_upstream.py` | The fake upstream on port 17499 that the gateway's `http` connector probes (scenario A8) |
| `ts_smoke.mjs` | Scenario A13: the built `@kernos/sdk` starts a run, follows it, lists approvals and replays |
| `validate_bundle.py` | Static validator for `bundle.json` against https://rhs2.github.io/kernos/reference/bundle-format/; standard library only |
| `NOTES.md` | Every assumption about kernel, gateway and worker behaviour the integrator must confirm |

The ledger schema, policies and directory the suite needs live with the bundle
(`bundles/reference/halcyon/ledger.sql`, `policies/`, `directory.json`,
`gateway.json`); the signing key is generated fresh on every run.

## Running

From the repository root, once the components are built:

```
make accept
```

is what the integrator wires. The direct invocation it wraps is:

```
python3 tests/acceptance/run.py
```

`run.py` builds nothing. It needs:

| What | Where it looks | Override |
|---|---|---|
| Kernel binary | `target/release/kernos` | `KERNOS_BIN=/path/to/kernos` |
| Gateway binary | `gateway/bin/kernos-gateway` | `KERNOS_GATEWAY_BIN=/path/to/kernos-gateway` |
| Worker command | `kernos-worker` on PATH | `KERNOS_WORKER_CMD="python3 -m kernos.worker"` (shell-split) |
| Eval command | `kernos-eval` on PATH (A12) | `KERNOS_EVAL_CMD="python3 -m kernos.eval"` |
| Python 3.10+ with `httpx` | the interpreter running `run.py` | install the Python SDK (`pip install -e sdk/python`) |
| `node` and `sdk/typescript/dist` (A13) | PATH and the local build | `cd sdk/typescript && npm install && npm run build` |

Flags:

```
--only A6          run one scenario (or several: --only A1,A2,A11)
--keep             keep the temporary work directory (data dirs, ledger, logs)
--fail-fast        stop at the first failing scenario (09 semantics; the default runs every selected scenario)
--list             print the scenarios and exit
```

Ports: kernel 17401, gateway 17402, fake upstream 17499, and 17411 for the second
kernel that A2 starts on a tampered copy of the data directory. The suite refuses
to start if any of them is busy. Kernel settings: `KERNOS_LEASE_TTL=3`,
`KERNOS_SWEEP_MS=500`, `KERNOS_APPROVAL_SWEEP_MS=500`; gateway:
`KERNOS_GATEWAY_TEST_TOOLS=1`, canary interval 2 s, quarantine after 2 failures.

Everything lives in a temporary directory (`kernos-accept-*` under the system
temp dir) that is deleted at the end unless `--keep` is given: `kernel-data/`,
`gateway-data/`, `halcyon-ledger.db`, the publisher keys, the signature file,
eval reports and `logs/` (one file per component, `cli.log`, `eval.log`,
`ts_smoke.log`).

## Output

One line per scenario, then a summary:

```
A1   PASS    14.2s  A run killed mid-step resumes and finishes (phase 1).
A2   PASS     3.1s  Replay reproduces every decision; a tampered byte breaks the chain (phase 1).
...
summary: 13 passed, 0 failed, 96.4s total
```

Exit code 0 when every selected scenario passed, 1 on any failure, 2 when the
harness could not start (missing binary, busy port, CLI error). On a failure the
message is printed under the FAIL line, followed by every event of the runs that
scenario started and the last 100 lines of every component log. Lines starting
with `note:` are observations that are not failures but that the integrator
should read (for example which escalation target the kernel chose in A6).

## Scenarios and how to run each alone

Each scenario is self-contained except A2, which reuses the run of A1 when both
are selected and otherwise starts its own `intake` run.

| Id | Proves | Bundle workflow | Alone |
|---|---|---|---|
| A1 | A worker killed inside `test.slow` loses its lease; a second worker finishes the run; one ledger row | `intake_slow` | `python3 tests/acceptance/run.py --only A1` |
| A2 | Replay agrees with the log; a flipped byte in a copied database makes `chain_valid` false with the seq named | A1's run or `intake` | `--only A2` |
| A3 | Abandon after two posts schedules two compensations in reverse order; both rows voided | `double_post_then_fail` | `--only A3` |
| A4 | `tool_not_in_remit`, `scope_not_granted`, `autonomy_too_low` refusals are logged and deterministic | `intake`, `double_post_then_fail` | `--only A4` |
| A5 | Derive widening is 422 `remit_widens` naming the field; a `propose` child reads but cannot write | `http_first` | `--only A5` |
| A6 | 7250 parks for `role:finance_admin`; wrong approver 403; approval resumes with `approved:<id>`; reconstruction; 5 s escalation; rejection | `intake` | `--only A6` |
| A7 | `model.called` for extract and code, stable `prefix_hash`, schema-valid outputs, usage sums, confidence escalation, refusal park | `intake` | `--only A7` |
| A8 | Renaming the upstream field quarantines `http` within 6 s, writes a repair file, parks a new run, release and resume complete it | `http_first` | `--only A8` |
| A9 | `test.fail` is quarantined after three attempts; no fourth lease | `poison` | `--only A9` |
| A10 | usd 0.0005 budget: soft threshold then `pacing: true`, then exceeded and parked; no further leases | `intake` (steps driven by the orchestrator) | `--only A10` |
| A11 | Untrusted key and tampered byte are 422; a broken policy is 422 with line and column; the flip test reports exactly the corpus rows between 5000 and 10000 | none | `--only A11` |
| A12 | `kernos-eval run` scores 1.0 on the golden set, 0.2 confidence lowers it, `gate` exits 1 then 0 | `intake` via kernos-eval | `--only A12` |
| A13 | `node tests/acceptance/ts_smoke.mjs` round-trips a run with the built TypeScript client | `intake` | `--only A13` |

The TypeScript smoke test can also be run by hand against any kernel that has
the reference bundle applied:

```
KERNOS_URL=http://127.0.0.1:17401 node tests/acceptance/ts_smoke.mjs
```

## Validating the bundle

```
python3 tests/acceptance/validate_bundle.py                       # the reference bundle
python3 tests/acceptance/validate_bundle.py path/to/other.json    # any bundle
```

Prints `OK` or one `ERROR <json path>: <message>` line per offence; exit 1 on
any error.

## Fake upstream by hand

```
python3 tests/acceptance/fixtures/fake_upstream.py --port 17499
curl http://127.0.0.1:17499/probe                                 # {"ok": true}
curl -X POST -d '{"mode":"renamed"}' http://127.0.0.1:17499/control
curl http://127.0.0.1:17499/probe                                 # okay: true   (text/plain, no JSON)
```
