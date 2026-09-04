# Deploy

Kernos is three long-running processes and a stateless worker pool. This page
covers the settings that matter in production; every value is in the
[configuration reference](../reference/configuration.md).

## Topology

- **Kernel**: one instance per environment, on a persistent volume. It is the
  only writer of durable state. SQLite in WAL mode handles a department's load
  comfortably; the data directory is the thing to back up.
- **Gateway**: one or more instances next to the systems they reach. Each holds
  the credentials for its connectors in its own environment and nothing else.
- **Workers**: as many as the model traffic needs. They hold nothing, so they
  scale to zero and back.

## Containers

```bash
docker compose -f deploy/docker-compose.yml up --build
```

The compose file starts the kernel, the gateway with the reference connectors
and a worker on the mock provider. Set `ANTHROPIC_API_KEY` and
`KERNOS_PROVIDER=anthropic` for real models. The images are published to GHCR
on every release and run as non-root with pinned bases.

## The settings that matter

| Setting | Production value |
|---|---|
| `KERNOS_TOKEN` | Set it. Workers and the gateway send it as a bearer token. Without it the kernel only listens on loopback |
| `KERNOS_DATA` | A persistent volume. Contains the database and the control-plane key |
| `KERNOS_LEASE_TTL` | 30 seconds is right for model steps; raise it for tools that legitimately take longer |
| `KERNOS_GATEWAY_TEST_TOOLS` | Never `1` outside a test environment |
| `KERNOS_PUBLIC_KEY` | Pin the control-plane key in the gateway instead of fetching it |
| `KERNOS_CANARY_INTERVAL` | 60 seconds; shorter for connectors whose drift is expensive |
| Connector credentials | Environment variables referenced as `${NAME}` from `gateway.json`, never literal values |

## Keys

The kernel creates its Ed25519 control-plane key on first start under
`KERNOS_DATA/keys/`. Back it up with the data directory; losing it invalidates
every outstanding remit (which is recoverable) and every remit ever recorded
in logs stays verifiable only with the public half, so keep that too.

Publisher keys sign bundles. Generate one per team that publishes bundles,
trust only its public half on the control plane, and keep the private half in
the team's own secret store. The control plane never needs it.

## Observability

Both the kernel and the gateway serve Prometheus text at `/v1/metrics`: runs by
state, step latency, tokens and currency per department, approvals pending,
refusals by reason, canary status and circuit state. Logs are structured
(`KERNOS_LOG=json`) with `run_id`, `step` and `lease_id` on every line that has
them. The event log itself is the source of truth for any investigation.

## Backups and upgrades

Back up `KERNOS_DATA` with a filesystem snapshot or `sqlite3 .backup`; the
database is in WAL mode and is consistent at every commit. Upgrade the kernel
by stopping it, replacing the binary, and starting it; migrations run on start
and are additive. Replay a sample of historical runs after an upgrade: a kernel
that folds old logs to a different state is a kernel to roll back.

## Systemd

```ini
[Unit]
Description=Kernos kernel
After=network.target

[Service]
User=kernos
EnvironmentFile=/etc/kernos/kernel.env
ExecStart=/usr/local/bin/kernos serve
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
```

The gateway and workers follow the same shape with their own environment
files. Workers exit cleanly on `SIGTERM` after the step in hand, and a step
they were killed inside is resumed by another worker when its lease expires.
