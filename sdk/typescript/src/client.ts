import { KernosError, KernosNetworkError } from "./errors.js";
import type {
  AbandonRequest,
  AbandonResponse,
  ActionProposeRequest,
  ActionProposeResponse,
  AppendEventAuth,
  AppendEventRequest,
  AppendEventResponse,
  Approval,
  ApprovalDecideRequest,
  ApprovalDecideResponse,
  ApprovalListQuery,
  Bundle,
  BundleApplyResponse,
  BundleRecord,
  BundleSignature,
  BundleSummary,
  Event,
  EventsPage,
  EventsQuery,
  ExternalEventKind,
  Health,
  HeartbeatResponse,
  Keys,
  Lease,
  LeaseCompleteRequest,
  LeaseCompleteResponse,
  LeaseFailRequest,
  LeaseFailResponse,
  LeaseRequest,
  PolicyApplyRequest,
  PolicyApplyResponse,
  PolicySummary,
  PolicyTestRequest,
  PolicyTestResponse,
  PolicyVersion,
  Remit,
  RemitDeriveRequest,
  RemitDeriveResponse,
  RemitIssueRequest,
  RemitIssueResponse,
  ReplayResult,
  ResumeRequest,
  ResumeResponse,
  RunListQuery,
  RunListResponse,
  RunStartRequest,
  RunStartResponse,
  RunState,
} from "./types.js";

/** The subset of `fetch` the client needs; the global `fetch` satisfies it. */
export type FetchLike = (input: string, init: RequestInit) => Promise<Response>;

/** Options for `new KernosClient(...)`. */
export interface KernosClientOptions {
  /** Base URL of the kernel, for example `http://127.0.0.1:7401`. A trailing slash is ignored. */
  baseUrl: string;
  /** `KERNOS_TOKEN`; sent as `Authorization: Bearer ...` when set. */
  token?: string | undefined;
  /** Replacement for the global `fetch` (tests, custom agents). */
  fetch?: FetchLike;
  /** Default polling interval for `runs.follow`, in milliseconds. Default 500. */
  pollMs?: number;
  /** Extra headers sent on every request. */
  headers?: Record<string, string>;
}

/** Options for `runs.follow`. */
export interface FollowOptions {
  /** First sequence number to read. Default 1. */
  from_seq?: number;
  /** Polling interval in milliseconds. Defaults to the client's `pollMs`. */
  pollMs?: number;
  /** Page size for each `events` request. Default 500. */
  limit?: number;
  /** Stops the generator early. */
  signal?: AbortSignal;
}

type Query = object;

const TERMINAL_KINDS: ReadonlySet<string> = new Set(["run.completed", "run.failed", "run.abandoned"]);

function buildQuery(query: Query | undefined): string {
  if (!query) return "";
  const parts: string[] = [];
  for (const [key, value] of Object.entries(query as Record<string, unknown>)) {
    if (value === undefined || value === null) continue;
    parts.push(`${encodeURIComponent(key)}=${encodeURIComponent(String(value))}`);
  }
  return parts.length === 0 ? "" : `?${parts.join("&")}`;
}

function sleep(ms: number, signal: AbortSignal | undefined): Promise<void> {
  return new Promise((resolve) => {
    if (signal?.aborted) {
      resolve();
      return;
    }
    const timer = setTimeout(done, ms);
    function done(): void {
      clearTimeout(timer);
      signal?.removeEventListener("abort", done);
      resolve();
    }
    signal?.addEventListener("abort", done, { once: true });
  });
}

/**
 * Typed client for the Kernos kernel and control-plane API (https://rhs2.github.io/kernos/reference/kernel-api/).
 *
 * Every method resolves with the parsed JSON body, throws `KernosError` on a
 * non-2xx response and `KernosNetworkError` when no response arrived.
 */
export class KernosClient {
  readonly baseUrl: string;
  readonly pollMs: number;
  private readonly token: string | undefined;
  private readonly fetchImpl: FetchLike;
  private readonly extraHeaders: Record<string, string>;

