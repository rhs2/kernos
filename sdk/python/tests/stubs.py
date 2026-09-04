"""In-process stubs of the kernel and the gateway for offline tests.

They speak the HTTP shapes of 02-KERNEL-API and 06-GATEWAY-API closely enough
for the client, the step executors, the worker loop and the evaluation harness
to run unchanged against them, and they record everything they are sent.
"""

from __future__ import annotations

import json
import re
import threading
import time
from collections import defaultdict
from collections.abc import Callable, Mapping
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any
from urllib.parse import parse_qs, urlparse

EXT_KINDS = {
    "step.escalated",
    "model.called",
    "model.responded",
    "tool.called",
    "tool.result",
    "tool.refused",
    "note",
}
ZERO_HASH = "0" * 64
Reply = tuple[int, Any]
Route = tuple[str, re.Pattern[str], Callable[..., Reply]]


def error(
    status: int, code: str, message: str = "", details: Mapping[str, Any] | None = None
) -> Reply:
    """An error reply in the shape of 00-OVERVIEW."""
    return status, {
        "error": {"code": code, "message": message or code, "details": dict(details or {})}
    }


class StubState:
    """Everything both stubs read and record; tests configure and inspect it."""

    def __init__(self) -> None:
        self.lock = threading.RLock()
        self.counter = 0
        self.timeline: list[tuple[float, str]] = []
        self.requests: list[dict[str, Any]] = []
        # kernel
        self.lease_queue: list[dict[str, Any]] = []
        self.lease_requests: list[dict[str, Any]] = []
        self.lease_status: int | None = None
        self.active_leases: dict[str, dict[str, Any]] = {}
        self.released: set[str] = set()
        self.expire_after_heartbeats: dict[str, int] = {}
        self.heartbeats: list[str] = []
        self.events: dict[str, list[dict[str, Any]]] = defaultdict(list)
        self.completes: list[dict[str, Any]] = []
        self.fails: list[dict[str, Any]] = []
        self.action_calls: list[dict[str, Any]] = []
        self.action_decisions: dict[str, Reply] = {}
        self.runs: dict[str, dict[str, Any]] = {}
        self.run_scripts: list[dict[str, Any]] = []
        self.run_starts: list[dict[str, Any]] = []
        self.bundles: list[dict[str, Any]] = []
        self.bundle_docs: dict[str, dict[str, Any]] = {}
        self.remits: list[dict[str, Any]] = []
        self.derives: list[dict[str, Any]] = []
        self.approvals: list[dict[str, Any]] = []
        self.decisions: list[dict[str, Any]] = []
        # gateway
        self.gateway_calls: list[dict[str, Any]] = []
        self.gateway_responses: dict[str, list[Reply]] = defaultdict(list)
        self.gateway_default: Reply = (
            200,
            {
                "ok": True,
                "result": {"entry_id": 1, "posted_at": "2026-09-04T12:00:00.000Z"},
                "scope": "sql:table:ledger_entries",
                "replayed": False,
                "latency_ms": 3,
            },
        )
        self.gateway_delay_s = 0.0
        self.gateway_tools: list[dict[str, Any]] = [
            {"id": "ledger.post_entry", "connector": "ledger", "writes": True},
            {"id": "ledger.lookup_vendor", "connector": "ledger", "writes": False},
        ]

    def next_id(self, prefix: str) -> str:
        with self.lock:
            self.counter += 1
            return f"{prefix}_{self.counter:026d}"

    def mark(self, label: str) -> None:
        with self.lock:
            self.timeline.append((time.monotonic(), label))

    def order(self, *prefixes: str) -> list[str]:
        """Timeline labels that start with one of ``prefixes``, in time order."""
        with self.lock:
            return [
                label
                for _, label in sorted(self.timeline)
                if any(label.startswith(prefix) for prefix in prefixes)
            ]

    def add_lease(self, lease: dict[str, Any]) -> dict[str, Any]:
        with self.lock:
            self.active_leases[lease["lease_id"]] = lease
            self.lease_queue.append(lease)
        return lease

    def release(self, lease_id: str) -> None:
        with self.lock:
            self.released.add(lease_id)

    def events_of(self, run_id: str, kind: str | None = None) -> list[dict[str, Any]]:
        with self.lock:
            return [e for e in self.events[run_id] if kind is None or e["kind"] == kind]

    def script_run(self, events: list[dict[str, Any]], state: dict[str, Any]) -> None:
        with self.lock:
            self.run_scripts.append({"events": events, "state": state})

    def gateway_reply(self, tool: str, status: int, body: dict[str, Any]) -> None:
        with self.lock:
            self.gateway_responses[tool].append((status, body))


