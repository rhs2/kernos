# Bundle format reference

Format version `kernos.bundle/1`. A bundle is data: workflows, steps, prompts,
tool declarations and policy references, signed by a trusted publisher key.

## Top level

| Field | Rule |
|---|---|
| `format` | `"kernos.bundle/1"` |
| `name` | `[a-z0-9_]+(\.[a-z0-9_]+)*`, at most 120 characters |
| `version` | Semantic version |
| `department` | Free text, used in policy as `run.department` and in metrics |
| `description` | Optional |
| `policies` | Policy names the control plane must have loaded; the remit's `policy_set` must include them |
| `tools` | `[{id, description, writes}]`; every tool a step or compensation uses must be declared |
| `prompts` | `{name: {system, user}}`; `system` may not contain templates |
| `mock` | Optional `{prompt name: output}` returned by the mock provider, with templating |
| `workflows` | `{name: {description, input_schema, steps}}` |

`input_schema` is JSON Schema (2020-12 subset: `type`, `required`, `properties`,
`items`, `enum`, `minimum`, `maximum`, `minLength`, `maxLength`, `pattern`,
`additionalProperties`).

## Steps

Common fields: `id` (`[a-z][a-z0-9_]*`, unique in the workflow), `kind`,
optional `description`, optional `timeout_seconds` (default 120).

### `model`

| Field | Meaning |
|---|---|
| `tier` | `deep`, `standard`, `cheap` |
| `effort` | `low`, `medium` (default), `high`, `xhigh` |
| `prompt` | A key of `prompts` |
| `output_schema` | JSON Schema the answer must satisfy |
| `max_output_tokens` | Default 2048 |
| `on_refusal` | `park` (default), `escalate`, `fail` |
| `escalate` | `{when_confidence_below, to_tier}`; requires `confidence` in the schema |
| `data_classes` | Classes the prompt content carries; redacted unless granted |

### `tool`

| Field | Meaning |
|---|---|
| `tool` | A declared tool id |
| `args` | Object with templating |
| `idempotency_key` | Templated string; required when the tool writes |
| `compensation` | `{tool, args}` run if the workflow is abandoned after this step completed; may reference this step's own output |
| `scope` | Literal scope for connectors without derivation |

### `action`

| Field | Meaning |
|---|---|
| `action` | `{kind, amount, currency, writes_to_system_of_record, target, data_classes, paths, idempotency_key, summary}` with templating |

The step's output is `{action_id, decision, rule, approval_id}`.

## Templating

Context: `{input, steps.<id>.output, run: {id, workflow, department, requested_by}}`.

- In strings, `{{path}}` is replaced by the value rendered as text (strings
  verbatim, numbers as JSON, objects and lists as compact JSON). A missing
  path is an error at execution time.
- As a whole value, `{"$ref": "path"}` substitutes the value with its type
  preserved, at any depth inside `args` and `action`.

Paths are dotted, with numeric segments for list indices.

## Signing

The signature covers the canonical JSON of the bundle object:

```json
{"key_id": "key_publisher_…", "algorithm": "ed25519", "signature": "<base64url>", "sha256": "<hex>"}
```

`kernos keys generate` makes a publisher key pair; `kernos keys trust`
installs the public half on the control plane; `kernos bundle sign` produces
the signature file; `kernos bundle apply` sends both.

## Validation

`422 bundle_invalid` with `details.path` when: a referenced prompt, tool or
step does not exist; step ids repeat; a writing tool step has no idempotency
key; `escalate` is set without `confidence` in the schema; a `system` prompt
contains `{{`; a `$ref` starts outside `input`, `steps` or `run`; a step
references a later step (a compensation may reference its own); or the
canonical size exceeds 1 MiB.
