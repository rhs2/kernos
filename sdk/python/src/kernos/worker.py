"""The reasoning worker (07-REASONING-SDK, Worker loop).

Concurrency is thread based: ``concurrency`` loop threads each lease, execute
and report one step at a time, sharing one kernel client, one gateway client
and one provider (the HTTP clients are thread safe). Every leased step gets its
own heartbeat thread; a heartbeat answered with 410 sets the step's abort flag
and the executor stops at its next checkpoint, never calling ``complete`` or
``fail``. ``SIGTERM`` (and ``SIGINT``) asks the loops to stop after the step
they are on; the process then exits 0.

Exit codes: 0 after a clean stop, 2 on configuration errors, never on step
failures.
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import random
import secrets
import signal
import sys
import threading
import time
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from kernos._logging import StepLogger, configure_logging, step_logger
from kernos.client import GatewayClient, KernelClient, KernosError, KernosNetworkError
from kernos.providers import ProviderUnavailable, get_provider
from kernos.providers.base import ModelProvider
from kernos.router import ModelRouter
from kernos.steps import STEP_KINDS, Lease, Runtime, StepOutcome, execute_step

__all__ = ["DEFAULT_KINDS", "Heartbeat", "Worker", "WorkerConfig", "main"]

logger = logging.getLogger("kernos.worker")

DEFAULT_KINDS: tuple[str, ...] = STEP_KINDS
ENV_KERNEL_URL = "KERNOS_KERNEL_URL"
ENV_GATEWAY_URL = "KERNOS_GATEWAY_URL"
ENV_TOKEN = "KERNOS_TOKEN"
ENV_PROVIDER = "KERNOS_PROVIDER"
ENV_LOG = "KERNOS_LOG"
EXIT_OK = 0
EXIT_CONFIG = 2


def _default_worker_id() -> str:
    return f"wrk-{secrets.token_hex(3)}"


@dataclass
class WorkerConfig:
    """Everything a worker needs; flags, environment and ``--config`` all map here."""

    kernel_url: str = "http://127.0.0.1:7401"
    gateway_url: str = "http://127.0.0.1:7402"
    provider: str = "mock"
    worker_id: str = field(default_factory=_default_worker_id)
    kinds: tuple[str, ...] = DEFAULT_KINDS
    lease_ttl: int = 30
    concurrency: int = 4
    token: str | None = None
    idle_sleep: tuple[float, float] = (0.2, 1.0)
    request_timeout: float = 30.0
    log_format: str = "text"

    def validate(self) -> None:
        """Raise ``ValueError`` on an unusable configuration."""
        for url in (self.kernel_url, self.gateway_url):
            if not url.startswith(("http://", "https://")):
                raise ValueError(f"url must start with http:// or https://: {url!r}")
        unknown = [kind for kind in self.kinds if kind not in STEP_KINDS]
        if unknown or not self.kinds:
            raise ValueError(f"kinds must be a non-empty subset of {STEP_KINDS}, got {self.kinds}")
        # The requested TTL is a request, not a promise: the kernel clamps it to
        # its own configured range and answers with the authoritative expiry, so
        # the worker only refuses a value that cannot be a duration at all.
        if not 1 <= self.lease_ttl <= 300:
            raise ValueError("lease ttl must be between 1 and 300 seconds")
        if self.concurrency < 1:
            raise ValueError("concurrency must be at least 1")
        if self.log_format not in ("text", "json"):
            raise ValueError("log format must be text or json")

    @classmethod
    def from_sources(
        cls,
        flags: Mapping[str, Any] | None = None,
        env: Mapping[str, str] | None = None,
        config_file: Mapping[str, Any] | None = None,
    ) -> WorkerConfig:
        """Merge sources: flags override environment, environment overrides the file."""
        environment = os.environ if env is None else env
        merged: dict[str, Any] = {}
        for key, value in (config_file or {}).items():
            merged[key] = value
        env_map = {
            "kernel_url": ENV_KERNEL_URL,
            "gateway_url": ENV_GATEWAY_URL,
            "token": ENV_TOKEN,
            "provider": ENV_PROVIDER,
            "log_format": ENV_LOG,
        }
        for key, variable in env_map.items():
            if environment.get(variable):
                merged[key] = environment[variable]
        for key, value in (flags or {}).items():
            if value is not None:
                merged[key] = value
        if isinstance(merged.get("kinds"), str):
            merged["kinds"] = tuple(k.strip() for k in merged["kinds"].split(",") if k.strip())
        elif isinstance(merged.get("kinds"), list):
            merged["kinds"] = tuple(str(k) for k in merged["kinds"])
        known = {name for name in cls.__dataclass_fields__}
        return cls(**{key: value for key, value in merged.items() if key in known})


class Heartbeat:
    """Heartbeat one lease every ``heartbeat_seconds``; set ``abort`` on 410."""

    def __init__(
        self,
        kernel: KernelClient,
        lease: Lease,
        abort: threading.Event,
        log: StepLogger,
        interval: float | None = None,
    ) -> None:
        self._kernel = kernel
        self._lease = lease
        self._abort = abort
        self._log = log
        self._interval = interval or lease.heartbeat_seconds or 10.0
        self._stopped = threading.Event()
        self._thread = threading.Thread(
            target=self._loop, name=f"heartbeat-{lease.lease_id}", daemon=True
        )
        self.beats = 0

    def start(self) -> None:
        """Start the heartbeat thread."""
        self._thread.start()

    def stop(self) -> None:
        """Stop the heartbeat thread and wait for it."""
        self._stopped.set()
        self._thread.join(timeout=self._interval + 5)

    def _loop(self) -> None:
        while not self._stopped.wait(self._interval):
            try:
                self._kernel.heartbeat(self._lease.lease_id, timeout=self._interval)
                self.beats += 1
            except KernosNetworkError as exc:
                self._log.warning("heartbeat failed: %s", exc)
            except KernosError as exc:
                if exc.status == 410:
                    self._log.warning("lease expired (410); abandoning the step locally")
                    self._abort.set()
                    return
                self._log.warning("heartbeat rejected: %s", exc)


class Worker:
    """The worker loop. Construct with a :class:`WorkerConfig`; call :meth:`run`."""

    def __init__(
        self,
        config: WorkerConfig,
        *,
        kernel: KernelClient | None = None,
        gateway: GatewayClient | None = None,
        provider: ModelProvider | None = None,
        router: ModelRouter | None = None,
    ) -> None:
        config.validate()
        self.config = config
        self.kernel = kernel or KernelClient(
            config.kernel_url, token=config.token, timeout=config.request_timeout
        )
        self.gateway = gateway or GatewayClient(
            config.gateway_url, token=config.token, timeout=config.request_timeout
        )
        self.provider = provider or get_provider(config.provider)
        self.router = router or ModelRouter()
        self.runtime = Runtime(
            kernel=self.kernel,
            gateway=self.gateway,
            provider=self.provider,
            router=self.router,
            worker_id=config.worker_id,
        )
        self._stop = threading.Event()
        self._schema_cache: dict[str, dict[str, Any] | None] = {}
        self._lock = threading.Lock()
        self.outcomes: list[StepOutcome] = []

    @property
    def stopping(self) -> bool:
        """Whether a stop was requested."""
        return self._stop.is_set()

    def stop(self) -> None:
        """Ask every loop to stop after its current step."""
        self._stop.set()

    def _install_signal_handlers(self) -> None:
        def handle(signum: int, _frame: Any) -> None:
            logger.info("signal %s received; finishing the current step", signum)
            self.stop()

        signal.signal(signal.SIGTERM, handle)
        signal.signal(signal.SIGINT, handle)

    def run(self, *, install_signal_handlers: bool = True) -> int:
        """Run ``concurrency`` loops until a stop is requested; return the exit code."""
        if install_signal_handlers and threading.current_thread() is threading.main_thread():
            self._install_signal_handlers()
        logger.info(
            "worker %s starting: kernel=%s gateway=%s provider=%s kinds=%s concurrency=%d",
            self.config.worker_id,
            self.config.kernel_url,
            self.config.gateway_url,
            self.provider.name,
            ",".join(self.config.kinds),
            self.config.concurrency,
        )
        threads = [
            threading.Thread(target=self._loop, name=f"kernos-loop-{index}", daemon=True)
            for index in range(self.config.concurrency)
        ]
        for thread in threads:
            thread.start()
        while any(thread.is_alive() for thread in threads):
            for thread in threads:
                thread.join(timeout=0.25)
        logger.info("worker %s stopped after %d step(s)", self.config.worker_id, len(self.outcomes))
        return EXIT_OK

    def _idle(self) -> None:
        low, high = self.config.idle_sleep
        self._stop.wait(random.uniform(low, high))

    def _lease(self) -> Lease | None:
        raw = self.kernel.lease(
            self.config.worker_id, list(self.config.kinds), self.config.lease_ttl
        )
        return Lease.model_validate(raw) if raw else None

    def run_once(self) -> StepOutcome | None:
        """Lease and execute at most one step; ``None`` when nothing is runnable."""
        lease = self._lease()
        if lease is None:
            return None
        return self._run_lease(lease)

    def _loop(self) -> None:
        backoff = 1.0
        while not self._stop.is_set():
            try:
                lease = self._lease()
            except KernosError as exc:
                logger.warning("lease request failed (%s); retrying in %.1f s", exc, backoff)
                self._stop.wait(backoff)
                backoff = min(backoff * 2, 30.0)
                continue
            backoff = 1.0
            if lease is None:
                self._idle()
                continue
            self._run_lease(lease)

    def _needs_input_schema(self, lease: Lease) -> bool:
        if lease.kind != "model":
            return False
        declared = set(lease.step_def.get("data_classes") or [])
        grants = set((lease.context.get("remit") or {}).get("grants") or [])
        return bool(declared - grants)

    def _input_schema(self, lease: Lease, log: StepLogger) -> dict[str, Any] | None:
        """The workflow's input schema, from the lease when the kernel provides it,
        else fetched once per bundle through the run and bundle endpoints."""
        provided = lease.context.get("input_schema")
        if isinstance(provided, dict):
            return provided
        workflow = str((lease.context.get("run") or {}).get("workflow", ""))
        try:
            run = self.kernel.get_run(lease.run_id)
            bundle_id = str((run.get("bundle") or {}).get("id", ""))
            cache_key = f"{bundle_id}:{workflow}"
            with self._lock:
                if cache_key in self._schema_cache:
                    return self._schema_cache[cache_key]
            document = self.kernel.get_bundle(bundle_id)
            bundle = document.get("bundle", document)
            schema = ((bundle.get("workflows") or {}).get(workflow) or {}).get("input_schema")
            with self._lock:
                self._schema_cache[cache_key] = schema if isinstance(schema, dict) else None
            return self._schema_cache[cache_key]
        except KernosError as exc:
            log.warning("could not fetch the input schema (%s); pattern rules only", exc)
            return None

    def _run_lease(self, lease: Lease) -> StepOutcome:
        log = step_logger(logger, lease.log_context(self.config.worker_id))
        log.info(
            "leased %s step attempt=%d pacing=%s",
            lease.kind,
            lease.attempt,
            bool(lease.context.get("pacing", False)),
        )
        abort = threading.Event()
        heartbeat = Heartbeat(self.kernel, lease, abort, log)
        heartbeat.start()
        deadline = time.monotonic() + lease.timeout_seconds
        try:
            schema = self._input_schema(lease, log) if self._needs_input_schema(lease) else None
            outcome = execute_step(
                self.runtime, lease, abort=abort, deadline=deadline, input_schema=schema
            )
        except KernosError as exc:
            log.error("step aborted, kernel unreachable or refused: %s", exc)
            outcome = StepOutcome(status="aborted", error={"code": exc.code, "message": str(exc)})
        except Exception as exc:  # a worker bug must not take the loop down
            log.exception("unexpected error in step")
            outcome = self._report_bug(lease, exc, log)
        finally:
            heartbeat.stop()
        with self._lock:
            self.outcomes.append(outcome)
        log.info("outcome %s", outcome.status)
        return outcome

    def _report_bug(self, lease: Lease, exc: Exception, log: StepLogger) -> StepOutcome:
        message = f"{type(exc).__name__}: {exc}"
        try:
            self.kernel.fail(lease.lease_id, "worker_error", message, deterministic=False)
        except KernosError as fail_exc:
            log.warning("could not report the failure: %s", fail_exc)
        return StepOutcome(
            status="failed", error={"code": "worker_error", "message": message}, deterministic=False
        )


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="kernos-worker", description="Lease and execute Kernos steps."
    )
    parser.add_argument("--kernel", dest="kernel_url", help=f"kernel url (env {ENV_KERNEL_URL})")
    parser.add_argument(
        "--gateway", dest="gateway_url", help=f"gateway url (env {ENV_GATEWAY_URL})"
    )
    parser.add_argument("--provider", choices=["mock", "anthropic"], help=f"env {ENV_PROVIDER}")
    parser.add_argument("--worker-id", dest="worker_id")
    parser.add_argument("--kinds", help="comma separated: model,tool,action,compensation")
    parser.add_argument("--lease-ttl", dest="lease_ttl", type=int)
    parser.add_argument("--concurrency", type=int)
    parser.add_argument("--token", help=f"bearer token (env {ENV_TOKEN})")
    parser.add_argument("--log-format", dest="log_format", choices=["text", "json"])
    parser.add_argument("--config", help="JSON file with the same keys as the flags")
    parser.add_argument("--verbose", action="store_true")
    return parser


def _load_config_file(path: str | None) -> dict[str, Any]:
    if not path:
        return {}
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    if not isinstance(data, dict):
        raise ValueError("config file must hold a JSON object")
    return data


def main(argv: Sequence[str] | None = None) -> int:
    """Entry point of ``kernos-worker``."""
    args = _parser().parse_args(argv)
    flags = {k: v for k, v in vars(args).items() if k not in ("config", "verbose")}
    try:
        config = WorkerConfig.from_sources(flags, config_file=_load_config_file(args.config))
        configure_logging(config.log_format, logging.DEBUG if args.verbose else logging.INFO)
        worker = Worker(config)
    except (ValueError, OSError, ProviderUnavailable) as exc:
        print(f"kernos-worker: configuration error: {exc}", file=sys.stderr)
        return EXIT_CONFIG
    return worker.run()


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())
