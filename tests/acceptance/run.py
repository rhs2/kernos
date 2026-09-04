#!/usr/bin/env python3
"""Kernos acceptance suite orchestrator (see README.md in this directory).

Starts a kernel, the fake upstream, a gateway and mock-provider workers on the
test ports, loads the Halcyon Provisions reference bundle and policies, and runs
scenarios A1 to A13 against the kernel HTTP API and the event log.

This script builds nothing. It expects:

  KERNOS_BIN          the kernel binary          (default target/release/kernos)
  KERNOS_GATEWAY_BIN  the gateway binary         (default gateway/bin/kernos-gateway)
  KERNOS_WORKER_CMD   the worker command line    (default "kernos-worker" on PATH)
  KERNOS_EVAL_CMD     the eval command line      (default "kernos-eval" on PATH)
  node on PATH and sdk/typescript/dist built     (scenario A13 only)

Usage:
  python3 tests/acceptance/run.py                 run every scenario
  python3 tests/acceptance/run.py --only A6       run one (or A1,A2,... several)
  python3 tests/acceptance/run.py --keep          keep the temporary directories
  python3 tests/acceptance/run.py --fail-fast     stop at the first failure
  python3 tests/acceptance/run.py --list          print the scenarios

Prints one PASS/FAIL line per scenario with the elapsed time, then a summary.
Exits 0 when every selected scenario passed, 1 on a failure, 2 on a setup error.
On failure the events of the scenario's runs and the last 100 lines of every
component log are printed.

Python 3.10+ standard library plus httpx (installed by the Python SDK).
"""
from __future__ import annotations

import argparse
import copy
import json
import os
import re
import shlex
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
import traceback
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Iterable

try:
    import httpx
except ImportError:  # pragma: no cover
    print("run.py: httpx is required (pip install httpx, or install the Python SDK)", file=sys.stderr)
    sys.exit(2)

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
FIXTURES = HERE / "fixtures"
BUNDLE_DIR = ROOT / "bundles" / "reference" / "halcyon"
SDK_TS_DIST = ROOT / "sdk" / "typescript" / "dist" / "index.js"

KERNEL_PORT = 17401
GATEWAY_PORT = 17402
UPSTREAM_PORT = 17499
TAMPER_KERNEL_PORT = 17411
KERNEL_URL = f"http://127.0.0.1:{KERNEL_PORT}"
GATEWAY_URL = f"http://127.0.0.1:{GATEWAY_PORT}"
UPSTREAM_URL = f"http://127.0.0.1:{UPSTREAM_PORT}"
PROBE_URL = f"{UPSTREAM_URL}/probe"

LEASE_TTL = 3           # 09: lease TTL 3 s (02 clamps to 5..300; both are allowed for below)
LEASE_TTL_CLAMP_MIN = 5
SWEEP_MS = 500
APPROVAL_SWEEP_MS = 500
CANARY_INTERVAL_S = 2
CANARY_QUARANTINE_AFTER = 2

BUNDLE_NAME = "halcyon.finance.invoice_intake"
BUNDLE_VERSION = "1.0.0"
POLICIES = [
    ("finance-default", BUNDLE_DIR / "policies" / "finance-default.policy"),
    ("finance-test", BUNDLE_DIR / "policies" / "finance-test.policy"),
    ("finance-default-10k", BUNDLE_DIR / "policies" / "finance-default-10k.policy"),
]

REQUESTED_BY = {"id": "u-ana", "role": "ap_clerk", "manager": "u-tom"}
ANA = {"id": "u-ana", "role": "ap_clerk"}
TOM = {"id": "u-tom", "role": "finance_admin"}
DEFAULT_TOOLS = ["ledger.*", "http.get", "test.*"]
DEFAULT_SCOPES = ["sql:table:ledger_entries", "sql:table:vendors", "http:host:127.0.0.1", "test:*"]
DEFAULT_SPEND = {"tokens": 200000, "usd": 2.0}
ACCOUNTS = ["5100", "5200", "5300", "6100"]
DEFAULT_WORKERS = ("wrk-a", "wrk-b")
WORKER_KINDS = "model,tool,action,compensation"
INVOICE_MARKER = "Northwind Dairy"

TERMINAL_STATES = {"completed", "failed", "abandoned"}


# ---------------------------------------------------------------------------
# Small helpers
# ---------------------------------------------------------------------------

class Failure(AssertionError):
    """A scenario assertion failed."""


class SetupError(RuntimeError):
    """The harness could not be brought up."""


def check(cond: Any, message: str) -> None:
    if not cond:
        raise Failure(message)


def now() -> float:
    return time.monotonic()


def short(obj: Any, limit: int = 300) -> str:
    text = json.dumps(obj, separators=(",", ":"), default=str)
    return text if len(text) <= limit else text[:limit] + "..."


def deep_get(obj: Any, path: str) -> Any:
    cur = obj
    for seg in path.split("."):
        if isinstance(cur, dict):
            cur = cur.get(seg)
        elif isinstance(cur, list) and seg.isdigit():
            idx = int(seg)
            cur = cur[idx] if idx < len(cur) else None
        else:
            return None
    return cur


def find(events: list[dict], kind: str, where: Callable[[dict], bool] | None = None) -> dict | None:
    for ev in events:
        if ev.get("kind") == kind and (where is None or where(ev.get("payload") or {})):
            return ev
    return None


def find_all(events: list[dict], kind: str, where: Callable[[dict], bool] | None = None) -> list[dict]:
    return [ev for ev in events if ev.get("kind") == kind and (where is None or where(ev.get("payload") or {}))]


def approver_is(value: Any, typ: str, name: str) -> bool:
    """Accept both approver forms: {"type": "role", "value": "x"} and "role:x" (or a bare user id)."""
    if isinstance(value, dict):
        return value.get("type") == typ and value.get("value") == name
    if isinstance(value, str):
        if value == f"{typ}:{name}":
            return True
        return typ == "user" and value == name
    return False


def approver_text(value: Any) -> str:
    if isinstance(value, dict):
        return f"{value.get('type')}:{value.get('value')}"
    return str(value)


def validate_schema(value: Any, schema: dict, path: str = "$") -> list[str]:
    """A tiny JSON Schema subset validator (type, required, properties, items, enum, bounds)."""
    problems: list[str] = []
    typ = schema.get("type")
    type_map = {"object": dict, "array": list, "string": str, "boolean": bool, "null": type(None)}
    if typ is not None:
        types = typ if isinstance(typ, list) else [typ]
        ok = False
        for t in types:
            if t == "number" and isinstance(value, (int, float)) and not isinstance(value, bool):
                ok = True
            elif t == "integer" and isinstance(value, int) and not isinstance(value, bool):
                ok = True
            elif t in type_map and isinstance(value, type_map[t]):
                ok = True
        if not ok:
            problems.append(f"{path}: expected type {typ}, got {type(value).__name__}")
            return problems
    if "enum" in schema and value not in schema["enum"]:
        problems.append(f"{path}: {value!r} not in enum")
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        if "minimum" in schema and value < schema["minimum"]:
            problems.append(f"{path}: {value} below minimum {schema['minimum']}")
        if "maximum" in schema and value > schema["maximum"]:
            problems.append(f"{path}: {value} above maximum {schema['maximum']}")
    if isinstance(value, str):
        if "minLength" in schema and len(value) < schema["minLength"]:
            problems.append(f"{path}: shorter than minLength")
        if "maxLength" in schema and len(value) > schema["maxLength"]:
            problems.append(f"{path}: longer than maxLength")
        if "pattern" in schema and not re.search(schema["pattern"], value):
            problems.append(f"{path}: does not match pattern")
    if isinstance(value, dict):
        for req in schema.get("required", []):
            if req not in value:
                problems.append(f"{path}: missing required {req!r}")
        for key, sub in (schema.get("properties") or {}).items():
            if key in value:
                problems.extend(validate_schema(value[key], sub, f"{path}.{key}"))
        if schema.get("additionalProperties") is False:
            for key in value:
                if key not in (schema.get("properties") or {}):
                    problems.append(f"{path}: unexpected property {key!r}")
    if isinstance(value, list) and isinstance(schema.get("items"), dict):
        for i, item in enumerate(value):
            problems.extend(validate_schema(item, schema["items"], f"{path}[{i}]"))
    return problems


def tail_lines(path: Path, n: int = 100) -> list[str]:
    try:
        with open(path, "rb") as fh:
            data = fh.read()
    except OSError:
        return []
    lines = data.decode("utf-8", errors="replace").splitlines()
    return lines[-n:]


def wait_until(pred: Callable[[], Any], timeout: float, interval: float = 0.25, what: str = "condition") -> Any:
    deadline = now() + timeout
    last_exc: Exception | None = None
    while True:
        try:
            result = pred()
            if result:
                return result
        except (httpx.HTTPError, sqlite3.Error, OSError, ValueError) as exc:
            last_exc = exc
        if now() >= deadline:
            detail = f" (last error: {last_exc})" if last_exc else ""
            raise Failure(f"timed out after {timeout:.1f}s waiting for {what}{detail}")
        time.sleep(interval)


# ---------------------------------------------------------------------------
# Processes
# ---------------------------------------------------------------------------