class KernelApi:
    """The kernel routes over a :class:`StubState`."""

    def __init__(self, state: StubState) -> None:
        self.state = state
        self.routes: list[Route] = [
            ("GET", re.compile(r"/v1/health"), self.health),
            ("GET", re.compile(r"/v1/bundles"), self.bundles_list),
            ("GET", re.compile(r"/v1/bundles/([^/]+)"), self.bundle_get),
            ("POST", re.compile(r"/v1/remits"), self.remit_issue),
            ("POST", re.compile(r"/v1/remits/([^/]+)/derive"), self.remit_derive),
            ("POST", re.compile(r"/v1/runs"), self.run_start),
            ("GET", re.compile(r"/v1/runs/([^/]+)"), self.run_get),
            ("GET", re.compile(r"/v1/runs/([^/]+)/events"), self.run_events),
            ("POST", re.compile(r"/v1/runs/([^/]+)/events"), self.event_post),
            ("POST", re.compile(r"/v1/runs/([^/]+)/replay"), self.run_replay),
            ("POST", re.compile(r"/v1/runs/([^/]+)/abandon"), self.run_abandon),
            ("POST", re.compile(r"/v1/runs/([^/]+)/resume"), self.run_resume),
            ("POST", re.compile(r"/v1/leases"), self.lease),
            ("POST", re.compile(r"/v1/leases/([^/]+)/heartbeat"), self.heartbeat),
            ("POST", re.compile(r"/v1/leases/([^/]+)/complete"), self.complete),
            ("POST", re.compile(r"/v1/leases/([^/]+)/fail"), self.fail),
            ("POST", re.compile(r"/v1/leases/([^/]+)/actions"), self.actions),
            ("GET", re.compile(r"/v1/approvals"), self.approvals_list),
            ("POST", re.compile(r"/v1/approvals/([^/]+)"), self.approval_decide),
        ]

    def health(
        self, match: re.Match[str], query: dict[str, str], headers: Mapping[str, str], body: Any
    ) -> Reply:
        return 200, {
            "ok": True,
            "version": "0.1.0",
            "uptime_s": 1,
            "runs": {"running": 0, "parked": 0},
        }

    def bundles_list(
        self, match: re.Match[str], query: dict[str, str], headers: Mapping[str, str], body: Any
    ) -> Reply:
        return 200, list(self.state.bundles)

    def bundle_get(
        self, match: re.Match[str], query: dict[str, str], headers: Mapping[str, str], body: Any
    ) -> Reply:
        document = self.state.bundle_docs.get(match.group(1))
        return (200, document) if document else error(404, "not_found", "no such bundle")

    def _remit_reply(self, remit_id: str, parent: str | None = None) -> Reply:
        reply = {
            "remit_id": remit_id,
            "token": f"krt1.{remit_id}.sig.key_1",
            "expires_at": "2026-09-05T00:00:00.000Z",
        }
        if parent:
            reply["parent_id"] = parent
        return 201, reply

    def remit_issue(
        self, match: re.Match[str], query: dict[str, str], headers: Mapping[str, str], body: Any
    ) -> Reply:
        remit_id = self.state.next_id("rem")
        self.state.remits.append({"remit_id": remit_id, **(body or {})})
        return self._remit_reply(remit_id)

    def remit_derive(
        self, match: re.Match[str], query: dict[str, str], headers: Mapping[str, str], body: Any
    ) -> Reply:
        remit_id = self.state.next_id("rem")
        self.state.derives.append({"parent_id": match.group(1), "remit_id": remit_id, "body": body})
        return self._remit_reply(remit_id, match.group(1))

    def run_start(
        self, match: re.Match[str], query: dict[str, str], headers: Mapping[str, str], body: Any
    ) -> Reply:
        state = self.state
        with state.lock:
            script = (
                state.run_scripts.pop(0)
                if state.run_scripts
                else {
                    "events": [
                        {"kind": "run.created"},
                        {"kind": "run.completed", "payload": {"output": {}}},
                    ],
                    "state": {"state": "completed", "steps": []},
                }
            )
            run_id = state.next_id("run")
            state.events[run_id] = [
                self._record(run_id, index + 1, item) for index, item in enumerate(script["events"])
            ]
            state.runs[run_id] = {"run_id": run_id, **script["state"]}
            state.run_starts.append(dict(body or {}))
        return 201, {"run_id": run_id, "state": "running"}

    @staticmethod
    def _record(run_id: str, seq: int, item: Mapping[str, Any]) -> dict[str, Any]:
        return {
            "schema": "kernos.events/1",
            "run_id": run_id,
            "seq": seq,
            "ts": "2026-09-04T12:00:00.000Z",
            "kind": item["kind"],
            "actor": item.get("actor", {"type": "kernel", "id": "kernel"}),
            "payload": item.get("payload", {}),
            "prev_hash": ZERO_HASH,
            "hash": f"{seq:064x}",
        }

    def run_get(
        self, match: re.Match[str], query: dict[str, str], headers: Mapping[str, str], body: Any
    ) -> Reply:
        run = self.state.runs.get(match.group(1))
        return (200, run) if run else error(404, "not_found", "no such run")

    def run_events(
        self, match: re.Match[str], query: dict[str, str], headers: Mapping[str, str], body: Any
    ) -> Reply:
        from_seq = int(query.get("from_seq", 1))
        limit = int(query.get("limit", 500))
        with self.state.lock:
            events = [e for e in self.state.events[match.group(1)] if e["seq"] >= from_seq][:limit]
        return 200, {"events": events, "next_seq": None}

    def event_post(
        self, match: re.Match[str], query: dict[str, str], headers: Mapping[str, str], body: Any
    ) -> Reply:
        state = self.state
        lease_id = headers.get("x-kernos-lease")
        remit = headers.get("x-kernos-remit")
        with state.lock:
            lease_ok = lease_id in state.active_leases and lease_id not in state.released
            if not (lease_ok or remit):
                return error(403, "event_not_permitted", "no valid lease or remit")
            kind = body["kind"]
            if kind not in EXT_KINDS:
                return error(403, "event_not_permitted", f"{kind} is not an external kind")
            run_id = match.group(1)
            seq = len(state.events[run_id]) + 1
            record = self._record(
                run_id, seq, {"kind": kind, "actor": body["actor"], "payload": body["payload"]}
            )
            state.events[run_id].append(record)
            state.mark(f"event:{kind}")
        return 201, {"seq": seq, "hash": record["hash"]}

    def run_replay(
        self, match: re.Match[str], query: dict[str, str], headers: Mapping[str, str], body: Any
    ) -> Reply:
        run_id = match.group(1)
        if run_id not in self.state.runs:
            return error(404, "not_found", "no such run")
        return 200, {
            "chain_valid": True,
            "events": len(self.state.events[run_id]),
            "state_matches": True,
            "decisions": 0,
            "decision_mismatches": [],
            "chain_errors": [],
            "state": self.state.runs[run_id],
        }

    def run_abandon(
        self, match: re.Match[str], query: dict[str, str], headers: Mapping[str, str], body: Any
    ) -> Reply:
        return 202, {"compensations_scheduled": 0}

    def run_resume(
        self, match: re.Match[str], query: dict[str, str], headers: Mapping[str, str], body: Any
    ) -> Reply:
        return 200, {"run_id": match.group(1), "state": "running"}

    def lease(
        self, match: re.Match[str], query: dict[str, str], headers: Mapping[str, str], body: Any
    ) -> Reply:
        state = self.state
        with state.lock:
            state.lease_requests.append(dict(body or {}))
            if state.lease_status:
                return error(state.lease_status, "kernel_error", "forced by the test")
            if state.lease_queue:
                lease = state.lease_queue.pop(0)
                state.mark(f"lease:{lease['lease_id']}")
                return 200, lease
        return 204, None

    def _lease_guard(self, lease_id: str) -> Reply | None:
        if lease_id not in self.state.active_leases:
            return error(404, "lease_not_found", "no such lease")
        if lease_id in self.state.released:
            return error(410, "lease_expired", "the lease is gone")
        return None

    def heartbeat(
        self, match: re.Match[str], query: dict[str, str], headers: Mapping[str, str], body: Any
    ) -> Reply:
        lease_id = match.group(1)
        with self.state.lock:
            guard = self._lease_guard(lease_id)
            if guard:
                return guard
            self.state.heartbeats.append(lease_id)
            limit = self.state.expire_after_heartbeats.get(lease_id)
            if limit is not None and self.state.heartbeats.count(lease_id) >= limit:
                self.state.released.add(lease_id)
        return 200, {"expires_at": "2026-09-04T12:00:30.000Z"}

    def complete(
        self, match: re.Match[str], query: dict[str, str], headers: Mapping[str, str], body: Any
    ) -> Reply:
        lease_id = match.group(1)
        with self.state.lock:
            guard = self._lease_guard(lease_id)
            if guard:
                return guard
            self.state.completes.append({"lease_id": lease_id, **(body or {})})
            self.state.mark("complete")
        return 200, {"run_state": "running", "next_step": None}

    def fail(
        self, match: re.Match[str], query: dict[str, str], headers: Mapping[str, str], body: Any
    ) -> Reply:
        lease_id = match.group(1)
        with self.state.lock:
            guard = self._lease_guard(lease_id)
            if guard:
                return guard
            self.state.fails.append({"lease_id": lease_id, **(body or {})})
            self.state.mark("fail")
        outcome = "quarantined" if body.get("deterministic") else "retry_scheduled"
        return 200, {"outcome": outcome, "delay_ms": 500}

    def actions(
        self, match: re.Match[str], query: dict[str, str], headers: Mapping[str, str], body: Any
    ) -> Reply:
        lease_id = match.group(1)
        with self.state.lock:
            guard = self._lease_guard(lease_id)
            if guard:
                return guard
            self.state.action_calls.append({"lease_id": lease_id, **(body or {})})
            kind = str((body or {}).get("action", {}).get("kind", ""))
            status, reply = self.state.action_decisions.get(
                kind, (200, {"decision": "allow", "rule": "default"})
            )
            reply = dict(reply)
            if status == 200:
                reply.setdefault("action_id", self.state.next_id("act"))
                if reply.get("decision") == "approval_required":
                    reply.setdefault("approval_id", self.state.next_id("apr"))
                    self.state.released.add(lease_id)
            self.state.mark("action")
        return status, reply

    def approvals_list(
        self, match: re.Match[str], query: dict[str, str], headers: Mapping[str, str], body: Any
    ) -> Reply:
        return 200, list(self.state.approvals)

    def approval_decide(
        self, match: re.Match[str], query: dict[str, str], headers: Mapping[str, str], body: Any
    ) -> Reply:
        self.state.decisions.append({"approval_id": match.group(1), **(body or {})})
        return 200, {"run_id": "run_1", "run_state": "running"}


