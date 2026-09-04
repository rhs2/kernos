# Install

Kernos is four components. Install the ones you run; a laptop needs all four
to try it, a production deployment usually runs the kernel and gateway as
services and the workers wherever the model traffic should originate.

## From the registries

=== "Kernel and CLI (Rust)"

    ```bash
    cargo install kernos
    kernos --version
    ```

    Or download a binary for Linux or macOS from the
    [releases page](https://github.com/rhs2/kernos/releases).

=== "Gateway (Go)"

    ```bash
    go install github.com/rhs2/kernos/gateway/cmd/kernos-gateway@latest
    kernos-gateway --help
    ```

    Binaries for Linux and macOS are attached to every release.

=== "Worker and SDK (Python)"

    ```bash
    pip install "kernos-sdk[anthropic]"      # the extra pulls the Anthropic SDK
    kernos-worker --help
    kernos-eval --help
    ```

    Python 3.10 or newer.

=== "Client (TypeScript)"

    ```bash
    npm install @kernos/sdk
    ```

    Node 18 or newer, or any browser with `fetch`.

=== "Containers"

    ```bash
    docker pull ghcr.io/rhs2/kernos-kernel:latest
    docker pull ghcr.io/rhs2/kernos-gateway:latest
    docker pull ghcr.io/rhs2/kernos-worker:latest
    ```

    `deploy/docker-compose.yml` in the repository starts all three with the
    reference connectors.

## From source

```bash
git clone https://github.com/rhs2/kernos && cd kernos
make build        # target/release/kernos, gateway/bin/kernos-gateway, sdk/python/.venv, sdk/typescript/dist
make test         # unit and integration tests in all four languages
make accept       # the end-to-end acceptance suite (no API key, no network)
```

Toolchains: Rust stable, Go 1.22 or newer, Python 3.10 or newer, Node 18 or
newer. `make` prints every target with `make help`.

## Model access

Nothing needs an API key to run: the mock provider executes any bundle end to
end from the mock outputs the bundle declares, which is how the acceptance
suite and the evaluation harness run in CI. To use real models, set
`ANTHROPIC_API_KEY` and start workers with `--provider anthropic`.
