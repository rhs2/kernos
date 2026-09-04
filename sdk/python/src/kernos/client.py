"""HTTP clients for the kernel (02-KERNEL-API) and the gateway (06-GATEWAY-API).

Every non-2xx response raises :class:`KernosError` carrying the HTTP status and
the stable error code; transport failures and timeouts raise
:class:`KernosNetworkError`. Every request has a timeout. A bearer token, when
given, is sent on every request.
"""

from __future__ import annotations

import time
from collections.abc import Iterator, Mapping
from typing import Any

import httpx

__all__ = [
    "TERMINAL_EVENTS",
    "GatewayClient",
    "KernelClient",
    "KernosError",
    "KernosNetworkError",
]

TERMINAL_EVENTS: frozenset[str] = frozenset({"run.completed", "run.failed", "run.abandoned"})
"""Event kinds that end a run; :meth:`KernelClient.follow` stops on them."""

JsonDict = dict[str, Any]


class KernosError(Exception):
    """A non-2xx answer from the kernel or the gateway.

    ``status`` is the HTTP status, ``code`` the stable snake_case error code
    (for a gateway refusal it is the ``refusal.reason``), ``details`` whatever
    the server attached.
    """

    def __init__(
        self,
        status: int,
        code: str,
        message: str,
        details: Mapping[str, Any] | None = None,
    ) -> None:
        self.status = status
        self.code = code
        self.message = message
        self.details: dict[str, Any] = dict(details or {})
        super().__init__(f"{status} {code}: {message}")

    @classmethod
    def from_response(cls, response: httpx.Response) -> KernosError:
        """Map an error response of either server onto an exception."""
        status = response.status_code
        try:
            body = response.json()
        except ValueError:
            body = None
        if isinstance(body, dict):
            error = body.get("error")
            refusal = body.get("refusal")
            if isinstance(refusal, dict):
                return cls(
                    status,
                    str(refusal.get("reason", "refused")),
                    str(refusal.get("detail", "refused by the gateway")),
                    refusal,
                )
            if isinstance(error, dict):
                return cls(
                    status,
                    str(error.get("code", f"http_{status}")),
                    str(error.get("message", error.get("code", "request failed"))),
                    error.get("details") if isinstance(error.get("details"), dict) else error,
                )
        text = response.text[:200] if response.text else ""
        return cls(status, f"http_{status}", text or f"request failed with status {status}")


class KernosNetworkError(KernosError):
    """The request never produced a response: connection failure or timeout.

    ``status`` is 0 and ``code`` is ``timeout`` or ``network_error``.
    """

    def __init__(self, message: str, code: str = "network_error") -> None:
        super().__init__(0, code, message)


class _HttpClient:
    """Shared request plumbing with error mapping and timeouts."""

    def __init__(
        self,
        base_url: str,
        token: str | None = None,
        timeout: float = 30.0,
        transport: httpx.BaseTransport | None = None,
    ) -> None:
        if not base_url.startswith(("http://", "https://")):
            raise ValueError(f"base url must start with http:// or https://: {base_url!r}")
        headers = {"Accept": "application/json"}
        if token:
            headers["Authorization"] = f"Bearer {token}"
        self.base_url = base_url.rstrip("/")
        self.timeout = timeout
        self._client = httpx.Client(
            base_url=self.base_url,
            headers=headers,
            timeout=httpx.Timeout(timeout),
            transport=transport,
        )

    def close(self) -> None:
        """Close the underlying connection pool."""
        self._client.close()

    def __enter__(self) -> Any:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def _request(
        self,
        method: str,
        path: str,
        *,
        json_body: Any | None = None,
        params: Mapping[str, Any] | None = None,
        headers: Mapping[str, str] | None = None,
        timeout: float | None = None,
    ) -> httpx.Response:
        request_timeout: Any = httpx.USE_CLIENT_DEFAULT if timeout is None else timeout
        try:
            response = self._client.request(
                method,
                path,
                json=json_body,
                params={k: v for k, v in (params or {}).items() if v is not None} or None,
                headers=dict(headers or {}),
                timeout=request_timeout,
            )
        except httpx.TimeoutException as exc:
            raise KernosNetworkError(f"timeout on {method} {path}: {exc}", "timeout") from exc
        except httpx.HTTPError as exc:
            raise KernosNetworkError(f"network error on {method} {path}: {exc}") from exc
        if response.status_code >= 400:
            raise KernosError.from_response(response)
        return response

    def _json(self, method: str, path: str, **kwargs: Any) -> Any:
        response = self._request(method, path, **kwargs)
        if response.status_code == 204 or not response.content:
            return None
        try:
            return response.json()
        except ValueError as exc:
            raise KernosError(
                response.status_code, "invalid_json", f"non-JSON body from {method} {path}"
            ) from exc