class GatewayApi:
    """The gateway routes over a :class:`StubState`."""

    def __init__(self, state: StubState) -> None:
        self.state = state
        self.routes: list[Route] = [
            ("GET", re.compile(r"/v1/health"), self.health),
            ("GET", re.compile(r"/v1/tools"), self.tools),
            ("GET", re.compile(r"/v1/canaries"), self.canaries),
            ("POST", re.compile(r"/v1/canaries/([^/]+)/(probe|release)"), self.canary_action),
            ("POST", re.compile(r"/v1/tools/call"), self.call),
        ]

    def health(
        self, match: re.Match[str], query: dict[str, str], headers: Mapping[str, str], body: Any
    ) -> Reply:
        return 200, {"ok": True, "version": "0.1.0", "connectors": {"ledger": "healthy"}}

    def tools(
        self, match: re.Match[str], query: dict[str, str], headers: Mapping[str, str], body: Any
    ) -> Reply:
        return 200, list(self.state.gateway_tools)

    def canaries(
        self, match: re.Match[str], query: dict[str, str], headers: Mapping[str, str], body: Any
    ) -> Reply:
        return 200, [{"connector": "ledger", "status": "healthy"}]

    def canary_action(
        self, match: re.Match[str], query: dict[str, str], headers: Mapping[str, str], body: Any
    ) -> Reply:
        return 200, {"connector": match.group(1), "status": "healthy"}

    def call(
        self, match: re.Match[str], query: dict[str, str], headers: Mapping[str, str], body: Any
    ) -> Reply:
        state = self.state
        tool = str((body or {}).get("tool", ""))
        state.mark(f"gateway:{tool}")
        with state.lock:
            state.gateway_calls.append(
                {**(body or {}), "authorization": headers.get("authorization")}
            )
            queue = state.gateway_responses.get(tool)
            status, reply = queue.pop(0) if queue else state.gateway_default
        if state.gateway_delay_s:
            time.sleep(state.gateway_delay_s)
        return status, reply


