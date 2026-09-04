# Contributing to Kernos

Thank you for considering it. Kernos is an engine, deliberately empty of
business logic, so most contributions fall into one of four kinds: a kernel or
control-plane improvement (Rust), a connector or gateway feature (Go), a
reasoning or evaluation feature (Python), or a client change (TypeScript).
Reference bundles on the fictional company are welcome too.

## Ground rules

- **The reference documentation is the contract.** The pages under
  `docs/reference/` describe the event log, the kernel and gateway APIs, the
  remit token, the policy language and the bundle format. A change to an
  interface is a change to those pages, in the same pull request, with the
  format version bumped when a wire format changes.
- **Nothing company-specific enters this repository.** No real system names,
  account structures, internal terminology or data. Examples use the fictional
  Halcyon Provisions and are generated, never extracted.
- **Every behaviour has a test**, and the acceptance suite (`make accept`) must
  pass. A pull request that changes what the engine does and does not touch a
  test is not ready.
- **Secrets never reach the reasoning layer.** Connectors hold credentials; the
  gateway substitutes them at egress. Full-history secret scanning runs in CI.
- Write proper sentences in comments, docs and messages. No em dashes or en
  dashes; use a comma, a colon or a new sentence.

## Setting up

```
git clone https://github.com/rhs2/kernos && cd kernos
make build      # Rust stable, Go 1.22+, Python 3.10+, Node 18+
make test
make accept
```

`make lint` runs clippy, go vet, ruff and tsc. `make fmt` formats everything.
`make docs` serves the documentation site locally.

## Pull requests

1. Open an issue first for anything larger than a bug fix, so the design is
   agreed before the code exists.
2. One concern per pull request. Keep the diff readable.
3. Update `CHANGELOG.md` under an `Unreleased` heading.
4. CI must be green: unit tests in all four languages, the acceptance suite, the
   version check and the secret scan.

## Releasing

Maintainers cut a release by bumping the version in every manifest (the check
in `scripts/check_versions.py` enforces that they agree), moving the changelog
entry under the new version, and pushing a tag `vX.Y.Z`. The release workflow
does the rest.

## Licence

By contributing you agree that your contribution is licensed under the Apache
License 2.0, including its patent grant.
