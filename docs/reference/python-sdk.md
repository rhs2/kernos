# Python SDK reference

Package `kernos-sdk`, import `kernos`, Python 3.10+. Runtime dependencies:
`httpx`, `jsonschema`, `pydantic`. Optional extra `anthropic`.

```bash
pip install "kernos-sdk[anthropic]"
```

## The worker

```
kernos-worker --kernel http://127.0.0.1:7401 --gateway http://127.0.0.1:7402 \
              --provider mock|anthropic --worker-id wrk-a1 \
              --kinds model,tool,action,compensation --lease-ttl 30 --concurrency 4
```

Environment equivalents: `KERNOS_KERNEL_URL`, `KERNOS_GATEWAY_URL`,
`KERNOS_TOKEN`, `KERNOS_PROVIDER`. The loop leases a step, heartbeats while
working, executes it by kind, and reports completion or failure with an honest
`deterministic` flag. A heartbeat answered `410` means another worker owns the
step; the current one abandons it at once. `SIGTERM` finishes the step in hand
and exits 0.

## Step execution

- **model**: builds the request with the stable prefix first (system text, the
  sorted tool list, the output schema), applies the data boundary, records
  `model.called`, calls the provider, records `model.responded`, handles refusal
  per the step, validates the output with one corrective retry, escalates on
  low confidence, and completes with the parsed output and usage.
- **tool**: renders arguments and the idempotency key, reuses a prior result
  for the same key from the lease context, records `tool.called` before the
  gateway call, records `tool.result`, maps `403` and `422` to deterministic
  failures and `5xx` to retryable ones.
- **action**: proposes the action; `allow` completes, `approval_required`
  stops without completing or failing, `deny` fails deterministically.
- **compensation**: identical to `tool` with resolved arguments.

## Model router

| Tier | Model | Override |
|---|---|---|
| `deep` | `claude-opus-5` | `KERNOS_MODEL_DEEP` |
| `standard` | `claude-sonnet-5` | `KERNOS_MODEL_STANDARD` |
| `cheap` | `claude-haiku-4-5-20251001` | `KERNOS_MODEL_CHEAP` |

Providers: `anthropic` (Messages API with adaptive thinking, per-step effort,
JSON-schema structured output, cache control on the system block, refusal
mapping) and `mock` (the bundle's mock outputs rendered with the context;
`KERNOS_MOCK_REFUSE=<prompt>` forces a refusal, `KERNOS_MOCK_CONFIDENCE=<prompt>=<value>`
overrides a confidence). Cost comes from a price table in `kernos.pricing`,
overridable with `KERNOS_PRICING_JSON`.

## Evaluation

```
kernos-eval run  --golden golden/ --kernel URL --gateway URL --provider mock --out report.json
kernos-eval gate --baseline base.json --candidate cand.json \
                 --max-pass-drop 0.0 --max-cost-increase 0.15 --max-error-increase 0.0
```

`run` executes every case as a real run and scores `expect` paths, `assert`
expressions (the policy expression grammar over `steps`, `run`, `input`) and
optional rubrics graded on the cheap tier. `gate` exits 0 to promote and 1 to
roll back.

## Public API

```python
from kernos import KernelClient, GatewayClient, Worker, ModelRouter, providers
from kernos.templating import render, resolve_refs
from kernos.eval import run_golden, gate
```

`KernelClient` and `GatewayClient` cover every endpoint of the two APIs and
raise `KernosError(status, code, message, details)` on a non-2xx answer and
`KernosNetworkError` on transport failure. Type hints and docstrings on every
public symbol; `py.typed` ships with the package.