  constructor(options: KernosClientOptions) {
    this.baseUrl = options.baseUrl.replace(/\/+$/, "");
    this.token = options.token && options.token.length > 0 ? options.token : undefined;
    this.pollMs = options.pollMs ?? 500;
    this.extraHeaders = { ...(options.headers ?? {}) };
    const impl: FetchLike | undefined = options.fetch ?? (globalThis.fetch as FetchLike | undefined);
    if (!impl) {
      throw new Error("KernosClient needs a global fetch (Node 18+ or a browser) or an explicit `fetch` option");
    }
    this.fetchImpl = impl;
  }

  // -------------------------------------------------------------------------
  // Health and keys
  // -------------------------------------------------------------------------

  /** `GET /v1/health` */
  health(): Promise<Health> {
    return this.request<Health>("GET", "/v1/health");
  }

  /** `GET /v1/keys` */
  keys(): Promise<Keys> {
    return this.request<Keys>("GET", "/v1/keys");
  }

  /** `GET /v1/metrics` as Prometheus text. */
  metrics(): Promise<string> {
    return this.requestText("GET", "/v1/metrics");
  }

  // -------------------------------------------------------------------------
  // Bundles
  // -------------------------------------------------------------------------

  readonly bundles = {
    /** `POST /v1/bundles` with `{bundle, signature}`. */
    apply: (bundle: Bundle, signature: BundleSignature): Promise<BundleApplyResponse> =>
      this.request<BundleApplyResponse>("POST", "/v1/bundles", { bundle, signature }),

    /** `GET /v1/bundles` */
    list: (): Promise<BundleSummary[]> => this.request<BundleSummary[]>("GET", "/v1/bundles"),

    /** `GET /v1/bundles/{id}` */
    get: (bundleId: string): Promise<BundleRecord> =>
      this.request<BundleRecord>("GET", `/v1/bundles/${encodeURIComponent(bundleId)}`),
  };

  // -------------------------------------------------------------------------
  // Policies
  // -------------------------------------------------------------------------

  readonly policies = {
    /** `POST /v1/policies` with `{name, version, source}`. */
    apply: (req: PolicyApplyRequest): Promise<PolicyApplyResponse> =>
      this.request<PolicyApplyResponse>("POST", "/v1/policies", req),

    /** `POST /v1/policies/test` */
    test: (req: PolicyTestRequest): Promise<PolicyTestResponse> =>
      this.request<PolicyTestResponse>("POST", "/v1/policies/test", req),

    /** `GET /v1/policies` */
    list: (): Promise<PolicySummary[]> => this.request<PolicySummary[]>("GET", "/v1/policies"),

    /** `GET /v1/policies/{name}`: every version of a policy. */
    get: (name: string): Promise<PolicySummary[]> =>
      this.request<PolicySummary[]>("GET", `/v1/policies/${encodeURIComponent(name)}`),

    /** `GET /v1/policies/{name}/{version}`: one version with its source. */
    getVersion: (name: string, version: number): Promise<PolicyVersion> =>
      this.request<PolicyVersion>("GET", `/v1/policies/${encodeURIComponent(name)}/${encodeURIComponent(String(version))}`),
  };

  // -------------------------------------------------------------------------
  // Remits
  // -------------------------------------------------------------------------

  readonly remits = {
    /** `POST /v1/remits` */
    issue: (req: RemitIssueRequest): Promise<RemitIssueResponse> =>
      this.request<RemitIssueResponse>("POST", "/v1/remits", req),

    /** `POST /v1/remits/{id}/derive`: every given field must narrow. */
    derive: (remitId: string, req: RemitDeriveRequest): Promise<RemitDeriveResponse> =>
      this.request<RemitDeriveResponse>("POST", `/v1/remits/${encodeURIComponent(remitId)}/derive`, req),

    /** `GET /v1/remits/{id}` */
    get: (remitId: string): Promise<Remit> =>
      this.request<Remit>("GET", `/v1/remits/${encodeURIComponent(remitId)}`),
  };

  // -------------------------------------------------------------------------
  // Runs
  // -------------------------------------------------------------------------

