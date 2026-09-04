"""The worker loop: leasing, heartbeats, 410 handling, completion, SIGTERM."""

from __future__ import annotations

import json
import logging
import os
import signal
import threading
import time
from pathlib import Path

import pytest

from conftest import ACTION_STEP, EXTRACT_STEP, INPUT_SCHEMA, POST_STEP, CapturingProvider, Stub
from kernos._logging import step_logger
from kernos.providers.base import ModelResponse, Usage
from kernos.steps import Lease
from kernos.worker import DEFAULT_KINDS, Heartbeat, Worker, WorkerConfig, main

VALID_EXTRACT = {
    "vendor": "Northwind Dairy",
    "invoice_id": "inv-1001",
    "total": 7250.0,
    "currency": "USD",
}


def make_config(stub: Stub, **overrides: object) -> WorkerConfig:
    values: dict[str, object] = {
        "kernel_url": stub.kernel_url,
        "gateway_url": stub.gateway_url,
        "provider": "mock",
        "worker_id": "wrk-a1",
        "concurrency": 1,
        "lease_ttl": 5,
        "idle_sleep": (0.01, 0.02),
        "request_timeout": 5.0,
    }
    values.update(overrides)
    return WorkerConfig(**values)  # type: ignore[arg-type]


def wait_for(predicate, timeout: float = 5.0) -> bool:  # type: ignore[no-untyped-def]
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.01)
    return False


def test_run_once_leases_executes_heartbeats_and_completes(stub: Stub) -> None:
    stub.state.gateway_delay_s = 0.3
    stub.queue(POST_STEP, heartbeat_seconds=0.05)
    worker = Worker(make_config(stub))
    outcome = worker.run_once()
    assert outcome is not None and outcome.status == "completed"
    assert stub.state.lease_requests[0] == {
        "worker_id": "wrk-a1",
        "kinds": list(DEFAULT_KINDS),
        "ttl_seconds": 5,
    }
    assert len(stub.state.heartbeats) >= 2
    assert len(stub.state.completes) == 1
    assert worker.run_once() is None


def test_heartbeat_410_abandons_the_step_locally(stub: Stub) -> None:
    stub.state.gateway_delay_s = 0.5
    stub.queue(POST_STEP, heartbeat_seconds=0.05)
    stub.state.expire_after_heartbeats["lse_1"] = 1
    outcome = Worker(make_config(stub)).run_once()
    assert outcome is not None and outcome.status == "lease_lost"
    assert stub.state.completes == [] and stub.state.fails == []
    assert [e["kind"] for e in stub.state.events_of("run_1")] == ["tool.called"]
    assert len(stub.state.gateway_calls) == 1


def test_run_finishes_steps_then_stops_on_sigterm(stub: Stub) -> None:
    stub.queue(POST_STEP, lease_id="lse_1")
    stub.queue(ACTION_STEP, lease_id="lse_2")
    worker = Worker(make_config(stub, concurrency=2))

    def fire() -> None:
        wait_for(lambda: len(worker.outcomes) == 2)
        os.kill(os.getpid(), signal.SIGTERM)

    previous_term = signal.getsignal(signal.SIGTERM)
    previous_int = signal.getsignal(signal.SIGINT)
    threading.Thread(target=fire, daemon=True).start()
    try:
        code = worker.run()
    finally:
        signal.signal(signal.SIGTERM, previous_term)
        signal.signal(signal.SIGINT, previous_int)
    assert code == 0
    assert worker.stopping
    assert len(stub.state.completes) == 2
    assert {o.status for o in worker.outcomes} == {"completed"}


def test_stop_from_another_thread_exits_zero(stub: Stub) -> None:
    worker = Worker(make_config(stub))
    results: list[int] = []
    thread = threading.Thread(
        target=lambda: results.append(worker.run(install_signal_handlers=False))
    )
    thread.start()
    assert wait_for(lambda: len(stub.state.lease_requests) >= 2)
    worker.stop()
    thread.join(timeout=5)
    assert results == [0]


def test_lease_errors_back_off_and_the_loop_survives(stub: Stub) -> None:
    stub.state.lease_status = 500
    worker = Worker(make_config(stub))
    results: list[int] = []
    thread = threading.Thread(
        target=lambda: results.append(worker.run(install_signal_handlers=False))
    )
    thread.start()
    assert wait_for(lambda: len(stub.state.lease_requests) >= 1)
    worker.stop()
    thread.join(timeout=5)
    assert results == [0]


def test_kernel_unreachable_mid_step_is_logged_not_fatal(stub: Stub) -> None:
    worker = Worker(make_config(stub, kernel_url="http://127.0.0.1:9", request_timeout=0.5))
    lease = Lease.model_validate(
        {
            "lease_id": "lse_x",
            "run_id": "run_1",
            "step": "post",
            "step_def": POST_STEP,
            "context": {},
        }
    )
    outcome = worker._run_lease(lease)
    assert outcome.status == "aborted"
    assert outcome.error is not None and outcome.error["code"] == "network_error"