class KernelClient(_HttpClient):
    """Client for the kernel and control-plane API at ``base_url`` (default port 7401)."""

    # Health and keys

    def health(self) -> JsonDict:
        """``GET /v1/health``."""
        return dict(self._json("GET", "/v1/health"))

    def keys(self) -> JsonDict:
        """``GET /v1/keys``: the control-plane public key."""
        return dict(self._json("GET", "/v1/keys"))

    # Bundles

    def apply_bundle(self, bundle: Mapping[str, Any], signature: Mapping[str, Any]) -> JsonDict:
        """``POST /v1/bundles`` with a signed bundle."""
        body = {"bundle": dict(bundle), "signature": dict(signature)}
        return dict(self._json("POST", "/v1/bundles", json_body=body))

    def list_bundles(self) -> list[JsonDict]:
        """``GET /v1/bundles``."""
        return list(self._json("GET", "/v1/bundles") or [])

    def get_bundle(self, bundle_id: str) -> JsonDict:
        """``GET /v1/bundles/{id}``."""
        return dict(self._json("GET", f"/v1/bundles/{bundle_id}"))

    # Policies

    def apply_policy(self, name: str, version: int, source: str) -> JsonDict:
        """``POST /v1/policies``."""
        body = {"name": name, "version": version, "source": source}
        return dict(self._json("POST", "/v1/policies", json_body=body))

    def list_policies(self) -> list[JsonDict]:
        """``GET /v1/policies``."""
        return list(self._json("GET", "/v1/policies") or [])

    def get_policy(self, name: str, version: int | None = None) -> Any:
        """``GET /v1/policies/{name}`` (all versions) or ``/{name}/{version}`` (source)."""
        path = f"/v1/policies/{name}" if version is None else f"/v1/policies/{name}/{version}"
        return self._json("GET", path)

    def test_policies(
        self,
        policy_a: Mapping[str, Any],
        policy_b: Mapping[str, Any],
        corpus: list[Mapping[str, Any]],
    ) -> JsonDict:
        """``POST /v1/policies/test``: decisions that flip between two policies."""
        body = {"policy_a": dict(policy_a), "policy_b": dict(policy_b), "corpus": list(corpus)}
        return dict(self._json("POST", "/v1/policies/test", json_body=body))

    # Remits

    def issue_remit(self, remit: Mapping[str, Any]) -> JsonDict:
        """``POST /v1/remits``: issue a remit; returns ``remit_id``, ``token``, ``expires_at``."""
        return dict(self._json("POST", "/v1/remits", json_body=dict(remit)))

    def derive_remit(self, remit_id: str, narrowing: Mapping[str, Any]) -> JsonDict:
        """``POST /v1/remits/{id}/derive``: a child remit that only narrows."""
        return dict(self._json("POST", f"/v1/remits/{remit_id}/derive", json_body=dict(narrowing)))

    def get_remit(self, remit_id: str) -> JsonDict:
        """``GET /v1/remits/{id}``."""
        return dict(self._json("GET", f"/v1/remits/{remit_id}"))

    # Runs

    def start_run(
        self,
        bundle_id: str,
        workflow: str,
        input_data: Mapping[str, Any],
        remit_id: str,
        requested_by: Mapping[str, Any],
    ) -> JsonDict:
        """``POST /v1/runs``: start a run; returns ``run_id`` and ``state``."""
        body = {
            "bundle_id": bundle_id,
            "workflow": workflow,
            "input": dict(input_data),
            "remit_id": remit_id,
            "requested_by": dict(requested_by),
        }
        return dict(self._json("POST", "/v1/runs", json_body=body))

    def get_run(self, run_id: str) -> JsonDict:
        """``GET /v1/runs/{id}``: the folded ``RunState``."""
        return dict(self._json("GET", f"/v1/runs/{run_id}"))

    def list_runs(
        self,
        state: str | None = None,
        department: str | None = None,
        limit: int | None = None,
        after: str | None = None,
    ) -> JsonDict:
        """``GET /v1/runs`` with optional filters; returns ``{"runs": [...], "next": ...}``."""
        params = {"state": state, "department": department, "limit": limit, "after": after}
        return dict(self._json("GET", "/v1/runs", params=params))

    def get_events(self, run_id: str, from_seq: int = 1, limit: int = 500) -> JsonDict:
        """``GET /v1/runs/{id}/events``; returns ``{"events": [...], "next_seq": n | null}``."""
        params = {"from_seq": from_seq, "limit": limit}
        return dict(self._json("GET", f"/v1/runs/{run_id}/events", params=params))

    def post_event(
        self,
        run_id: str,
        kind: str,
        payload: Mapping[str, Any],
        actor: Mapping[str, Any],
        *,
        lease_id: str | None = None,
        remit_token: str | None = None,
        timeout: float | None = None,
    ) -> JsonDict:
        """``POST /v1/runs/{id}/events``: append an external event.

        A worker passes ``lease_id`` (header ``X-Kernos-Lease``); the gateway
        passes ``remit_token`` (header ``X-Kernos-Remit``).
        """
        headers: dict[str, str] = {}
        if lease_id:
            headers["X-Kernos-Lease"] = lease_id
        if remit_token:
            headers["X-Kernos-Remit"] = remit_token
        body = {"kind": kind, "payload": dict(payload), "actor": dict(actor)}
        return dict(
            self._json(
                "POST",
                f"/v1/runs/{run_id}/events",
                json_body=body,
                headers=headers,
                timeout=timeout,
            )
        )

    def replay(self, run_id: str) -> JsonDict:
        """``POST /v1/runs/{id}/replay``: verify chain, state and decisions."""
        return dict(self._json("POST", f"/v1/runs/{run_id}/replay"))

    def abandon_run(self, run_id: str, reason: str, actor: Mapping[str, Any]) -> JsonDict:
        """``POST /v1/runs/{id}/abandon``: schedule compensation."""
        body = {"reason": reason, "actor": dict(actor)}
        return dict(self._json("POST", f"/v1/runs/{run_id}/abandon", json_body=body))

    def resume_run(self, run_id: str, actor: Mapping[str, Any]) -> JsonDict:
        """``POST /v1/runs/{id}/resume``."""
        return dict(
            self._json("POST", f"/v1/runs/{run_id}/resume", json_body={"actor": dict(actor)})
        )

    def follow(
        self,
        run_id: str,
        *,
        from_seq: int = 1,
        poll_s: float = 0.5,
        timeout_s: float | None = None,
    ) -> Iterator[JsonDict]:
        """Yield the run's events in order until a terminal event arrives.

        Polls ``get_events`` every ``poll_s`` seconds. Raises ``TimeoutError``
        when ``timeout_s`` elapses without the run ending.
        """
        deadline = None if timeout_s is None else time.monotonic() + timeout_s
        next_seq = from_seq
        while True:
            page = self.get_events(run_id, from_seq=next_seq)
            events = list(page.get("events") or [])
            for event in events:
                yield event
                if event.get("kind") in TERMINAL_EVENTS:
                    return
            if events:
                next_seq = int(events[-1].get("seq", next_seq)) + 1
            if page.get("next_seq"):
                next_seq = max(next_seq, int(page["next_seq"]))
            if deadline is not None and time.monotonic() >= deadline:
                raise TimeoutError(f"run {run_id} did not end within {timeout_s} s")
            time.sleep(poll_s)

    # Leases

    def lease(self, worker_id: str, kinds: list[str], ttl_seconds: int) -> JsonDict | None:
        """``POST /v1/leases``: the next runnable step, or ``None`` on 204."""
        body = {"worker_id": worker_id, "kinds": list(kinds), "ttl_seconds": ttl_seconds}
        result = self._json("POST", "/v1/leases", json_body=body)
        return dict(result) if result else None

    def heartbeat(self, lease_id: str, timeout: float | None = None) -> JsonDict:
        """``POST /v1/leases/{id}/heartbeat``; a 410 raises ``KernosError(lease_expired)``."""
        return dict(self._json("POST", f"/v1/leases/{lease_id}/heartbeat", timeout=timeout))

    def complete(
        self,
        lease_id: str,
        output: Any,
        usage: Mapping[str, Any] | None = None,
        timeout: float | None = None,
    ) -> JsonDict:
        """``POST /v1/leases/{id}/complete`` with the step output and optional usage."""
        body: JsonDict = {"output": output}
        if usage is not None:
            body["usage"] = dict(usage)
        return dict(
            self._json("POST", f"/v1/leases/{lease_id}/complete", json_body=body, timeout=timeout)
        )

    def fail(
        self,
        lease_id: str,
        code: str,
        message: str,
        deterministic: bool,
        timeout: float | None = None,
    ) -> JsonDict:
        """``POST /v1/leases/{id}/fail`` with an honest ``deterministic`` flag."""
        body = {"error": {"code": code, "message": message}, "deterministic": deterministic}
        return dict(
            self._json("POST", f"/v1/leases/{lease_id}/fail", json_body=body, timeout=timeout)
        )

    def propose_action(
        self, lease_id: str, action: Mapping[str, Any], timeout: float | None = None
    ) -> JsonDict:
        """``POST /v1/leases/{id}/actions``: policy decision for a proposed action.

        A denied action may come back as HTTP 403 ``action_denied``; the caller
        handles both that and a 200 with ``decision: deny``.
        """
        return dict(
            self._json(
                "POST",
                f"/v1/leases/{lease_id}/actions",
                json_body={"action": dict(action)},
                timeout=timeout,
            )
        )

    # Approvals

    def list_approvals(
        self, state: str | None = None, approver: str | None = None
    ) -> list[JsonDict]:
        """``GET /v1/approvals`` with optional ``state`` and ``approver`` filters."""
        params = {"state": state, "approver": approver}
        return list(self._json("GET", "/v1/approvals", params=params) or [])

    def decide_approval(
        self, approval_id: str, decision: str, actor: Mapping[str, Any], reason: str
    ) -> JsonDict:
        """``POST /v1/approvals/{id}`` with ``approved`` or ``rejected``."""
        body = {"decision": decision, "actor": dict(actor), "reason": reason}
        return dict(self._json("POST", f"/v1/approvals/{approval_id}", json_body=body))

    def metrics(self) -> str:
        """``GET /v1/metrics``: Prometheus text."""
        return self._request("GET", "/v1/metrics").text