  readonly runs = {
    /** `POST /v1/runs` */
    start: (req: RunStartRequest): Promise<RunStartResponse> =>
      this.request<RunStartResponse>("POST", "/v1/runs", req),

    /** `GET /v1/runs/{id}`: the folded `RunState`. */
    get: (runId: string): Promise<RunState> =>
      this.request<RunState>("GET", `/v1/runs/${encodeURIComponent(runId)}`),

    /** `GET /v1/runs?state=...&department=...&limit=...&after=...` */
    list: (query: RunListQuery = {}): Promise<RunListResponse> =>
      this.request<RunListResponse>("GET", `/v1/runs${buildQuery(query)}`),

    /** `GET /v1/runs/{id}/events?from_seq=...&limit=...` */
    events: (runId: string, query: EventsQuery = {}): Promise<EventsPage> =>
      this.request<EventsPage>("GET", `/v1/runs/${encodeURIComponent(runId)}/events${buildQuery(query)}`),

    /**
     * Polls `events` with `from_seq` every `pollMs` and yields each event in
     * order, finishing after `run.completed`, `run.failed` or `run.abandoned`.
     */
    follow: (runId: string, options: FollowOptions = {}): AsyncGenerator<Event, void, undefined> =>
      this.followEvents(runId, options),

    /** `POST /v1/runs/{id}/replay`: 200 whether or not it verified; the flags say. */
    replay: (runId: string): Promise<ReplayResult> =>
      this.request<ReplayResult>("POST", `/v1/runs/${encodeURIComponent(runId)}/replay`),

    /** `POST /v1/runs/{id}/abandon` */
    abandon: (runId: string, req: AbandonRequest): Promise<AbandonResponse> =>
      this.request<AbandonResponse>("POST", `/v1/runs/${encodeURIComponent(runId)}/abandon`, req),

    /** `POST /v1/runs/{id}/resume` */
    resume: (runId: string, req: ResumeRequest): Promise<ResumeResponse> =>
      this.request<ResumeResponse>("POST", `/v1/runs/${encodeURIComponent(runId)}/resume`, req),

    /**
     * `POST /v1/runs/{id}/events`: append an external event. The poster must
     * hold the step's lease (`auth.lease`) or be the gateway (`auth.remit`).
     */
    appendEvent: <K extends ExternalEventKind>(
      runId: string,
      req: AppendEventRequest<K>,
      auth: AppendEventAuth = {},
    ): Promise<AppendEventResponse> => {
      const headers: Record<string, string> = {};
      if (auth.lease) headers["X-Kernos-Lease"] = auth.lease;
      if (auth.remit) headers["X-Kernos-Remit"] = auth.remit;
      return this.request<AppendEventResponse>("POST", `/v1/runs/${encodeURIComponent(runId)}/events`, req, headers);
    },
  };

  // -------------------------------------------------------------------------
  // Leases (worker side)
  // -------------------------------------------------------------------------

  readonly leases = {
    /** `POST /v1/leases`: `null` when the kernel answers 204 (nothing runnable). */
    acquire: (req: LeaseRequest): Promise<Lease | null> =>
      this.request<Lease | null>("POST", "/v1/leases", req, {}, { allowNoContent: true }),

    /** `POST /v1/leases/{id}/heartbeat` */
    heartbeat: (leaseId: string): Promise<HeartbeatResponse> =>
      this.request<HeartbeatResponse>("POST", `/v1/leases/${encodeURIComponent(leaseId)}/heartbeat`),

    /** `POST /v1/leases/{id}/complete` */
    complete: (leaseId: string, req: LeaseCompleteRequest): Promise<LeaseCompleteResponse> =>
      this.request<LeaseCompleteResponse>("POST", `/v1/leases/${encodeURIComponent(leaseId)}/complete`, req),

    /** `POST /v1/leases/{id}/fail` */
    fail: (leaseId: string, req: LeaseFailRequest): Promise<LeaseFailResponse> =>
      this.request<LeaseFailResponse>("POST", `/v1/leases/${encodeURIComponent(leaseId)}/fail`, req),

    /** `POST /v1/leases/{id}/actions`: propose an action for policy evaluation. */
    propose: (leaseId: string, req: ActionProposeRequest): Promise<ActionProposeResponse> =>
      this.request<ActionProposeResponse>("POST", `/v1/leases/${encodeURIComponent(leaseId)}/actions`, req),
  };

