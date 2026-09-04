# Quickstart

Twenty minutes, no API key. You will start the kernel and gateway, load the
reference bundle and policy for the fictional Halcyon Provisions, run one invoice
through intake, watch it park at the approval gate, approve it, and replay the log.

## 1. Build and prove it works

```bash
git clone https://github.com/rhs2/kernos && cd kernos
make build
make accept
```

The acceptance suite starts every component on test ports and runs thirteen
scenarios: a worker killed mid-step resumes without double-posting, replay
detects a tampered byte, an abandoned run voids what it posted, calls outside
the remit are refused, delegation cannot widen, a payment parks for approval and
resumes, a renamed upstream field quarantines a connector, and so on. Every line
should read `PASS`.

## 2. Start the kernel

```bash
export PATH="$PWD/target/release:$PWD/gateway/bin:$PWD/sdk/python/.venv/bin:$PATH"
mkdir -p run && cd run
cp ../bundles/reference/halcyon/directory.json .          # who reports to whom, for escalation
KERNOS_DATA=./kernos-data kernos serve &
kernos health
```

On first start the kernel creates its Ed25519 control-plane key under
`kernos-data/keys/`. That key signs remits; the gateway will fetch its public
half.

## 3. Start the gateway with the ledger connector

```bash
cp ../bundles/reference/halcyon/ledger.sql .
export HALCYON_LEDGER_DB=$PWD/halcyon-ledger.db KERNOS_GATEWAY_DATA=$PWD/gateway-data
kernos-gateway --config ../bundles/reference/halcyon/gateway.quickstart.json &
curl -s http://127.0.0.1:7402/v1/health
```

The gateway creates the ledger database from `ledger.sql` on first start,
because the connector names it under `init_sql`, and then lists its connectors
with their canary status. The ledger connector exposes `ledger.post_entry`,
`ledger.void_entry` and `ledger.lookup_vendor`. The database path came from the
environment, never from the config file, which is how every credential reaches
a connector.

## 4. Load the policy and the bundle

```bash
kernos policy apply ../bundles/reference/halcyon/policies/finance-default.policy \
    --name finance-default --version 1

kernos keys generate --out publisher                # a publisher key for signing bundles
kernos keys trust publisher.pub                     # the control plane will accept its signatures
kernos bundle sign ../bundles/reference/halcyon/bundle.json --key publisher.key --out bundle.sig.json
kernos bundle apply ../bundles/reference/halcyon/bundle.json --sig bundle.sig.json
```

Try applying the bundle without the signature, or after changing one character
in it: the control plane refuses both.

## 5. Issue a remit and start a worker

```bash
kernos remit issue \
    --tools "ledger.*" \
    --scopes "sql:table:ledger_entries,sql:table:vendors" \
    --usd 2 --tokens 200000 --autonomy supervised --ttl 24h \
    --policy-set finance-default \
    --requested-by u-ana --role ap_clerk --manager u-tom
kernos-worker --provider mock --worker-id wrk-1 &
```

Note the remit id it prints; the next step needs it. The remit says what this
run may do and for whom. The worker uses the mock
provider, which answers from the mock outputs declared in the bundle, so no
model traffic leaves your machine.

## 6. Run an invoice and hit the gate

```bash
cat > invoice.json <<'EOF'
{"invoice_id": "inv-1001", "total": 7250.00,
 "text": "Northwind Dairy, invoice 1001, milk delivery, total 7,250.00 USD",
 "accounts": ["5100 Cost of goods", "6200 Freight"]}
EOF
kernos run start --bundle halcyon.finance.invoice_intake@1.0.0 --workflow intake \
    --input invoice.json --remit <remit id>
sleep 2
kernos run show <run id>
```

The run extracted the invoice, coded it, proposed a payment of 7,250, and
parked: the policy requires a `finance_admin` to approve anything at or above
5,000. The worker holds no lease while it waits.

```bash
kernos approvals list
kernos approvals decide <approval id> --approve --as u-tom --role finance_admin \
    --reason "Matches purchase order 4471"
sleep 2
kernos run show <run id>
sqlite3 halcyon-ledger.db 'select id, invoice_id, vendor, account, amount from ledger_entries'
```

Deciding as `u-ana` with role `ap_clerk` is refused: she is not the approver.
After Tom's decision the step resumes, the same proposal is now allowed, and
one ledger row exists.

## 7. Read the log and replay it

```bash
kernos run events <run id>
kernos run replay <run id>
```

The events answer who approved what, when and why, and what the model was asked
and answered. Replay recomputes the hash chain, folds the events back into the
run state, and re-evaluates every policy decision: all three should verify.

## 8. Abandon a run

Start a third invoice, let it park at the gate, and abandon it instead of
deciding:

```bash
kernos run abandon <run id> --reason "duplicate invoice"
kernos run show <run id>
kernos approvals list
```

The pending approval is cancelled, the run ends `abandoned`, and no ledger row
was written because the run never reached its posting step. A run may be
abandoned while it is running, parked or failed, never once it has completed.

Had it already posted, the kernel would have scheduled the compensation the
bundle declares for that step, `ledger.void_entry`, a worker would have run it,
and the entry would carry its `voided_at` and reason. The run stays `running`
until every compensation has completed and only then reads `abandoned`, so a
client that waits for that state sees the unwinding finished. Scenario A3 of
the acceptance suite proves exactly that, with two writes unwound in reverse
order.

## Where next

- [The Halcyon example](halcyon.md) explains every step and policy rule you just used.
- [Write a bundle](../guides/write-a-bundle.md) to describe your own workflow.
- [Write a connector](../guides/write-a-connector.md) to reach your own systems.
- [Deploy](../guides/deploy.md) for containers, tokens and production settings.
