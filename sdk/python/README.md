# kernos-sdk

The Python part of [Kernos](https://github.com/rhs2/kernos): the reasoning worker
that leases steps from the kernel and executes them, a typed client for the kernel
and gateway HTTP APIs, the model router and providers, and the evaluation harness
that gates promotions.

```bash
pip install kernos-sdk            # mock provider only
pip install "kernos-sdk[anthropic]"

kernos-worker --kernel http://127.0.0.1:7401 --gateway http://127.0.0.1:7402 \
              --provider mock --worker-id wrk-a1 --concurrency 4

kernos-eval run --golden golden/ --kernel http://127.0.0.1:7401 \
                --gateway http://127.0.0.1:7402 --provider mock --out report.json
kernos-eval gate --baseline base.json --candidate cand.json
```

```python
from kernos import GatewayClient, KernelClient, ModelRouter, Worker, providers
from kernos.eval import gate, run_golden
from kernos.templating import render, resolve_refs
```

The contracts this package implements are documented at https://rhs2.github.io/kernos/reference/.
specification is silent are listed in `NOTES.md`.

Every environment variable has a flag equivalent: `KERNOS_KERNEL_URL`,
`KERNOS_GATEWAY_URL`, `KERNOS_TOKEN`, `KERNOS_PROVIDER`, `KERNOS_MODEL_DEEP`,
`KERNOS_MODEL_STANDARD`, `KERNOS_MODEL_CHEAP`, `KERNOS_PRICING_JSON`,
`KERNOS_MOCK_REFUSE`, `KERNOS_MOCK_CONFIDENCE`, `KERNOS_LOG`.

Licensed under Apache 2.0.
