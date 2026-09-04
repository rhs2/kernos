# Security policy

## Reporting a vulnerability

Please do not open a public issue for a security problem. Use GitHub's private
vulnerability reporting on this repository (**Security** tab, **Report a
vulnerability**). You will get an acknowledgement within three working days and
a fix or a mitigation plan within thirty. Credit is given in the release notes
unless you ask otherwise.

## What counts

Anything that lets a run exceed its remit, forge or widen a remit, bypass an
approval, tamper with the event log without detection, leak a credential from
the gateway into a prompt, log or event, or apply an automatic repair that
changes policy rather than a mapping. Those are the guarantees the design makes
and they are the ones treated as critical.

## Supported versions

The latest minor release receives security fixes. Older releases receive them
when the fix is small enough to backport safely.

## Design notes for reviewers

- Authority is a signed capability verified at the gateway on every call.
  Prompt content cannot change it. See the remit reference in the documentation.
- The event log is hash-chained and append-only; replay recomputes the chain.
- Secrets are substituted at egress in the gateway from environment variables
  and never serialised into responses, logs or events.
- Bundles are signed artefacts; unsigned bundles are refused. Policies are
  versioned and tested against historical actions before they are applied.
- Full-history secret scanning runs in CI on every push and before every release.