def test_worker_fetches_the_input_schema_once_per_bundle(stub: Stub) -> None:
    stub.state.runs["run_1"] = {
        "run_id": "run_1",
        "state": "running",
        "bundle": {"id": "bnd_1", "name": "halcyon.finance.invoice_intake", "version": "1.0.0"},
    }
    stub.state.bundle_docs["bnd_1"] = {
        "bundle_id": "bnd_1",
        "bundle": {"workflows": {"intake": {"input_schema": INPUT_SCHEMA}}},
    }
    step = {**EXTRACT_STEP, "data_classes": ["pii"]}
    stub.queue(step, lease_id="lse_1")
    stub.queue(step, lease_id="lse_2")
    provider = CapturingProvider(
        [
            ModelResponse(output=VALID_EXTRACT, usage=Usage(1, 1)),
            ModelResponse(output=VALID_EXTRACT, usage=Usage(1, 1)),
        ]
    )
    worker = Worker(make_config(stub), provider=provider)
    first = worker.run_once()
    second = worker.run_once()
    assert first is not None and first.status == "completed"
    assert second is not None and second.status == "completed"
    notes = [e["payload"] for e in stub.state.events_of("run_1", "note")]
    assert len(notes) == 2 and notes[0]["redacted"] == ["pii"]
    assert all("[REDACTED:contact_name]" in r.user for r in provider.requests)
    bundle_fetches = [r for r in stub.state.requests if r["path"] == "/v1/bundles/bnd_1"]
    assert len(bundle_fetches) == 1


def test_missing_input_schema_falls_back_to_patterns(stub: Stub) -> None:
    step = {**EXTRACT_STEP, "data_classes": ["pii"]}
    stub.queue(step)
    provider = CapturingProvider([ModelResponse(output=VALID_EXTRACT, usage=Usage(1, 1))])
    outcome = Worker(make_config(stub), provider=provider).run_once()
    assert outcome is not None and outcome.status == "completed"
    user = provider.requests[0].user
    assert "ana@halcyon.example" not in user
    assert "Ana Reyes" in user


def test_worker_bug_is_reported_as_non_deterministic_failure(stub: Stub) -> None:
    class Broken:
        name = "broken"

        def generate(self, request: object) -> ModelResponse:
            raise RuntimeError("provider bug")

    stub.queue(EXTRACT_STEP)
    outcome = Worker(make_config(stub), provider=Broken()).run_once()
    assert outcome is not None and outcome.status == "failed"
    assert stub.state.fails[0]["error"]["code"] == "worker_error"
    assert stub.state.fails[0]["deterministic"] is False


def test_heartbeat_thread_beats_and_stops(stub: Stub) -> None:
    lease = stub.lease(POST_STEP)
    abort = threading.Event()
    log = step_logger(logging.getLogger("test"), lease.log_context("wrk-a1"))
    heartbeat = Heartbeat(stub.kernel(), lease, abort, log, interval=0.02)
    heartbeat.start()
    assert wait_for(lambda: heartbeat.beats >= 2)
    heartbeat.stop()
    assert not abort.is_set()


def test_config_precedence(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    config_file = {"kernel_url": "http://file:1", "concurrency": 2, "kinds": ["tool"]}
    monkeypatch.setenv("KERNOS_KERNEL_URL", "http://env:1")
    monkeypatch.setenv("KERNOS_PROVIDER", "anthropic")
    config = WorkerConfig.from_sources(
        {"kernel_url": "http://flag:1", "kinds": "model,tool", "provider": None},
        config_file=config_file,
    )
    assert config.kernel_url == "http://flag:1"
    assert config.kinds == ("model", "tool")
    assert config.concurrency == 2
    assert config.provider == "anthropic"
    env_only = WorkerConfig.from_sources({}, config_file=config_file)
    assert env_only.kernel_url == "http://env:1" and env_only.kinds == ("tool",)


@pytest.mark.parametrize(
    "overrides",
    [
        {"kernel_url": "ftp://x"},
        {"kinds": ("model", "mystery")},
        {"kinds": ()},
        {"lease_ttl": 0},
        {"lease_ttl": 301},
        {"concurrency": 0},
        {"log_format": "xml"},
    ],
)
def test_config_validation(overrides: dict) -> None:
    with pytest.raises(ValueError):
        WorkerConfig(**overrides).validate()


def test_main_exits_2_on_configuration_errors(stub: Stub, tmp_path: Path) -> None:
    assert main(["--kernel", "ftp://x"]) == 2
    assert main(["--provider", "anthropic", "--kernel", stub.kernel_url]) == 2
    assert main(["--config", str(tmp_path / "missing.json")]) == 2
    bad = tmp_path / "bad.json"
    bad.write_text(json.dumps([1]), encoding="utf-8")
    assert main(["--config", str(bad)]) == 2
    assert main(["--kernel", stub.kernel_url, "--concurrency", "0"]) == 2