class GatewayClient(_HttpClient):
    """Client for the gateway API at ``base_url`` (default port 7402)."""

    def health(self) -> JsonDict:
        """``GET /v1/health``."""
        return dict(self._json("GET", "/v1/health"))

    def tools(self) -> list[JsonDict]:
        """``GET /v1/tools``: every tool the connectors expose."""
        return list(self._json("GET", "/v1/tools") or [])

    def canaries(self) -> list[JsonDict]:
        """``GET /v1/canaries``."""
        return list(self._json("GET", "/v1/canaries") or [])

    def probe(self, connector: str) -> JsonDict:
        """``POST /v1/canaries/{connector}/probe``."""
        return dict(self._json("POST", f"/v1/canaries/{connector}/probe"))

    def release(self, connector: str) -> JsonDict:
        """``POST /v1/canaries/{connector}/release``."""
        return dict(self._json("POST", f"/v1/canaries/{connector}/release"))

    def call_tool(
        self,
        *,
        remit_token: str,
        run_id: str,
        step: str,
        lease_id: str,
        tool: str,
        args: Mapping[str, Any],
        idempotency_key: str | None,
        scope: str | None = None,
        timeout: float | None = None,
    ) -> JsonDict:
        """``POST /v1/tools/call``.

        Returns the 200 body (``ok``, ``result``, ``scope``, ``replayed``,
        ``latency_ms``). A refusal (403), conflict (409), invalid arguments
        (422), quarantine (503) or upstream error (502) raises
        :class:`KernosError` with the matching status and code.
        """
        body = {
            "remit_token": remit_token,
            "run_id": run_id,
            "step": step,
            "lease_id": lease_id,
            "tool": tool,
            "args": dict(args),
            "idempotency_key": idempotency_key,
            "scope": scope,
        }
        return dict(self._json("POST", "/v1/tools/call", json_body=body, timeout=timeout))
