# Connector SDK reference (Go)

Package `github.com/rhs2/kernos/gateway/connect`. A connector is a Go type
that the gateway binary registers; the gateway handles remit verification,
idempotency, breakers, canaries and metrics around it.

```go
type ToolSpec struct {
    ID              string
    Description     string
    Writes          bool
    InputSchema     map[string]any
    Contract        Contract
    ScopeDerivation string
}

type Contract struct {
    Required map[string]string   // field name -> "string" | "number" | "bool" | "object" | "list"
}

type Connector interface {
    Name() string
    Tools() []ToolSpec
    Call(ctx context.Context, tool string, args map[string]any) (result map[string]any, scopes []string, err error)
    Probe(ctx context.Context) (result map[string]any, err error)
}

func Register(factory func(cfg map[string]any) (Connector, error), typeName string)

var ErrDeterministic = errors.New("deterministic failure")
```

## Contract

| Method | Responsibility |
|---|---|
| `Tools` | Declare every tool with an honest `Writes`, an input schema the gateway validates before calling you, a response contract the canary checks, and how scope is derived |
| `Call` | Do the work, and return the scopes the call touched, derived from the arguments and never from a caller-supplied field |
| `Probe` | A harmless operation whose result is checked against the contract on every canary interval |

Wrap errors the kernel should not retry with `ErrDeterministic`
(`fmt.Errorf("%w: %v", connect.ErrDeterministic, err)`); the gateway answers
`422` with `deterministic: true`. Any other error is `502` and retried by the
kernel's backoff policy.

## Registration and configuration

`Register` in the package's `init`; import the package in your gateway binary
for its side effect. The factory receives the connector's object from
`gateway.json` with `${VAR}` already substituted from the environment. Never
log or return a configuration value that came from a substitution.

## Helpers

`connect` also exports scope-string constructors, the JSON Schema subset
validator used for input schemas, and the contract checker, so a connector's
own tests can assert its probe satisfies its contract.