@dataclass
class Proc:
    name: str
    argv: list[str]
    log_path: Path
    env: dict[str, str]
    cwd: Path | None = None
    popen: subprocess.Popen | None = field(default=None, init=False)
    log_file: Any = field(default=None, init=False)

    def start(self) -> "Proc":
        self.log_file = open(self.log_path, "ab")
        self.log_file.write(f"# {self.name}: {' '.join(shlex.quote(a) for a in self.argv)}\n".encode())
        self.log_file.flush()
        try:
            self.popen = subprocess.Popen(
                self.argv,
                env=self.env,
                cwd=str(self.cwd) if self.cwd else None,
                stdout=self.log_file,
                stderr=subprocess.STDOUT,
                stdin=subprocess.DEVNULL,
                start_new_session=True,
            )
        except OSError as exc:
            raise SetupError(f"{self.name}: cannot start {self.argv[0]}: {exc}") from exc
        return self

    @property
    def alive(self) -> bool:
        return self.popen is not None and self.popen.poll() is None

    @property
    def pid(self) -> int:
        return self.popen.pid if self.popen else -1

    def kill(self) -> None:
        """SIGKILL, no grace."""
        if self.alive and self.popen:
            try:
                os.killpg(self.popen.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            self.popen.wait(timeout=10)
        self._close_log()

    def terminate(self, grace: float = 5.0) -> None:
        """SIGTERM, then SIGKILL after grace seconds."""
        if self.alive and self.popen:
            try:
                os.killpg(self.popen.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                self.popen.wait(timeout=grace)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(self.popen.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                self.popen.wait(timeout=10)
        self._close_log()

    def _close_log(self) -> None:
        if self.log_file:
            try:
                self.log_file.close()
            except OSError:
                pass
            self.log_file = None


# ---------------------------------------------------------------------------
# Harness
# ---------------------------------------------------------------------------

class Harness:
    """Owns the processes, the temporary directories and the kernel API helpers."""

    def __init__(self, keep: bool, verbose: bool = False) -> None:
        self.keep = keep
        self.verbose = verbose
        self.session = uuid.uuid4().hex[:6]
        self.work = Path(tempfile.mkdtemp(prefix="kernos-accept-"))
        self.logs = self.work / "logs"
        self.logs.mkdir()
        self.kernel_data = self.work / "kernel-data"
        self.gateway_data = self.work / "gateway-data"
        self.ledger_db = self.work / "halcyon-ledger.db"
        self.kernos_bin = Path(os.environ.get("KERNOS_BIN") or ROOT / "target" / "release" / "kernos")
        self.gateway_bin = Path(os.environ.get("KERNOS_GATEWAY_BIN") or ROOT / "gateway" / "bin" / "kernos-gateway")
        self.worker_cmd = shlex.split(os.environ.get("KERNOS_WORKER_CMD") or "kernos-worker")
        self.eval_cmd = shlex.split(os.environ.get("KERNOS_EVAL_CMD") or "kernos-eval")
        self.http = httpx.Client(base_url=KERNEL_URL, timeout=15.0)
        self.gw = httpx.Client(base_url=GATEWAY_URL, timeout=20.0)
        self.up = httpx.Client(base_url=UPSTREAM_URL, timeout=5.0)
        self.procs: list[Proc] = []
        self.workers: dict[str, Proc] = {}
        self.worker_env_extra: dict[str, str] = {}
        self.bundle_id: str | None = None
        self.bundle = json.loads((BUNDLE_DIR / "bundle.json").read_text(encoding="utf-8"))
        self.all_runs: list[str] = []
        self.scenario_runs: list[str] = []
        self.a1_run_id: str | None = None
        self.notes: list[str] = []

    # -- logging -------------------------------------------------------------

    def log(self, message: str) -> None:
        print(f"    {message}", flush=True)

    def note(self, message: str) -> None:
        """Something the integrator should know that is not a failure."""
        self.notes.append(message)
        print(f"    note: {message}", flush=True)

    # -- setup -----------------------------------------------------------------

    def setup(self) -> None:
        self._check_prerequisites()
        self.kernel_data.mkdir()
        self.gateway_data.mkdir()
        shutil.copy(BUNDLE_DIR / "directory.json", self.kernel_data / "directory.json")

        self.log("generating and trusting a throwaway publisher key")
        self.cli("keys", "generate", "--out", str(self.work / "publisher"), server=False)
        pub = self._key_file(self.work / "publisher", ".pub")
        self.cli("keys", "trust", str(pub), server=False)

        self.kernel = self.start_kernel("kernel", KERNEL_PORT, self.kernel_data)
        self.upstream = self.start_upstream()
        self.init_ledger()
        self.gateway = self.start_gateway()

        self.log("signing and applying the reference bundle")
        self.bundle_id = self.apply_bundle()
        self.log(f"bundle_id {self.bundle_id}")
        for name, path in POLICIES:
            self.cli("policy", "apply", str(path), "--name", name, "--version", "1")
        self.log("policies applied: " + ", ".join(n for n, _ in POLICIES))

        self.restart_workers()

    def _check_prerequisites(self) -> None:
        problems = []
        if not self.kernos_bin.exists():
            problems.append(f"kernel binary not found at {self.kernos_bin} (set KERNOS_BIN)")
        if not self.gateway_bin.exists():
            problems.append(f"gateway binary not found at {self.gateway_bin} (set KERNOS_GATEWAY_BIN)")
        if shutil.which(self.worker_cmd[0]) is None and not Path(self.worker_cmd[0]).exists():
            problems.append(f"worker command {self.worker_cmd[0]!r} not found on PATH (set KERNOS_WORKER_CMD)")
        if shutil.which(self.eval_cmd[0]) is None and not Path(self.eval_cmd[0]).exists():
            problems.append(f"eval command {self.eval_cmd[0]!r} not found on PATH (set KERNOS_EVAL_CMD); A12 needs it")
        for port in (KERNEL_PORT, GATEWAY_PORT, UPSTREAM_PORT, TAMPER_KERNEL_PORT):
            if self._port_in_use(port):
                problems.append(f"port {port} is already in use")
        if problems:
            raise SetupError("\n".join(problems))

    @staticmethod
    def _port_in_use(port: int) -> bool:
        import socket
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
            s.settimeout(0.2)
            return s.connect_ex(("127.0.0.1", port)) == 0

    @staticmethod
    def _key_file(base: Path, suffix: str) -> Path:
        """`kernos keys generate --out base` writes base.key and base.pub, or a directory."""
        direct = base.with_suffix(suffix)
        if direct.exists():
            return direct
        if base.is_dir():
            found = sorted(base.glob(f"*{suffix}"))
            if found:
                return found[0]
        raise SetupError(f"key file with suffix {suffix} not found for {base}")

    def base_env(self) -> dict[str, str]:
        env = dict(os.environ)
        env["KERNOS_DATA"] = str(self.kernel_data)
        env.setdefault("PYTHONUNBUFFERED", "1")
        return env

    def cli(self, *args: str, server: bool = True, env: dict[str, str] | None = None, check_exit: bool = True) -> Any:
        """Run the kernos CLI with --json and return the parsed output (or raw text)."""
        argv = [str(self.kernos_bin), *args, "--json"]
        if server:
            argv += ["--server", KERNEL_URL]
        full_env = self.base_env()
        if env:
            full_env.update(env)
        res = subprocess.run(argv, env=full_env, capture_output=True, text=True, timeout=60)
        with open(self.logs / "cli.log", "a", encoding="utf-8") as fh:
            fh.write(f"$ {' '.join(shlex.quote(a) for a in argv)}\n[exit {res.returncode}]\n{res.stdout}{res.stderr}\n")
        if check_exit and res.returncode != 0:
            raise SetupError(f"kernos {' '.join(args)} failed (exit {res.returncode}):\n{res.stdout}{res.stderr}")
        text = res.stdout.strip()
        try:
            return json.loads(text) if text else None
        except json.JSONDecodeError:
            return text

    def start_kernel(self, name: str, port: int, data_dir: Path) -> Proc:
        env = self.base_env()
        env.update({
            "KERNOS_DATA": str(data_dir),
            "KERNOS_LISTEN": f"127.0.0.1:{port}",
            "KERNOS_LEASE_TTL": str(LEASE_TTL),
            "KERNOS_SWEEP_MS": str(SWEEP_MS),
            "KERNOS_APPROVAL_SWEEP_MS": str(APPROVAL_SWEEP_MS),
        })
        proc = Proc(name, [str(self.kernos_bin), "serve", "--listen", f"127.0.0.1:{port}", "--data", str(data_dir)],
                    self.logs / f"{name}.log", env).start()
        self.procs.append(proc)
        self._wait_http(proc, f"http://127.0.0.1:{port}/v1/health", 30)
        self.log(f"{name} up on 127.0.0.1:{port} (pid {proc.pid})")
        return proc

    def start_upstream(self) -> Proc:
        proc = Proc("upstream", [sys.executable, str(FIXTURES / "fake_upstream.py"), "--port", str(UPSTREAM_PORT)],
                    self.logs / "upstream.log", self.base_env()).start()
        self.procs.append(proc)
        self._wait_http(proc, f"{UPSTREAM_URL}/health", 15)
        self.log(f"fake upstream up on 127.0.0.1:{UPSTREAM_PORT}")
        return proc

    def init_ledger(self) -> None:
        if self.ledger_db.exists():
            self.ledger_db.unlink()
        con = sqlite3.connect(self.ledger_db)
        try:
            con.executescript((BUNDLE_DIR / "ledger.sql").read_text(encoding="utf-8"))
            con.commit()
        finally:
            con.close()

    def start_gateway(self) -> Proc:
        env = self.base_env()
        env.update({
            "KERNOS_GATEWAY_TEST_TOOLS": "1",
            "KERNOS_GATEWAY_DATA": str(self.gateway_data),
            "HALCYON_LEDGER_DB": str(self.ledger_db),
            "KERNOS_KERNEL_URL": KERNEL_URL,
            "KERNOS_CANARY_INTERVAL": str(CANARY_INTERVAL_S),
            "KERNOS_CANARY_QUARANTINE_AFTER": str(CANARY_QUARANTINE_AFTER),
        })
        proc = Proc("gateway", [str(self.gateway_bin), "--config", str(BUNDLE_DIR / "gateway.json")],
                    self.logs / "gateway.log", env).start()
        self.procs.append(proc)
        self._wait_http(proc, f"{GATEWAY_URL}/v1/health", 30)
        self.log(f"gateway up on 127.0.0.1:{GATEWAY_PORT} (pid {proc.pid})")
        return proc

    def _wait_http(self, proc: Proc, url: str, timeout: float) -> None:
        deadline = now() + timeout
        while now() < deadline:
            if not proc.alive:
                raise SetupError(f"{proc.name} exited early; log tail:\n" + "\n".join(tail_lines(proc.log_path, 40)))
            try:
                r = httpx.get(url, timeout=2.0)
                if r.status_code == 200:
                    return
            except httpx.HTTPError:
                pass
            time.sleep(0.2)
        raise SetupError(f"{proc.name} did not answer on {url} within {timeout}s; log tail:\n" + "\n".join(tail_lines(proc.log_path, 40)))

    def apply_bundle(self) -> str:
        key = self._key_file(self.work / "publisher", ".key")
        sig_path = self.work / "bundle.sig.json"
        self.cli("bundle", "sign", str(BUNDLE_DIR / "bundle.json"), "--key", str(key), "--out", str(sig_path), server=False)
        out = self.cli("bundle", "apply", str(BUNDLE_DIR / "bundle.json"), "--sig", str(sig_path))
        bundle_id = out.get("bundle_id") if isinstance(out, dict) else None
        if not bundle_id:
            for b in self.api("GET", "/v1/bundles", expect=200).json():
                if b.get("name") == BUNDLE_NAME and b.get("version") == BUNDLE_VERSION:
                    bundle_id = b["bundle_id"]
        if not bundle_id:
            raise SetupError(f"could not determine the bundle id after apply; CLI output: {out!r}")
        return bundle_id

    # -- workers ---------------------------------------------------------------

    def start_worker(self, worker_id: str, extra_env: dict[str, str] | None = None) -> Proc:
        env = self.base_env()
        env.update(extra_env or {})
        argv = [*self.worker_cmd, "--kernel", KERNEL_URL, "--gateway", GATEWAY_URL, "--provider", "mock",
                "--worker-id", worker_id, "--kinds", WORKER_KINDS, "--lease-ttl", str(LEASE_TTL), "--concurrency", "1"]
        proc = Proc(f"worker-{worker_id}", argv, self.logs / f"worker-{worker_id}.log", env).start()
        self.procs.append(proc)
        self.workers[worker_id] = proc
        time.sleep(0.3)
        if not proc.alive:
            raise SetupError(f"worker {worker_id} exited at once; log tail:\n" + "\n".join(tail_lines(proc.log_path, 40)))
        return proc

    def stop_worker(self, worker_id: str, kill: bool = False) -> None:
        proc = self.workers.pop(worker_id, None)
        if proc is None:
            return
        if kill:
            proc.kill()
        else:
            proc.terminate()

    def stop_workers(self) -> None:
        for wid in list(self.workers):
            self.stop_worker(wid)

    def restart_workers(self, extra_env: dict[str, str] | None = None, ids: Iterable[str] = DEFAULT_WORKERS) -> None:
        self.stop_workers()
        self.worker_env_extra = dict(extra_env or {})
        for wid in ids:
            self.start_worker(wid, self.worker_env_extra)
        label = f" with {self.worker_env_extra}" if self.worker_env_extra else ""
        self.log(f"workers running: {', '.join(self.workers)}{label}")

    def ensure_default_workers(self) -> None:
        if set(self.workers) != set(DEFAULT_WORKERS) or self.worker_env_extra or not all(p.alive for p in self.workers.values()):
            self.restart_workers()

    # -- kernel API ------------------------------------------------------------

    def api(self, method: str, path: str, body: Any = None, expect: int | tuple[int, ...] | None = None,
            headers: dict[str, str] | None = None) -> httpx.Response:
        res = self.http.request(method, path, json=body, headers=headers)
        if expect is not None:
            allowed = (expect,) if isinstance(expect, int) else expect
            if res.status_code not in allowed:
                raise Failure(f"{method} {path} returned {res.status_code}, expected {allowed}: {res.text[:400]}")
        return res

    def error_code(self, res: httpx.Response) -> str:
        try:
            return str(res.json().get("error", {}).get("code"))
        except (ValueError, AttributeError):
            return ""

    def error_details(self, res: httpx.Response) -> dict:
        try:
            return dict(res.json().get("error", {}).get("details") or {})
        except (ValueError, AttributeError):
            return {}

    def events(self, run_id: str) -> list[dict]:
        out: list[dict] = []
        from_seq = 1
        for _ in range(1000):
            page = self.http.get(f"/v1/runs/{run_id}/events", params={"from_seq": from_seq, "limit": 500})
            if page.status_code != 200:
                raise Failure(f"GET events for {run_id} returned {page.status_code}: {page.text[:200]}")
            body = page.json()
            evs = body.get("events") or []
            out.extend(evs)
            nxt = body.get("next_seq")
            if not evs or nxt is None or nxt <= from_seq:
                break
            from_seq = max(nxt, out[-1]["seq"] + 1)
        return out

    def run_state(self, run_id: str) -> dict:
        return self.api("GET", f"/v1/runs/{run_id}", expect=200).json()

    def wait_for_event(self, run_id: str, kind: str, timeout: float = 30.0,
                       where: Callable[[dict], bool] | None = None, after_seq: int = 0) -> dict:
        deadline = now() + timeout
        seen: set[str] = set()
        while True:
            evs = self.events(run_id)
            for ev in evs:
                seen.add(ev.get("kind", "?"))
                if ev.get("kind") == kind and ev.get("seq", 0) > after_seq and (where is None or where(ev.get("payload") or {})):
                    return ev
            if now() >= deadline:
                raise Failure(f"run {run_id}: no {kind} event within {timeout:.0f}s (kinds seen: {', '.join(sorted(seen))})")
            time.sleep(0.25)

    def wait_for_state(self, run_id: str, states: Iterable[str], timeout: float = 60.0) -> dict:
        wanted = set(states)
        deadline = now() + timeout
        while True:
            st = self.run_state(run_id)
            if st.get("state") in wanted:
                return st
            if st.get("state") in TERMINAL_STATES and st.get("state") not in wanted:
                raise Failure(f"run {run_id} ended {st.get('state')} (wanted {sorted(wanted)}): error={short(st.get('error'))}")
            if now() >= deadline:
                raise Failure(f"run {run_id} still {st.get('state')} after {timeout:.0f}s (wanted {sorted(wanted)})")
            time.sleep(0.3)

    def issue_remit(self, **overrides: Any) -> dict:
        body = {
            "tools": list(DEFAULT_TOOLS),
            "scopes": list(DEFAULT_SCOPES),
            "grants": [],
            "spend": dict(DEFAULT_SPEND),
            "autonomy": "autonomous",
            "ttl_seconds": 3600,
            "policy_set": ["finance-default"],
            "requested_by": dict(REQUESTED_BY),
        }
        body.update(overrides)
        return self.api("POST", "/v1/remits", body, expect=201).json()

    def invoice(self, tag: str, total: float, probe_url: str | None = None) -> dict:
        invoice_id = f"INV-{tag.upper()}-{self.session}"
        text = (
            f"NORTHWIND DAIRY\nInvoice {invoice_id}\nBill to: Halcyon Provisions, Accounts Payable\n"
            f"Whole milk and cream delivered to the Harbor Street depot.\nTerms: net 30\n"
            f"Supplier legal name: {INVOICE_MARKER}\nTotal due: USD {total:,.2f}\n"
        )
        inp: dict[str, Any] = {"invoice_id": invoice_id, "text": text, "total": total, "accounts": list(ACCOUNTS)}
        if probe_url is not None:
            inp["probe_url"] = probe_url
        return inp

    def start_run(self, workflow: str, inp: dict, remit_id: str | None = None, requested_by: dict | None = None) -> str:
        if remit_id is None:
            remit_id = self.issue_remit()["remit_id"]
        body = {"bundle_id": self.bundle_id, "workflow": workflow, "input": inp,
                "remit_id": remit_id, "requested_by": requested_by or dict(REQUESTED_BY)}
        run_id = self.api("POST", "/v1/runs", body, expect=201).json()["run_id"]
        self.all_runs.append(run_id)
        self.scenario_runs.append(run_id)
        self.log(f"run {run_id} started ({workflow}, invoice {inp.get('invoice_id')})")
        return run_id

    def ledger_rows(self, invoice_id: str) -> list[dict]:
        con = sqlite3.connect(f"file:{self.ledger_db}?mode=ro", uri=True)
        try:
            con.row_factory = sqlite3.Row
            rows = con.execute("select * from ledger_entries where invoice_id = ? order by id", (invoice_id,)).fetchall()
            return [dict(r) for r in rows]
        finally:
            con.close()

    def upstream_mode(self, mode: str) -> None:
        r = self.up.post("/control", json={"mode": mode})
        check(r.status_code == 200, f"fake upstream refused mode {mode}: {r.status_code} {r.text}")
        self.log(f"fake upstream mode: {mode}")

    def canary(self, connector: str) -> dict | None:
        r = self.gw.get("/v1/canaries")
        if r.status_code != 200:
            return None
        for c in r.json():
            if c.get("connector") == connector:
                return c
        return None

    # -- failure reporting -----------------------------------------------------

    def dump_failure(self) -> None:
        for run_id in self.scenario_runs:
            print(f"----- events of {run_id} -----")
            try:
                for ev in self.events(run_id):
                    print(json.dumps(ev, separators=(",", ":"), default=str))
            except Exception as exc:  # noqa: BLE001
                print(f"(could not read events: {exc})")
        for proc in self.procs:
            print(f"----- last 100 lines of {proc.log_path.name} -----")
            for line in tail_lines(proc.log_path, 100):
                print(line)
        cli_log = self.logs / "cli.log"
        if cli_log.exists():
            print("----- last 100 lines of cli.log -----")
            for line in tail_lines(cli_log, 100):
                print(line)

    # -- teardown --------------------------------------------------------------

    def teardown(self) -> None:
        for proc in reversed(self.procs):
            try:
                proc.terminate(grace=5.0)
            except Exception:  # noqa: BLE001
                pass
        for client in (self.http, self.gw, self.up):
            client.close()
        if self.keep:
            print(f"keeping {self.work}")
        else:
            shutil.rmtree(self.work, ignore_errors=True)


# ---------------------------------------------------------------------------
# Scenarios
# ---------------------------------------------------------------------------

def a1(h: Harness) -> None:
    """A run killed mid-step resumes and finishes (phase 1)."""
    h.stop_workers()
    h.start_worker("wrk-1")
    inp = h.invoice("a1", 1200.0)
    run_id = h.start_run("intake_slow", inp)
    h.a1_run_id = run_id

    leased = h.wait_for_event(run_id, "step.leased", 90, lambda p: p.get("step") == "wait")
    check(leased["payload"].get("worker_id") == "wrk-1", f"wait step leased by {leased['payload'].get('worker_id')}, expected wrk-1")
    time.sleep(0.8)  # let the worker enter the gateway call of test.slow
    h.stop_worker("wrk-1", kill=True)
    killed_at = now()
    h.log("wrk-1 killed with SIGKILL inside the wait step")

    expired = h.wait_for_event(run_id, "step.lease_expired", 20, lambda p: p.get("step") == "wait", after_seq=leased["seq"])
    elapsed = now() - killed_at
    allowed = LEASE_TTL_CLAMP_MIN + SWEEP_MS / 1000 + 2.0
    check(elapsed <= allowed, f"lease expired {elapsed:.1f}s after the kill; expected within TTL plus sweep ({allowed:.1f}s)")
    check(expired["payload"].get("lease_id") == leased["payload"].get("lease_id"), "lease_expired names a different lease")
    h.log(f"lease expired {elapsed:.1f}s after the kill")

    h.start_worker("wrk-2")
    released = h.wait_for_event(run_id, "step.leased", 30, lambda p: p.get("step") == "wait" and p.get("worker_id") == "wrk-2", after_seq=expired["seq"])
    check(released["payload"].get("attempt", 0) >= 1, "re-lease carries no attempt number")
    h.wait_for_state(run_id, {"completed"}, 90)

    rows = h.ledger_rows(inp["invoice_id"])
    check(len(rows) == 1, f"expected exactly one ledger row for {inp['invoice_id']}, found {len(rows)}")
    evs = h.events(run_id)
    ok_posts = find_all(evs, "tool.result", lambda p: p.get("tool") == "ledger.post_entry" and p.get("ok") is True)
    check(len(ok_posts) == 1, f"expected exactly one ok tool.result for ledger.post_entry, found {len(ok_posts)}")


def _consistent_copy_of_kernel_data(h: Harness) -> Path:
    copy_dir = h.work / "kernel-data-tampered"
    if copy_dir.exists():
        shutil.rmtree(copy_dir)
    shutil.copytree(h.kernel_data, copy_dir)
    for stale in copy_dir.glob("*.db-*"):
        stale.unlink()
    db_copy = copy_dir / "kernos.db"
    if db_copy.exists():
        db_copy.unlink()
    src = sqlite3.connect(f"file:{h.kernel_data / 'kernos.db'}?mode=ro", uri=True)
    dst = sqlite3.connect(db_copy)
    try:
        src.backup(dst)
    finally:
        dst.close()
        src.close()
    return copy_dir


def _tamper_one_byte(db_path: Path, run_id: str) -> tuple[int, str]:
    """Change one byte inside one stored event of run_id. Returns (seq or 0, where)."""
    marker = INVOICE_MARKER
    tampered = marker[:-1] + ("z" if marker[-1] != "z" else "y")
    con = sqlite3.connect(db_path)
    try:
        tables = [r[0] for r in con.execute("select name from sqlite_master where type = 'table'")]
        ordered = sorted(tables, key=lambda t: (0 if "event" in t.lower() else 1, t))
        for table in ordered:
            cols = [r[1] for r in con.execute(f'pragma table_info("{table}")')]
            has_run = "run_id" in cols
            sql = f'select rowid, * from "{table}"' + (" where run_id = ?" if has_run else "")
            rows = con.execute(sql, (run_id,) if has_run else ()).fetchall()
            for row in rows:
                rowid, values = row[0], dict(zip(cols, row[1:]))
                if not has_run and not any(isinstance(v, (str, bytes)) and run_id in (v if isinstance(v, str) else v.decode("utf-8", "replace")) for v in values.values()):
                    continue
                for col, val in values.items():
                    if isinstance(val, str) and marker in val:
                        con.execute(f'update "{table}" set "{col}" = ? where rowid = ?', (val.replace(marker, tampered, 1), rowid))
                    elif isinstance(val, bytes) and marker.encode() in val:
                        con.execute(f'update "{table}" set "{col}" = ? where rowid = ?', (val.replace(marker.encode(), tampered.encode(), 1), rowid))
                    else:
                        continue
                    con.commit()
                    seq = values.get("seq")
                    return (int(seq) if isinstance(seq, int) else 0, f"{table}.{col} rowid {rowid}")
        raise Failure(f"could not find the marker {marker!r} of run {run_id} in any table of {db_path.name} (tables: {', '.join(tables)}); "
                      "the tamper step assumes events are stored as plain JSON text or bytes")
    finally:
        con.close()


def a2(h: Harness) -> None:
    """Replay reproduces every decision; a tampered byte breaks the chain (phase 1)."""
    run_id = h.a1_run_id
    if run_id is None:
        inp = h.invoice("a2", 1100.0)
        run_id = h.start_run("intake", inp)
        h.wait_for_state(run_id, {"completed"}, 90)
    else:
        h.scenario_runs.append(run_id)

    rep = h.api("POST", f"/v1/runs/{run_id}/replay", expect=200).json()
    check(rep.get("chain_valid") is True, f"chain_valid is {rep.get('chain_valid')}: {short(rep.get('chain_errors'))}")
    check(rep.get("state_matches") is True, f"state_matches is {rep.get('state_matches')}")
    check(rep.get("decision_mismatches") == [], f"decision_mismatches: {short(rep.get('decision_mismatches'))}")
    decided = find_all(h.events(run_id), "policy.decided")
    check(rep.get("decisions") == len(decided), f"replay reports {rep.get('decisions')} decisions, log has {len(decided)} policy.decided events")
    h.log(f"replay ok: {rep.get('events')} events, {rep.get('decisions')} decisions")

    copy_dir = _consistent_copy_of_kernel_data(h)
    seq, where = _tamper_one_byte(copy_dir / "kernos.db", run_id)
    h.log(f"tampered one byte in {where} (seq {seq or 'unknown'})")

    tamper_kernel = h.start_kernel("kernel-tamper", TAMPER_KERNEL_PORT, copy_dir)
    try:
        r = httpx.post(f"http://127.0.0.1:{TAMPER_KERNEL_PORT}/v1/runs/{run_id}/replay", timeout=30.0)
        check(r.status_code == 200, f"tampered replay returned {r.status_code}: {r.text[:300]}")
        rep2 = r.json()
        check(rep2.get("chain_valid") is False, "tampered log still reports chain_valid true")
        errors = rep2.get("chain_errors") or []
        check(len(errors) > 0, "chain_valid false but chain_errors is empty")
        if seq:
            named = any((e.get("seq") == seq) if isinstance(e, dict) else (str(seq) in str(e)) for e in errors)
            check(named, f"chain_errors do not name seq {seq}: {short(errors)}")
        else:
            h.note(f"tampered table has no seq column; chain_errors were {short(errors)}")
        h.log(f"tampered replay: chain_valid false, chain_errors {short(errors)}")
    finally:
        tamper_kernel.terminate()
        h.procs.remove(tamper_kernel)


def a3(h: Harness) -> None:
    """An abandoned run unwinds its completed writes (phase 1)."""
    inp = h.invoice("a3", 900.0)
    run_id = h.start_run("double_post_then_fail", inp)
    h.wait_for_event(run_id, "step.completed", 90, lambda p: p.get("step") == "post_b")
    h.wait_for_event(run_id, "step.quarantined", 90, lambda p: p.get("step") == "boom")
    rows_before = h.ledger_rows(inp["invoice_id"])
    check(len(rows_before) == 2, f"expected two ledger rows before abandon, found {len(rows_before)}")

    r = h.api("POST", f"/v1/runs/{run_id}/abandon", {"reason": "acceptance A3", "actor": dict(TOM)}, expect=202)
    check(r.json().get("compensations_scheduled") == 2, f"compensations_scheduled = {r.json().get('compensations_scheduled')}, expected 2")
    h.wait_for_state(run_id, {"abandoned"}, 90)

    evs = h.events(run_id)
    scheduled = find_all(evs, "compensation.scheduled")
    check([p["payload"].get("for_step") for p in scheduled] == ["post_b", "post_a"],
          f"compensation.scheduled order is {[p['payload'].get('for_step') for p in scheduled]}, expected ['post_b', 'post_a']")
    completed = find_all(evs, "compensation.completed")
    check(len(completed) == 2, f"expected two compensation.completed, found {len(completed)}")
    check(not find_all(evs, "compensation.failed"), "a compensation failed")
    rows = h.ledger_rows(inp["invoice_id"])
    check(len(rows) == 2 and all(r.get("voided_at") for r in rows), f"ledger rows not both voided: {short(rows)}")


def _expect_refusal(h: Harness, run_id: str, tool: str, reason: str, step: str) -> None:
    refused = h.wait_for_event(run_id, "tool.refused", 90, lambda p: p.get("tool") == tool)
    p = refused["payload"]
    check(p.get("reason") == reason, f"tool.refused reason is {p.get('reason')!r}, expected {reason!r} (detail: {p.get('detail')})")
    check(str(p.get("remit_id", "")).startswith("rem_"), f"tool.refused carries no remit_id: {short(p)}")
    failed = h.wait_for_event(run_id, "step.failed", 30, lambda q: q.get("step") == step, after_seq=refused["seq"])
    check(failed["payload"].get("deterministic") is True, f"step.failed after a refusal is not deterministic: {short(failed['payload'])}")
    h.log(f"{tool} refused with {reason}; step {step} failed deterministically")


def a4(h: Harness) -> None:
    """A call outside the remit is refused at the gateway and logged (phase 2)."""
    inp = h.invoice("a4a", 1500.0)
    rem = h.issue_remit(tools=["ledger.lookup_vendor"])
    run_id = h.start_run("intake", inp, rem["remit_id"])
    _expect_refusal(h, run_id, "ledger.post_entry", "tool_not_in_remit", "post")
    check(h.ledger_rows(inp["invoice_id"]) == [], "a ledger row exists although the write was refused")

    inp2 = h.invoice("a4b", 1500.0)
    rem2 = h.issue_remit(scopes=["sql:table:vendors", "http:host:127.0.0.1", "test:*"])
    run2 = h.start_run("intake", inp2, rem2["remit_id"])
    _expect_refusal(h, run2, "ledger.post_entry", "scope_not_granted", "post")
    check(h.ledger_rows(inp2["invoice_id"]) == [], "a ledger row exists although the scope was not granted")

    inp3 = h.invoice("a4c", 1500.0)
    rem3 = h.issue_remit(autonomy="observe")
    run3 = h.start_run("double_post_then_fail", inp3, rem3["remit_id"])
    _expect_refusal(h, run3, "ledger.post_entry", "autonomy_too_low", "post_a")
    check(h.ledger_rows(inp3["invoice_id"]) == [], "a ledger row exists although autonomy was observe")


def a5(h: Harness) -> None:
    """Delegation narrows, never widens (phase 2)."""
    parent = h.issue_remit(tools=["ledger.post_entry", "http.get"], spend={"tokens": 200000, "usd": 2.0}, autonomy="supervised",
                           scopes=["sql:table:ledger_entries", "http:host:127.0.0.1"])
    pid = parent["remit_id"]

    def widen(body: dict, field_name: str) -> None:
        r = h.api("POST", f"/v1/remits/{pid}/derive", body)
        check(r.status_code == 422, f"derive {short(body)} returned {r.status_code}, expected 422: {r.text[:200]}")
        check(h.error_code(r) == "remit_widens", f"derive {short(body)} code is {h.error_code(r)!r}, expected remit_widens")
        got = h.error_details(r).get("field")
        check(got == field_name, f"derive {short(body)} details.field is {got!r}, expected {field_name!r}")
        h.log(f"derive {short(body)}: 422 remit_widens field {field_name}")

    widen({"tools": ["ledger.*"]}, "tools")
    widen({"spend": {"usd": 3.0}}, "spend.usd")
    widen({"autonomy": "autonomous"}, "autonomy")

    r = h.api("POST", f"/v1/remits/{pid}/derive",
              {"tools": ["ledger.post_entry", "http.get"], "spend": {"tokens": 200000, "usd": 1.0}, "autonomy": "propose"}, expect=201)
    child = r.json()
    check(child.get("parent_id") == pid, f"child parent_id {child.get('parent_id')} != {pid}")
    check(str(child.get("token", "")).startswith("krt1."), "child token is not a krt1 token")
    h.log(f"child remit {child['remit_id']} derived (propose, usd 1.0)")

    inp = h.invoice("a5", 800.0, probe_url=PROBE_URL)
    run_id = h.start_run("http_first", inp, child["remit_id"])
    fetched = h.wait_for_event(run_id, "tool.result", 90, lambda p: p.get("step") == "fetch")
    check(fetched["payload"].get("ok") is True, f"http.get under the child remit did not verify: {short(fetched['payload'])}")
    _expect_refusal(h, run_id, "ledger.post_entry", "autonomy_too_low", "post")
    check(h.ledger_rows(inp["invoice_id"]) == [], "a ledger row exists although the child remit is propose")


def a6(h: Harness) -> None:
    """A gated action parks, is approved by the right person, resumes, and is reconstructible (phase 5)."""
    inp = h.invoice("a6", 7250.0)
    run_id = h.start_run("intake", inp)
    requested = h.wait_for_event(run_id, "approval.requested", 90)
    evs = h.events(run_id)

    proposed = find(evs, "action.proposed")
    check(proposed is not None, "no action.proposed event")
    assert proposed is not None
    check(deep_get(proposed, "payload.action.amount") == 7250.0, f"proposed amount is {deep_get(proposed, 'payload.action.amount')}")
    decided = find(evs, "policy.decided", lambda p: p.get("action_id") == proposed["payload"].get("action_id"))
    check(decided is not None, "no policy.decided for the proposed action")
    assert decided is not None
    check(decided["payload"].get("decision") == "approval_required", f"policy decision {decided['payload'].get('decision')}")
    check(decided["payload"].get("rule") == "finance-default@1#0", f"rule is {decided['payload'].get('rule')!r}, expected 'finance-default@1#0'")
    check(approver_is(requested["payload"].get("approver"), "role", "finance_admin"),
          f"approver is {approver_text(requested['payload'].get('approver'))}, expected role:finance_admin")
    parked = h.wait_for_event(run_id, "run.parked", 15)
    check(parked["payload"].get("reason") == "approval", f"parked reason {parked['payload'].get('reason')}")

    st = h.run_state(run_id)
    check(st.get("state") == "parked", f"run state is {st.get('state')}, expected parked")
    step = next((s for s in st.get("steps", []) if s.get("id") == "propose_payment"), None)
    check(step is not None and not step.get("lease"), f"propose_payment still holds a lease: {short(step)}")
    check(all(e["seq"] < parked["seq"] for e in find_all(h.events(run_id), "step.leased")), "a lease was issued after the run parked")
    approval_id = requested["payload"]["approval_id"]
    pending = h.http.get("/v1/approvals", params={"state": "pending"})
    check(pending.status_code == 200, f"GET /v1/approvals returned {pending.status_code}")
    check(any(a.get("approval_id") == approval_id for a in pending.json()), f"approval {approval_id} is not listed as pending")

    wrong = h.api("POST", f"/v1/approvals/{approval_id}", {"decision": "approved", "actor": dict(ANA), "reason": "trying as a clerk"})
    check(wrong.status_code == 403 and h.error_code(wrong) == "not_the_approver",
          f"deciding as ap_clerk returned {wrong.status_code} {h.error_code(wrong)!r}, expected 403 not_the_approver")

    reason = "Verified against the delivery note and the purchase order"
    right = h.api("POST", f"/v1/approvals/{approval_id}", {"decision": "approved", "actor": dict(TOM), "reason": reason}, expect=200)
    check(right.json().get("run_id") == run_id, f"decide response names run {right.json().get('run_id')}")
    decided_ev = h.wait_for_event(run_id, "approval.decided", 15)
    resumed = h.wait_for_event(run_id, "run.resumed", 15)
    released = h.wait_for_event(run_id, "step.leased", 60, lambda p: p.get("step") == "propose_payment", after_seq=resumed["seq"])
    completed = h.wait_for_event(run_id, "step.completed", 60, lambda p: p.get("step") == "propose_payment", after_seq=released["seq"])
    out = completed["payload"].get("output") or {}
    check(out.get("decision") == "allow", f"re-run action decision is {out.get('decision')}, expected allow")
    check(out.get("rule") == f"approved:{approval_id}", f"re-run action rule is {out.get('rule')!r}, expected 'approved:{approval_id}'")
    h.wait_for_state(run_id, {"completed"}, 90)
    check(len(h.ledger_rows(inp["invoice_id"])) == 1, "approved run did not post exactly one ledger row")

    # Reconstruct from the log alone.
    evs = h.events(run_id)
    dec = find(evs, "approval.decided", lambda p: p.get("approval_id") == approval_id)
    assert dec is not None
    act = find(evs, "action.proposed", lambda p: p.get("action_id") == dec["payload"].get("action_id"))
    check(act is not None, "approval.decided does not link to an action.proposed")
    assert act is not None
    who = deep_get(dec, "payload.actor.id")
    when = dec.get("ts")
    why = deep_get(dec, "payload.reason")
    amount = deep_get(act, "payload.action.amount")
    check(who == "u-tom", f"reconstructed approver {who}")
    check(isinstance(when, str) and re.match(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?Z$", when), f"reconstructed timestamp {when!r} is not RFC 3339 UTC")
    check(why == reason, f"reconstructed reason {why!r}")
    check(amount == 7250.0, f"reconstructed amount {amount}")
    h.log(f"reconstructed: {who} approved {amount} at {when} because {why!r}")

    # Escalation past the 5 s SLA of finance-test.
    inp2 = h.invoice("a6esc", 7250.0)
    rem2 = h.issue_remit(policy_set=["finance-test", "finance-default"])
    run2 = h.start_run("intake", inp2, rem2["remit_id"])
    req2 = h.wait_for_event(run2, "approval.requested", 90)
    check(req2["payload"].get("sla_seconds") == 5, f"finance-test SLA is {req2['payload'].get('sla_seconds')}, expected 5")
    esc = h.wait_for_event(run2, "approval.escalated", 30)
    to = esc["payload"].get("to")
    if approver_is(to, "user", "u-cfo"):
        h.log("escalated to u-cfo (09 reading of reporting_line)")
    elif approver_is(to, "role", "admin"):
        h.note("escalated to role:admin: the kernel follows 04 (reporting_line of a role approver is role(\"admin\")), 09 expects u-cfo")
    else:
        raise Failure(f"approval.escalated to {approver_text(to)}, expected user u-cfo (09) or role admin (04)")
    frm = esc["payload"].get("from")
    check(frm is None or approver_is(frm, "role", "finance_admin"), f"approval.escalated from {approver_text(frm)}, expected role:finance_admin")

    # Rejection.
    inp3 = h.invoice("a6rej", 7250.0)
    run3 = h.start_run("intake", inp3)
    req3 = h.wait_for_event(run3, "approval.requested", 90)
    h.api("POST", f"/v1/approvals/{req3['payload']['approval_id']}", {"decision": "rejected", "actor": dict(TOM), "reason": "Duplicate of an invoice already paid"}, expect=200)
    st3 = h.wait_for_state(run3, {"failed"}, 60)
    evs3 = h.events(run3)
    codes = {deep_get(st3, "error.code"), deep_get(find(evs3, "run.failed") or {}, "payload.error.code")}
    codes |= {deep_get(e, "payload.error.code") for e in find_all(evs3, "step.failed")}
    check("action_rejected" in codes, f"rejected run does not carry action_rejected (codes seen: {sorted(str(c) for c in codes)})")
    check(h.ledger_rows(inp3["invoice_id"]) == [], "rejected run posted a ledger row")


def a7(h: Harness) -> None:
    """A three-step model workflow runs end to end with a stable cache prefix (phase 4)."""
    inp = h.invoice("a7", 2100.0)
    run_id = h.start_run("intake", inp)
    h.wait_for_state(run_id, {"completed"}, 90)
    evs = h.events(run_id)

    called = find_all(evs, "model.called")
    steps_called = {e["payload"].get("step") for e in called}
    check({"extract", "code"} <= steps_called, f"model.called steps {sorted(steps_called)}, expected extract and code")
    for e in called:
        check(re.fullmatch(r"[0-9a-f]{64}", str(e["payload"].get("prefix_hash", ""))), f"prefix_hash is not sha256 hex: {short(e['payload'])}")

    schemas = {s["id"]: s["output_schema"] for s in h.bundle["workflows"]["intake"]["steps"] if s["kind"] == "model"}
    for step_id, schema in schemas.items():
        done = find(evs, "step.completed", lambda p, s=step_id: p.get("step") == s)
        check(done is not None, f"no step.completed for {step_id}")
        assert done is not None
        problems = validate_schema(done["payload"].get("output"), schema)
        check(not problems, f"{step_id} output violates its schema: {problems}")
        responded = find(evs, "model.responded", lambda p, s=step_id: p.get("step") == s)
        check(responded is not None, f"no model.responded for {step_id}")
        assert responded is not None
        problems = validate_schema(responded["payload"].get("output"), schema)
        check(not problems, f"{step_id} model.responded output violates its schema: {problems}")

    responded_all = find_all(evs, "model.responded")
    usage_all = find_all(evs, "usage.recorded")
    check(usage_all, "no usage.recorded events")
    tokens_io = sum(int(deep_get(e, "payload.usage.input_tokens") or 0) + int(deep_get(e, "payload.usage.output_tokens") or 0) for e in responded_all)
    tokens_all = tokens_io + sum(int(deep_get(e, "payload.usage.cache_read_tokens") or 0) + int(deep_get(e, "payload.usage.cache_write_tokens") or 0) for e in responded_all)
    usd = sum(float(e["payload"].get("cost_usd") or 0.0) for e in responded_all)
    last = usage_all[-1]["payload"]
    check(last.get("cumulative_tokens") in (tokens_io, tokens_all),
          f"cumulative_tokens {last.get('cumulative_tokens')} != sum of model.responded ({tokens_io} or {tokens_all} with cache)")
    check(abs(float(last.get("cumulative_usd") or 0.0) - usd) < 1e-9, f"cumulative_usd {last.get('cumulative_usd')} != {usd}")
    h.log(f"usage: {last.get('cumulative_tokens')} tokens, {last.get('cumulative_usd')} usd across {len(responded_all)} model calls")

    # Every model.called for the same prompt across the suite carries the same prefix_hash.
    hashes: dict[str, set[str]] = {}
    for rid in h.all_runs:
        for e in find_all(h.events(rid), "model.called"):
            hashes.setdefault(str(e["payload"].get("step")), set()).add(str(e["payload"].get("prefix_hash")))
    for step_id, hs in hashes.items():
        check(len(hs) == 1, f"prefix_hash for step {step_id} varies across the suite: {sorted(hs)}")
    h.log(f"stable prefix hashes across {len(h.all_runs)} runs: " + ", ".join(f"{k}={next(iter(v))[:12]}" for k, v in sorted(hashes.items())))

    try:
        h.restart_workers({"KERNOS_MOCK_CONFIDENCE": "code=0.4"})
        inp2 = h.invoice("a7esc", 2200.0)
        run2 = h.start_run("intake", inp2)
        esc = h.wait_for_event(run2, "step.escalated", 90, lambda p: p.get("step") == "code")
        check(esc["payload"].get("from_tier") == "cheap" and esc["payload"].get("to_tier") == "standard",
              f"step.escalated tiers {esc['payload'].get('from_tier')} to {esc['payload'].get('to_tier')}, expected cheap to standard")
        h.wait_for_state(run2, {"completed"}, 90)

        h.restart_workers({"KERNOS_MOCK_REFUSE": "extract"})
        inp3 = h.invoice("a7ref", 2300.0)
        run3 = h.start_run("intake", inp3)
        parked = h.wait_for_event(run3, "run.parked", 90)
        check(parked["payload"].get("reason") == "refusal", f"parked reason {parked['payload'].get('reason')}, expected refusal")
    finally:
        h.restart_workers()


def a8(h: Harness) -> None:
    """A renamed upstream field is caught by a canary and the run parks (phases 3 and 7)."""
    h.upstream_mode("healthy")
    c0 = wait_until(lambda: (h.canary("http") or {}).get("status") == "healthy" and h.canary("http"), 15, what="http canary healthy")
    h.log(f"http canary: {c0.get('status')}")
    inp = h.invoice("a8a", 1300.0, probe_url=PROBE_URL)
    run_id = h.start_run("http_first", inp)
    h.wait_for_state(run_id, {"completed"}, 90)
    fetched = find(h.events(run_id), "tool.result", lambda p: p.get("step") == "fetch")
    check(fetched is not None and fetched["payload"].get("ok") is True, "first http.get did not succeed")

    h.upstream_mode("renamed")
    switched = now()
    c = wait_until(lambda: (h.canary("http") or {}).get("status") == "quarantined" and h.canary("http"), 6.5, 0.2, "http connector quarantined")
    h.log(f"http quarantined {now() - switched:.1f}s after the rename")
    missing = deep_get(c, "contract_diff.missing") or []
    check("json" in missing, f"contract_diff.missing is {missing}, expected to contain 'json'")
    repairs = sorted((h.gateway_data / "repairs").glob("*")) if (h.gateway_data / "repairs").exists() else []
    check(any(p.name.startswith("http") for p in repairs), f"no repair file for http under {h.gateway_data / 'repairs'} (found {[p.name for p in repairs]})")
    h.log(f"repair file: {[p.name for p in repairs if p.name.startswith('http')][0]}")

    inp2 = h.invoice("a8b", 1400.0, probe_url=PROBE_URL)
    run2 = h.start_run("http_first", inp2)
    failed = h.wait_for_event(run2, "step.failed", 60, lambda p: p.get("step") == "fetch")
    check(failed["payload"].get("deterministic") is False, f"quarantined call failed deterministically: {short(failed['payload'])}")
    result = find(h.events(run2), "tool.result", lambda p: p.get("step") == "fetch")
    check(result is not None and result["payload"].get("ok") is False, "no failed tool.result for the quarantined call")
    check("connector_quarantined" in json.dumps(result["payload"]) or "503" in json.dumps(result["payload"]),
          f"tool.result does not show the 503 connector_quarantined answer: {short(result['payload'])}")
    parked = h.wait_for_event(run2, "run.parked", 120)
    check(parked["payload"].get("reason") == "connector_quarantined", f"parked reason {parked['payload'].get('reason')}, expected connector_quarantined")
    attempts = len(find_all(h.events(run2), "step.failed", lambda p: p.get("step") == "fetch"))
    h.log(f"run parked for connector_quarantined after {attempts} failed attempt(s)")

    h.upstream_mode("healthy")
    r = h.gw.post("/v1/canaries/http/release")
    check(r.status_code in (200, 202, 204), f"release returned {r.status_code}: {r.text[:200]}")
    h.api("POST", f"/v1/runs/{run2}/resume", {"actor": dict(TOM)}, expect=(200, 202))
    h.wait_for_state(run2, {"completed"}, 120)
    check(len(h.ledger_rows(inp2["invoice_id"])) == 1, "resumed run did not post its ledger row")


def a9(h: Harness) -> None:
    """Poison input is quarantined, not retried forever (phase 7)."""
    inp = h.invoice("a9", 500.0)
    run_id = h.start_run("poison", inp)
    quarantined = h.wait_for_event(run_id, "step.quarantined", 90, lambda p: p.get("step") == "boom")
    check(quarantined["payload"].get("attempts") == 3, f"step.quarantined attempts = {quarantined['payload'].get('attempts')}, expected 3")
    evs = h.events(run_id)
    failed = find_all(evs, "step.failed", lambda p: p.get("step") == "boom")
    check(len(failed) == 3, f"expected three step.failed for boom, found {len(failed)}")
    check(all(e["payload"].get("deterministic") is True for e in failed), "a test.fail failure was recorded as non-deterministic")
    parked = h.wait_for_event(run_id, "run.parked", 15)
    check(parked["payload"].get("reason") == "quarantine", f"parked reason {parked['payload'].get('reason')}, expected quarantine")
    leases = len(find_all(evs, "step.leased", lambda p: p.get("step") == "boom"))
    check(leases == 3, f"expected three leases for boom, found {leases}")
    time.sleep(3.0)
    leases_after = len(find_all(h.events(run_id), "step.leased", lambda p: p.get("step") == "boom"))
    check(leases_after == 3, f"a fourth lease was issued for the quarantined step ({leases_after} total)")


def a10(h: Harness) -> None:
    """Budgets throttle first and park second (phase 7)."""
    h.stop_workers()
    try:
        inp = h.invoice("a10", 1000.0)
        rem = h.issue_remit(spend={"tokens": 200000, "usd": 0.0005})
        run_id = h.start_run("intake", inp, rem["remit_id"])
        lease_body = {"worker_id": "wrk-orchestrator", "kinds": ["model", "tool", "action", "compensation"], "ttl_seconds": 30}

        def lease() -> dict:
            r = h.api("POST", "/v1/leases", lease_body, expect=(200, 204))
            check(r.status_code == 200, "kernel answered 204 although a step of the budget run is runnable")
            lease_ = r.json()
            check(lease_.get("run_id") == run_id, f"leased a step of another run ({lease_.get('run_id')}); make sure no other run is runnable")
            return lease_

        first = lease()
        check(first.get("step") == "extract", f"first lease is for {first.get('step')}, expected extract")
        check(deep_get(first, "context.pacing") is False, f"first lease already carries pacing {deep_get(first, 'context.pacing')}")
        extract_out = {"vendor": "Northwind Dairy", "invoice_id": inp["invoice_id"], "total": inp["total"], "currency": "USD", "description": "Milk delivery"}
        h.api("POST", f"/v1/leases/{first['lease_id']}/complete", {"output": extract_out, "usage": {"tokens": 300, "usd": 0.00042}}, expect=200)
        soft = h.wait_for_event(run_id, "budget.soft_threshold", 15)
        check(float(soft["payload"].get("ratio") or 0) >= 0.8, f"soft threshold ratio {soft['payload'].get('ratio')}")
        h.log(f"budget.soft_threshold at {soft['payload'].get('cumulative_usd')} of {soft['payload'].get('ceiling_usd')} usd")

        second = wait_until(lambda: (lambda r: r.json() if r.status_code == 200 else None)(h.api("POST", "/v1/leases", lease_body, expect=(200, 204))), 15, 0.3, "the code step lease")
        check(second.get("run_id") == run_id and second.get("step") == "code", f"second lease is {second.get('run_id')}/{second.get('step')}, expected code")
        check(deep_get(second, "context.pacing") is True, f"lease after the soft threshold does not carry pacing: true (got {deep_get(second, 'context.pacing')})")
        h.api("POST", f"/v1/leases/{second['lease_id']}/complete", {"output": {"account": "5100", "confidence": 0.93}, "usage": {"tokens": 100, "usd": 0.0002}}, expect=200)
        exceeded = h.wait_for_event(run_id, "budget.exceeded", 15)
        h.log(f"budget.exceeded: {short(exceeded['payload'])}")
        parked = h.wait_for_event(run_id, "run.parked", 15)
        check(parked["payload"].get("reason") == "budget", f"parked reason {parked['payload'].get('reason')}, expected budget")

        for _ in range(4):
            r = h.api("POST", "/v1/leases", lease_body, expect=(200, 204))
            check(r.status_code == 204 or r.json().get("run_id") != run_id, "the kernel issued a lease for the budget-parked run")
            if r.status_code == 200:
                raise Failure(f"another run ({r.json().get('run_id')}) was runnable during A10; the leased step will expire in 30 s")
            time.sleep(0.4)
        check(h.run_state(run_id).get("state") == "parked", "run is no longer parked")
    finally:
        h.restart_workers()


def a11(h: Harness) -> None:
    """Bundles and policies are verified artefacts (phases 2 and 7)."""
    bundle = copy.deepcopy(h.bundle)
    good_sig = json.loads((h.work / "bundle.sig.json").read_text(encoding="utf-8"))

    h.cli("keys", "generate", "--out", str(h.work / "untrusted"), server=False)
    untrusted_key = h._key_file(h.work / "untrusted", ".key")
    h.cli("bundle", "sign", str(BUNDLE_DIR / "bundle.json"), "--key", str(untrusted_key), "--out", str(h.work / "untrusted.sig.json"), server=False)
    bad_sig = json.loads((h.work / "untrusted.sig.json").read_text(encoding="utf-8"))
    r = h.api("POST", "/v1/bundles", {"bundle": bundle, "signature": bad_sig})
    check(r.status_code == 422 and h.error_code(r) == "bundle_signature_invalid",
          f"untrusted key: {r.status_code} {h.error_code(r)!r}, expected 422 bundle_signature_invalid")
    h.log("untrusted key: 422 bundle_signature_invalid")

    tampered = copy.deepcopy(bundle)
    desc = tampered.get("description") or "Invoice intake."
    tampered["description"] = desc[:-1] + ("!" if desc[-1] != "!" else "?")
    r = h.api("POST", "/v1/bundles", {"bundle": tampered, "signature": good_sig})
    check(r.status_code == 422, f"tampered bundle: {r.status_code} {h.error_code(r)!r}, expected 422")
    h.log(f"tampered byte: 422 {h.error_code(r)}")

    r = h.api("POST", "/v1/bundles", {"bundle": bundle, "signature": good_sig})
    check(r.status_code == 200, f"re-applying the identical signed bundle returned {r.status_code}, expected 200")

    broken = 'policy "broken"\n\nrequire approval when\n  action.kind == "payment.issue" and action.amount >=\n  -> approver: role("finance_admin")\n'
    r = h.api("POST", "/v1/policies", {"name": "broken", "version": 1, "source": broken})
    check(r.status_code == 422 and h.error_code(r) == "policy_invalid", f"broken policy: {r.status_code} {h.error_code(r)!r}, expected 422 policy_invalid")
    details = h.error_details(r)
    check(isinstance(details.get("line"), int) and isinstance(details.get("column"), int), f"policy_invalid details lack line and column: {details}")
    h.log(f"broken policy: 422 policy_invalid at line {details.get('line')} column {details.get('column')}")

    corpus = [json.loads(line) for line in (BUNDLE_DIR / "corpus" / "actions.jsonl").read_text(encoding="utf-8").splitlines() if line.strip()]
    r = h.api("POST", "/v1/policies/test", {"policy_a": {"name": "finance-default", "version": 1}, "policy_b": {"name": "finance-default-10k", "version": 1}, "corpus": corpus}, expect=200)
    body = r.json()
    check(body.get("cases") == len(corpus), f"policies/test cases {body.get('cases')} != {len(corpus)}")
    expected = sorted(i for i, c in enumerate(corpus) if 5000 < float(c["action"]["amount"]) < 10000)
    got = sorted(int(f["index"]) for f in body.get("flips", []))
    check(got == expected, f"flips at {got}, expected exactly {expected}")
    for f in body.get("flips", []):
        check(f.get("a") == "approval_required" and f.get("b") == "allow", f"flip {f.get('index')} is {f.get('a')} to {f.get('b')}, expected approval_required to allow")
    h.log(f"policy flips: {got}")


def a12(h: Harness) -> None:
    """The evaluation gate promotes only on evidence (phase 6)."""
    golden = BUNDLE_DIR / "golden"
    base = h.work / "eval-base.json"
    cand = h.work / "eval-cand.json"

    def run_eval(out: Path, extra_env: dict[str, str]) -> dict:
        env = h.base_env()
        env.update(extra_env)
        argv = [*h.eval_cmd, "run", "--golden", str(golden), "--kernel", KERNEL_URL, "--gateway", GATEWAY_URL, "--provider", "mock", "--out", str(out)]
        res = subprocess.run(argv, env=env, capture_output=True, text=True, timeout=600)
        with open(h.logs / "eval.log", "a", encoding="utf-8") as fh:
            fh.write(f"$ {' '.join(shlex.quote(a) for a in argv)}\n[exit {res.returncode}]\n{res.stdout}{res.stderr}\n")
        check(res.returncode in (0, 1), f"kernos-eval run exited {res.returncode}: {res.stderr[-400:]}")
        check(out.exists(), f"kernos-eval run wrote no report at {out}")
        return json.loads(out.read_text(encoding="utf-8"))

    def gate(baseline: Path, candidate: Path) -> int:
        argv = [*h.eval_cmd, "gate", "--baseline", str(baseline), "--candidate", str(candidate), "--max-pass-drop", "0.0", "--max-cost-increase", "0.15", "--max-error-increase", "0.0"]
        res = subprocess.run(argv, env=h.base_env(), capture_output=True, text=True, timeout=120)
        with open(h.logs / "eval.log", "a", encoding="utf-8") as fh:
            fh.write(f"$ {' '.join(shlex.quote(a) for a in argv)}\n[exit {res.returncode}]\n{res.stdout}{res.stderr}\n")
        return res.returncode

    try:
        report = run_eval(base, {})
        check(report.get("cases") == 6, f"baseline report has {report.get('cases')} cases, expected 6")
        check(float(report.get("pass_rate", 0)) == 1.0, f"baseline pass_rate {report.get('pass_rate')}, expected 1.0 (failures: {short(report.get('failures'))})")
        h.log(f"baseline: {report.get('passed')}/{report.get('cases')} passed, cost {report.get('cost_usd')}")

        h.restart_workers({"KERNOS_MOCK_CONFIDENCE": "code=0.2"})
        report2 = run_eval(cand, {"KERNOS_MOCK_CONFIDENCE": "code=0.2"})
        check(float(report2.get("pass_rate", 1)) < 1.0, f"candidate pass_rate {report2.get('pass_rate')} did not drop under KERNOS_MOCK_CONFIDENCE=code=0.2")
        h.log(f"candidate: {report2.get('passed')}/{report2.get('cases')} passed")

        code = gate(base, cand)
        check(code == 1, f"kernos-eval gate exited {code} for a degraded candidate, expected 1")
        code = gate(base, base)
        check(code == 0, f"kernos-eval gate exited {code} for identical reports, expected 0")
    finally:
        h.restart_workers()


def a13(h: Harness) -> None:
    """The TypeScript client round-trips the same run (phase 5 surface)."""
    check(SDK_TS_DIST.exists(), f"{SDK_TS_DIST} missing; run `npm install && npm run build` in sdk/typescript")
    node = shutil.which("node")
    check(node is not None, "node is not on PATH")
    env = h.base_env()
    env["KERNOS_URL"] = KERNEL_URL
    env["KERNOS_BUNDLE_ID"] = str(h.bundle_id)
    res = subprocess.run([str(node), str(HERE / "ts_smoke.mjs")], env=env, capture_output=True, text=True, timeout=180)
    with open(h.logs / "ts_smoke.log", "a", encoding="utf-8") as fh:
        fh.write(f"[exit {res.returncode}]\n{res.stdout}{res.stderr}\n")
    for line in res.stdout.strip().splitlines()[-6:]:
        h.log(line)
    check(res.returncode == 0, f"ts_smoke.mjs exited {res.returncode}:\n{res.stdout[-800:]}{res.stderr[-800:]}")
    try:
        summary = json.loads(res.stdout.strip().splitlines()[-1])
        if summary.get("run_id"):
            h.scenario_runs.append(summary["run_id"])
            h.all_runs.append(summary["run_id"])
    except (ValueError, IndexError):
        pass


SCENARIOS: list[tuple[str, Callable[[Harness], None]]] = [
    ("A1", a1), ("A2", a2), ("A3", a3), ("A4", a4), ("A5", a5), ("A6", a6), ("A7", a7),
    ("A8", a8), ("A9", a9), ("A10", a10), ("A11", a11), ("A12", a12), ("A13", a13),
]


# ---------------------------------------------------------------------------
# Runner
# ---------------------------------------------------------------------------

def title_of(fn: Callable[..., Any]) -> str:
    doc = (fn.__doc__ or "").strip().splitlines()
    return doc[0] if doc else fn.__name__


def parse_args(argv: list[str]) -> argparse.Namespace:
    ap = argparse.ArgumentParser(description="Kernos acceptance suite")
    ap.add_argument("--only", help="comma-separated scenario ids to run, for example A6 or A1,A2")
    ap.add_argument("--keep", action="store_true", help="do not delete the temporary directories")
    ap.add_argument("--fail-fast", action="store_true", help="stop at the first failing scenario")
    ap.add_argument("--list", action="store_true", help="print the scenarios and exit")
    ap.add_argument("--verbose", action="store_true", help="more progress output")
    return ap.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    if args.list:
        for sid, fn in SCENARIOS:
            print(f"{sid:4} {title_of(fn)}")
        return 0
    selected = SCENARIOS
    if args.only:
        wanted = {s.strip().upper() for s in args.only.split(",") if s.strip()}
        unknown = wanted - {sid for sid, _ in SCENARIOS}
        if unknown:
            print(f"run.py: unknown scenario id(s): {', '.join(sorted(unknown))}", file=sys.stderr)
            return 2
        selected = [(sid, fn) for sid, fn in SCENARIOS if sid in wanted]

    print(f"kernos acceptance: {len(selected)} scenario(s); kernel {KERNEL_URL}, gateway {GATEWAY_URL}, upstream {UPSTREAM_URL}", flush=True)
    harness = Harness(keep=args.keep, verbose=args.verbose)
    print(f"work dir {harness.work}", flush=True)
    results: list[tuple[str, bool, float, str]] = []
    suite_start = now()
    try:
        try:
            harness.setup()
        except SetupError as exc:
            sys.stdout.flush()
            print(f"SETUP FAILED: {exc}", file=sys.stderr, flush=True)
            harness.dump_failure()
            return 2

        for sid, fn in selected:
            harness.scenario_runs = []
            print(f"{sid} {title_of(fn)}", flush=True)
            started = now()
            ok, detail = True, ""
            try:
                fn(harness)
            except Failure as exc:
                ok, detail = False, str(exc)
            except KeyboardInterrupt:
                raise
            except Exception as exc:  # noqa: BLE001
                ok, detail = False, f"{type(exc).__name__}: {exc}\n{traceback.format_exc()}"
            elapsed = now() - started
            results.append((sid, ok, elapsed, detail))
            print(f"{sid:4} {'PASS' if ok else 'FAIL'} {elapsed:7.1f}s  {title_of(fn)}", flush=True)
            if not ok:
                print(f"     {detail}")
                harness.dump_failure()
                if args.fail_fast:
                    break
            try:
                harness.ensure_default_workers()
            except SetupError as exc:
                print(f"     could not restore the default workers: {exc}")
                break
    except KeyboardInterrupt:
        print("interrupted")
    finally:
        harness.teardown()

    passed = sum(1 for _, ok, _, _ in results if ok)
    failed = [sid for sid, ok, _, _ in results if not ok]
    skipped = len(selected) - len(results)
    print()
    print(f"summary: {passed} passed, {len(failed)} failed{f', {skipped} not run' if skipped else ''}, {now() - suite_start:.1f}s total")
    if failed:
        print("failed: " + ", ".join(failed))
    for note in harness.notes:
        print(f"note: {note}")
    if not args.keep and failed:
        print("re-run with --keep to retain the work directory and logs")
    return 0 if not failed and not skipped else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
