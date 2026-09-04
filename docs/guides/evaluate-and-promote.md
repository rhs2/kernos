# Evaluate and promote

Nothing reaches production because someone was confident. A prompt edit, a
model upgrade, a connector mapping and a bundle version all ship the same way:
score the candidate against a golden set, compare cost, and promote only on
evidence. This is the part teams skip and the reason their agents rot.

## 1. Build a golden set

A golden set is a directory of cases for one workflow:

```
golden/
  set.json
  cases/c001.json
  cases/c002.json
```

```json
{"name": "invoice_intake", "bundle": "halcyon.finance.invoice_intake@1.0.0", "workflow": "intake", "version": 3}
```

```json
{"id": "c001",
 "input": {"invoice_id": "inv-9001", "total": 1280.5, "text": "Harbor Greens invoice 9001 ..."},
 "expect": {"steps.extract.output.vendor": "Harbor Greens", "steps.code.output.account": "5100"},
 "assert": ["steps.code.output.confidence >= 0.7", "run.state == 'completed'"],
 "rubric": {"step": "extract", "criteria": ["vendor is the legal name on the invoice"]}}
```

`expect` paths must equal, `assert` expressions must hold, and `rubric`
criteria are graded by a model on the cheap tier. Cases are generated or
anonymised, never copied from real data: a real golden set is a data leak
wearing a test's clothes.

## 2. Score a version

```bash
kernos-eval run --golden golden/ --kernel http://127.0.0.1:7401 --gateway http://127.0.0.1:7402 \
    --provider anthropic --out baseline.json
```

Every case runs as a real run through the kernel, so the event log is the
evidence. The report carries the pass rate, cost, latency and every failure
with its reason.

## 3. Score the candidate

Apply the new bundle version (or point workers at the candidate model), run the
same golden set, and compare:

```bash
kernos-eval run --golden golden/ ... --out candidate.json
kernos-eval gate --baseline baseline.json --candidate candidate.json \
    --max-pass-drop 0.0 --max-cost-increase 0.15 --max-error-increase 0.0
```

The gate exits 0 to promote and 1 to roll back, and prints the comparison. Wire
it into whatever promotes for you: a CI job, a release script, or a scheduled
run that tries each new model as a candidate and files a report when it wins.

## 4. Watch for drift in the set itself

When production inputs stop resembling the golden set, the suite is no longer
measuring reality. Compare the distribution of inputs on recent runs with the
set periodically and treat divergence as an alert to add cases, not as a pass.

## What the gate guards

| Change | Candidate | Evidence |
|---|---|---|
| Prompt edit | new bundle version | pass rate and cost on the golden set |
| Model upgrade | workers on the new model id | same set, same gate; promote only if better on quality and acceptable on cost |
| Connector mapping fix | patched gateway config | the connector's own contract and probe, then the golden set |
| Policy change | new policy version | the corpus flip report from `kernos policy test` |
