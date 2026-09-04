# The model layer

Kernos is model-agnostic in structure and ships with Claude as the reference
implementation, because the features the design depends on are ones that API
provides.

## Three tiers, chosen per step

| Tier | Default model | Used for |
|---|---|---|
| `deep` | `claude-opus-5` | Planning a run, ambiguous judgement, code changes, anything whose error is expensive |
| `standard` | `claude-sonnet-5` | The bulk of steps: drafting, extraction, structured transformation |
| `cheap` | `claude-haiku-4-5-20251001` | Classification, routing, extraction from a known shape, fan-out |

The tier is declared on the step in the bundle, so cost is reviewable where a
reviewer looks. The router picks from the declaration, not the model's opinion.
A step may declare an escalation rule (retry at the deeper tier when confidence
is low), and that escalation is itself an event. Model ids are overridable
through the environment.

## What each step declares

- **Effort**, passed as adaptive thinking effort. Planning runs high; mechanical
  steps run low. Effort is the main cost lever and belongs where a reviewer
  sees it.
- **A token ceiling** for the answer, so a long step finishes gracefully.
- **An output schema.** Structured output is requested from the model and
  validated on return, so a step that feeds another system parses by guarantee.
- **What a refusal means.** A refusal is a first-class outcome recorded on the
  run: park, escalate a tier, or fail.
- **Data classes** the prompt content carries. Content whose class the remit
  does not grant is redacted before it reaches the provider, and the redaction
  is recorded.

## Caching discipline

The system prompt and the sorted tool list come first and never change within
a bundle version; the volatile content comes last. The worker records a hash of
that prefix on every call, so cache hit rate is measurable and a silent
invalidator shows up as a regression instead of a quietly larger bill.

## The mock provider

Every bundle can carry mock outputs per prompt. With the mock provider the whole
engine runs end to end with no API key and no network: the acceptance suite,
the golden-set evaluation, and any local development use it. Refusals and
confidence values can be forced through the environment so those paths are
tested too.

## Cost

Usage from every model call is priced from a table the SDK ships with and
recorded on the run as cumulative tokens and currency. Budgets, pacing and the
promotion gate's cost comparison all read those numbers.
