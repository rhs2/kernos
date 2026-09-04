# Bundles

A bundle is what a company writes. It describes a workflow as data: the steps,
the prompts, the tools those steps may use, the policies that gate them, and
what to undo if the run is abandoned. It contains no code. The engine loads and
verifies signed bundles at runtime, so changing what a workflow does is a
bundle publish, not a deploy.

This is also what keeps the engine publishable: Kernos does not know what an
invoice is. A bundle does, and the bundle never leaves the company that wrote it.

## Shape

```json
{
  "format": "kernos.bundle/1",
  "name": "halcyon.finance.invoice_intake",
  "version": "1.0.0",
  "department": "finance",
  "policies": ["finance-default"],
  "tools": [
    {"id": "ledger.post_entry", "writes": true},
    {"id": "ledger.void_entry", "writes": true}
  ],
  "prompts": {
    "extract": {"system": "You extract fields from supplier invoices.", "user": "Invoice text:\n{{input.text}}"}
  },
  "workflows": {
    "intake": {
      "input_schema": {"type": "object", "required": ["invoice_id", "text", "total"]},
      "steps": [
        {"id": "extract", "kind": "model", "tier": "standard", "prompt": "extract", "output_schema": {"...": "..."}},
        {"id": "propose_payment", "kind": "action", "action": {"kind": "payment.issue", "amount": {"$ref": "steps.extract.output.total"}, "writes_to_system_of_record": true, "target": "ledger"}},
        {"id": "post", "kind": "tool", "tool": "ledger.post_entry", "args": {"invoice_id": {"$ref": "input.invoice_id"}},
         "idempotency_key": {"$ref": "input.invoice_id"},
         "compensation": {"tool": "ledger.void_entry", "args": {"entry_id": {"$ref": "steps.post.output.entry_id"}}}}
      ]
    }
  }
}
```

## Three kinds of step

- **model**: a call to a model at a declared tier and effort, with a prompt
  and an output schema the answer must satisfy. Refusals and low-confidence
  answers have declared outcomes.
- **action**: a proposal the policy engine decides on. It is the gate. The
  step's output is the decision.
- **tool**: a call through the gateway to a company system. A tool that writes
  must carry an idempotency key, and may declare a compensation.

Steps run in order. Each step sees the input and the outputs of the steps
before it, through `{{path}}` templates in strings and `{"$ref": "path"}` for
typed values.

## Frozen prompts, stable caches

A prompt's `system` text is frozen: it may not contain templates. Together
with the tool list it forms the stable prefix of every model call, which is
what makes prompt caching work and what makes cache hit rate a meaningful
service objective. Everything volatile goes in the user turn.

## Signed, versioned, pinned

A bundle is signed by a publisher key the control plane trusts. Unsigned
bundles and unknown keys are refused. In-flight runs finish on the bundle
version they started with, and the log records that version, so a run from
six weeks ago replays against the bundle it actually used.

See the [bundle format reference](../reference/bundle-format.md) for every
field, the templating rules and the validation the control plane applies, and
the guide [Write a bundle](../guides/write-a-bundle.md) for a walk-through.