def start_server(api: KernelApi | GatewayApi) -> tuple[ThreadingHTTPServer, str]:
    """Serve ``api`` on an ephemeral loopback port in a daemon thread."""

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, format: str, *args: Any) -> None:
            return

        def _dispatch(self, method: str) -> None:
            parsed = urlparse(self.path)
            query = {key: values[-1] for key, values in parse_qs(parsed.query).items()}
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length) if length else b""
            body = json.loads(raw) if raw else None
            headers = {key.lower(): value for key, value in self.headers.items()}
            api.state.requests.append(
                {"method": method, "path": parsed.path, "headers": headers, "body": body}
            )
            for route_method, pattern, function in api.routes:
                match = pattern.fullmatch(parsed.path)
                if route_method == method and match:
                    status, reply = function(match, query, headers, body)
                    break
            else:
                status, reply = error(404, "not_found", f"no route for {method} {parsed.path}")
            payload = b"" if reply is None else json.dumps(reply).encode("utf-8")
            self.send_response(status)
            if payload:
                self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            if payload:
                self.wfile.write(payload)

        def do_GET(self) -> None:
            self._dispatch("GET")

        def do_POST(self) -> None:
            self._dispatch("POST")

    class QuietServer(ThreadingHTTPServer):
        daemon_threads = True

        def handle_error(self, request: Any, client_address: Any) -> None:
            # A client that gave up (the timeout tests) closes the socket
            # before the reply; that is expected and not worth a traceback.
            return

    server = QuietServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, name="stub-server", daemon=True).start()
    return server, f"http://127.0.0.1:{server.server_port}"
