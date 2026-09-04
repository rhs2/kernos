# kernos

The [Kernos](https://github.com/rhs2/kernos) kernel and control plane as one
binary: the HTTP API of the kernel (event log, scheduler, leases, budgets,
compensation) and the control plane (bundles, remits, policies, approvals), plus
the operator command line.

```text
kernos serve --listen 127.0.0.1:7401 --data ./kernos-data
kernos keys generate --out publisher
kernos keys trust publisher.pub
kernos bundle sign bundle.json --key publisher.key --out bundle.sig.json
kernos bundle apply bundle.json --sig bundle.sig.json --json
kernos policy apply finance-default.policy --name finance-default --version 1
kernos remit issue --tools "ledger.*" --usd 2 --autonomy supervised --ttl 24h
kernos run start --bundle halcyon.finance.invoice_intake@1.0.0 --workflow intake --input input.json --remit rem_...
kernos run show run_...   kernos run replay run_...   kernos approvals list
```

Every subcommand prints JSON with `--json`, exits non-zero on any error and talks
to a remote server with `--server URL` (or `KERNOS_SERVER`). Configuration comes
from `--config kernos.json` and `KERNOS_*` environment variables; see
`https://rhs2.github.io/kernos/reference/kernel-api/` in the repository for the full table.

Licensed under Apache-2.0.
