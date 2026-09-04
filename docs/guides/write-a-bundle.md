# Write a bundle

A bundle describes one department's workflows as data. This guide builds a
small one, signs it, loads it, and runs it, using the reference bundle as the
model. The complete field list is in the
[bundle format reference](../reference/bundle-format.md).

## 1. Name it and declare its tools

```json
{
  "format": "kernos.bundle/1",
  "name": "halcyon.support.triage",
  "version": "1.0.0",
  "department": "support",
  "policies": ["support-default"],
  "tools": [
    {"id": "helpdesk.lookup_ticket", "description": "Read one ticket", "writes": false},
    {"id": "helpdesk.set_queue", "description": "Route a ticket to a queue", "writes": true}
  ]
}
```

Every tool a step uses must be declared, with `writes` set honestly: the gateway
uses it to refuse writes under an `observe` remit.

## 2. Write the prompts

```json
"prompts": {
  "classify": {
    "system": "You triage support tickets for Halcyon Provisions. Answer only with the JSON the schema asks for.",
    "user": "Ticket subject: {{steps.fetch.output.subject}}\nBody:\n{{steps.fetch.output.body}}\nQueues: {{input.queues}}"
  }
}
```

The `system` text is frozen and may not contain templates; it is the stable
cache prefix. Everything that varies goes in `user`. Add a `mock` section with
an example output per prompt so the bundle runs end to end without a model:

```json
"mock": {"classify": {"queue": "billing", "confidence": 0.9}}
```

## 3. Define the workflow

```json
"workflows": {
  "triage": {
    "input_schema": {"type": "object", "required": ["ticket_id", "queues"],
                     "properties": {"ticket_id": {"type": "string"}, "queues": {"type": "array"}}},
    "steps": [
      {"id": "fetch", "kind": "tool", "tool": "helpdesk.lookup_ticket",
       "args": {"ticket_id": {"$ref": "input.ticket_id"}}},
      {"id": "classify", "kind": "model", "tier": "cheap", "effort": "low", "prompt": "classify",
       "output_schema": {"type": "object", "required": ["queue", "confidence"],
                         "properties": {"queue": {"type": "string"}, "confidence": {"type": "number"}}},
       "escalate": {"when_confidence_below": 0.7, "to_tier": "standard"}},
      {"id": "propose_route", "kind": "action",
       "action": {"kind": "ticket.route", "writes_to_system_of_record": true, "target": "helpdesk",
                  "summary": "Route {{input.ticket_id}} to {{steps.classify.output.queue}}"}},
      {"id": "route", "kind": "tool", "tool": "helpdesk.set_queue",
       "args": {"ticket_id": {"$ref": "input.ticket_id"}, "queue": {"$ref": "steps.classify.output.queue"}},
       "idempotency_key": "route-{{input.ticket_id}}",
       "compensation": {"tool": "helpdesk.set_queue",
                        "args": {"ticket_id": {"$ref": "input.ticket_id"}, "queue": {"$ref": "steps.fetch.output.queue"}}}}
    ]
  }
}
```

Three habits worth keeping:

- **Gate before you write.** An `action` step in front of every write gives
  policy a place to decide. The write itself is the next step.
- **Every write has an idempotency key** built from the input, so a resumed
  step never repeats it.
- **Every write that matters has a compensation** that puts the system back.
  Here it routes the ticket back to the queue it came from.

## 4. Validate, sign, apply

```bash
kernos bundle validate triage.json                   # the control plane's rules, offline
kernos bundle sign triage.json --key publisher.key --out triage.sig.json
kernos bundle apply triage.json --sig triage.sig.json
```

Validation catches references to undeclared tools, steps that reference later
steps, missing idempotency keys on writes, templates in system prompts and
schema mistakes, with the path of the offence.

## 5. Run it with a matching remit

```bash
kernos remit issue --tools "helpdesk.*" --scopes "helpdesk:queue:*" \
    --usd 0.5 --autonomy supervised --ttl 8h --policy-set support-default \
    --requested-by u-ana --role support_agent --manager u-tom
kernos run start --bundle halcyon.support.triage@1.0.0 --workflow triage \
    --input '{"ticket_id": "T-100", "queues": ["billing", "delivery", "quality"]}' --remit <remit id>
```

## 6. Change it without a deploy

Bump the version, sign, apply. Runs in flight finish on `1.0.0`; new runs use
`1.1.0`. Before you promote a prompt change to real traffic, score both versions
with the [evaluation harness](evaluate-and-promote.md).
