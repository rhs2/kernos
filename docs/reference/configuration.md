# Configuration reference

Every binary reads a JSON config file (`--config`) and the environment; the
environment wins. Nothing is required for a local run with the mock provider.
A complete `.env.example` ships in the repository.

## Kernel

| Key | Environment | Default |
|---|---|---|
| `listen` | `KERNOS_LISTEN` | `127.0.0.1:7401` |
| `data_dir` | `KERNOS_DATA` | `./kernos-data` |
| `token` | `KERNOS_TOKEN` | unset (loopback only) |
| `lease_ttl_default` | `KERNOS_LEASE_TTL` | 30 |
| `lease_sweep_interval_ms` | `KERNOS_SWEEP_MS` | 1000 |
| `approval_sweep_interval_ms` | `KERNOS_APPROVAL_SWEEP_MS` | 5000 |
| `budget_soft_ratio` | `KERNOS_BUDGET_SOFT_RATIO` | 0.8 |
| `max_attempts_nondeterministic` | `KERNOS_MAX_ATTEMPTS` | 5 |
| `max_attempts_deterministic` | `KERNOS_MAX_DET_ATTEMPTS` | 3 |
| `log_format` | `KERNOS_LOG` | `text` or `json` |

Data directory:

```
kernos-data/
  kernos.db                 SQLite, WAL mode
  directory.json            users, roles and managers for approvals (optional)
  keys/control-plane.key    Ed25519 private key, created on first start, mode 0600
  keys/control-plane.pub
  keys/trusted/*.pub        publisher keys whose bundle signatures are accepted
```

## Gateway

| Key | Environment | Default |
|---|---|---|
| `listen` | `KERNOS_GATEWAY_LISTEN` | `127.0.0.1:7402` |
| `data_dir` | `KERNOS_GATEWAY_DATA` | `./gateway-data` |
| `kernel_url` | `KERNOS_KERNEL_URL` | `http://127.0.0.1:7401` |
| `token` | `KERNOS_TOKEN` | unset |
| | `KERNOS_PUBLIC_KEY` | fetched from the kernel when unset |
| `canary.interval_seconds` | `KERNOS_CANARY_INTERVAL` | 60 |
| `canary.quarantine_after` | `KERNOS_CANARY_QUARANTINE_AFTER` | 2 |
| `canary.auto_release` | `KERNOS_CANARY_AUTO_RELEASE` | false |
| | `KERNOS_GATEWAY_TEST_TOOLS` | 0 (never 1 in production) |

Connector credentials are `${NAME}` references inside `gateway.json`,
substituted from the environment at load.

## CLI

| Environment | Meaning |
|---|---|
| `KERNOS_SERVER` | Default `--server` for the commands that talk to a kernel (`http://127.0.0.1:7401`) |
| `KERNOS_TOKEN` | Default `--token` |
| `KERNOS_DATA` | Data directory for the offline key commands |

## Worker

| Environment | Default |
|---|---|
| `KERNOS_KERNEL_URL` | `http://127.0.0.1:7401` |
| `KERNOS_GATEWAY_URL` | `http://127.0.0.1:7402` |
| `KERNOS_TOKEN` | unset |
| `KERNOS_PROVIDER` | `mock` or `anthropic` |
| `ANTHROPIC_API_KEY` | required for `anthropic` |
| `KERNOS_MODEL_DEEP` / `_STANDARD` / `_CHEAP` | the three Claude tiers |
| `KERNOS_PRICING_JSON` | path to a price table override |
| `KERNOS_MOCK_REFUSE`, `KERNOS_MOCK_CONFIDENCE` | test-only mock behaviour |

## Conventions

Identifiers are a type prefix and a ULID (`run_`, `stp_`, `lse_`, `rem_`,
`apr_`, `act_`, `bnd_`, `pol_`, `key_`). Timestamps are RFC 3339 UTC with
milliseconds. Durations are integer seconds in JSON and `30m`, `4h`, `2d` in the
policy language. Money is `{"amount", "currency"}`.
