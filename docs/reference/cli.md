# CLI reference

The `kernos` binary is both the server and the operator's tool. Every
subcommand prints a readable table by default and JSON with `--json`, exits
non-zero on any error, and works against a remote server with `--server URL`
(or `KERNOS_SERVER`) and `--token` (or `KERNOS_TOKEN`).

## Server

```
kernos serve [--listen 127.0.0.1:7401] [--data ./kernos-data] [--config kernos.json]
kernos health [--server URL]
```

## Keys

```
kernos keys generate --out publisher          # publisher.key (0600) and publisher.pub
kernos keys trust publisher.pub               # install into KERNOS_DATA/keys/trusted/
```

## Bundles

```
kernos bundle validate bundle.json                          # offline, the control plane's own rules
kernos bundle sign bundle.json --key publisher.key --out bundle.sig.json
kernos bundle apply bundle.json [--sig bundle.sig.json]
kernos bundle list
kernos bundle show bnd_...
```

## Policies

```
kernos policy check finance-default.policy                  # offline, parse only
kernos policy apply finance-default.policy --name finance-default --version 1
kernos policy list
kernos policy show finance-default [--version 1]
kernos policy test --a finance-default@1 --b finance-default-10k@1 --corpus actions.jsonl
```

`policy test` reports every row of the corpus whose **outcome** changes: the
decision, and for an approval the gate it creates (approver, SLA, escalation).
The rule that matched each side is printed for context and is not itself the
comparison, so testing two differently named policies does not report every
matched row.

## Remits

```
kernos remit issue --tools "ledger.*,http.get" --scopes "sql:table:ledger_entries" \
    [--grants pii] --usd 2 --tokens 200000 --autonomy supervised --ttl 24h \
    --policy-set finance-default --requested-by u-ana --role ap_clerk [--manager u-tom]
kernos remit derive rem_… [--tools …] [--scopes …] [--usd …] [--autonomy …] [--ttl …]
kernos remit show rem_…
```

## Runs

```
kernos run start --bundle NAME@VERSION --workflow intake --input input.json --remit rem_… \
    [--requested-by u-ana --role ap_clerk --manager u-tom]
kernos run list [--state parked] [--department finance]
kernos run show run_…
kernos run events run_… [--from 1]
kernos run replay run_…
kernos run abandon run_… --reason "…"
kernos run resume run_…
kernos run actions [--department finance] [--since 30d]     # export a policy test corpus
```

## Approvals

```
kernos approvals list [--state pending] [--approver role:finance_admin]
kernos approvals decide apr_… --approve|--reject --as u-tom --role finance_admin --reason "…"
```

## Exit codes

0 success; 1 the server refused or the command failed (the error code and
message are printed); 2 usage error.