  // -------------------------------------------------------------------------
  // Approvals
  // -------------------------------------------------------------------------

  readonly approvals = {
    /** `GET /v1/approvals?state=pending&approver=role:finance_admin` */
    list: (query: ApprovalListQuery = {}): Promise<Approval[]> =>
      this.request<Approval[]>("GET", `/v1/approvals${buildQuery(query)}`),

    /** `POST /v1/approvals/{id}` with `{decision, actor, reason}`. */
    decide: (approvalId: string, req: ApprovalDecideRequest): Promise<ApprovalDecideResponse> =>
      this.request<ApprovalDecideResponse>("POST", `/v1/approvals/${encodeURIComponent(approvalId)}`, req),
  };

  // -------------------------------------------------------------------------
  // Internals
  // -------------------------------------------------------------------------

  private async *followEvents(runId: string, options: FollowOptions): AsyncGenerator<Event, void, undefined> {
    const pollMs = options.pollMs ?? this.pollMs;
    const limit = options.limit ?? 500;
    const signal = options.signal;
    let from = options.from_seq ?? 1;
    for (;;) {
      if (signal?.aborted) return;
      const page = await this.runs.events(runId, { from_seq: from, limit });
      for (const ev of page.events) {
        if (ev.seq < from) continue;
        from = ev.seq + 1;
        yield ev;
        if (TERMINAL_KINDS.has(ev.kind)) return;
      }
      if (page.events.length > 0 && page.next_seq !== null && page.next_seq !== undefined) {
        from = Math.max(from, page.next_seq);
        continue;
      }
      await sleep(pollMs, signal);
    }
  }

  private headersFor(body: unknown, extra: Record<string, string>): Record<string, string> {
    const headers: Record<string, string> = { accept: "application/json", ...this.extraHeaders };
    if (body !== undefined) headers["content-type"] = "application/json";
    if (this.token !== undefined) headers.authorization = `Bearer ${this.token}`;
    for (const [k, v] of Object.entries(extra)) headers[k] = v;
    return headers;
  }

  private async send(method: string, path: string, body: unknown, extra: Record<string, string>): Promise<{ res: Response; text: string; url: string }> {
    const url = this.baseUrl + path;
    const init: RequestInit = { method, headers: this.headersFor(body, extra) };
    if (body !== undefined) init.body = JSON.stringify(body);
    let res: Response;
    try {
      res = await this.fetchImpl(url, init);
    } catch (cause) {
      throw new KernosNetworkError(`${method} ${url}: ${describe(cause)}`, cause, { method, url });
    }
    let text: string;
    try {
      text = await res.text();
    } catch (cause) {
      throw new KernosNetworkError(`${method} ${url}: response body could not be read: ${describe(cause)}`, cause, { method, url });
    }
    if (!res.ok) throw KernosError.fromResponse(res.status, text, { method, url });
    return { res, text, url };
  }

  private async request<T>(
    method: string,
    path: string,
    body?: unknown,
    extra: Record<string, string> = {},
    opts: { allowNoContent?: boolean } = {},
  ): Promise<T> {
    const { res, text, url } = await this.send(method, path, body, extra);
    if (res.status === 204 || text.length === 0) {
      if (opts.allowNoContent) return null as T;
      throw new KernosError(res.status, "response_invalid", `${method} ${url} returned an empty body`, {}, { method, url });
    }
    try {
      return JSON.parse(text) as T;
    } catch {
      throw new KernosError(res.status, "response_invalid", `${method} ${url} returned a body that is not JSON`, { body: text.slice(0, 200) }, { method, url });
    }
  }

  private async requestText(method: string, path: string): Promise<string> {
    const { text } = await this.send(method, path, undefined, { accept: "text/plain" });
    return text;
  }
}

function describe(cause: unknown): string {
  if (cause instanceof Error) {
    const inner = (cause as { cause?: unknown }).cause;
    if (inner instanceof Error && inner.message) return `${cause.message} (${inner.message})`;
    return cause.message;
  }
  return String(cause);
}
