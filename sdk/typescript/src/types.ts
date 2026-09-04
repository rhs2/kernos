/**
 * Types for the Kernos kernel and control-plane API.
 *
 * Every shape here mirrors https://rhs2.github.io/kernos/reference/events/ and https://rhs2.github.io/kernos/reference/kernel-api/.
 * Keys are snake_case exactly as they travel over the wire.
 */

/** A JSON value. */
export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

/** A JSON object. */
export type JsonObject = { [key: string]: Json };

/** Money as it appears in policies and remits: amounts are JSON numbers. */
export interface Money {
  amount: number;
  currency: string;
}

/** The error envelope every non-2xx kernel response carries. */
export interface ErrorBody {
  error: {
    code: string;
    message: string;
    details?: Record<string, unknown>;
  };
}

/** Who requested a run or a remit. */
export interface RequestedBy {
  id: string;
  role: string;
  manager: string | null;
}

/** An actor recorded on an event, an approval or an abandon request. */
export interface Actor {
  id: string;
  role?: string;
}

/** Actor field on an event record (01-EVENTS). */
export interface EventActor {
  type: "kernel" | "worker" | "gateway" | "policy" | "user" | "system";
  id: string;
}

/** Autonomy levels, ordered from least to most (03-REMIT). */
export type Autonomy = "observe" | "propose" | "supervised" | "autonomous";

/** Result of a policy evaluation (04-POLICY). */
export type Decision = "allow" | "approval_required" | "deny";

/** A resolved approver (04-POLICY, approver resolution). */
export interface Approver {
  type: "role" | "user";
  value: string;
  fallback?: boolean;
}

/** Run states (01-EVENTS state machines). */
export type RunStateName = "created" | "running" | "parked" | "completed" | "failed" | "abandoned";

/** Step states (01-EVENTS state machines). */
export type StepStateName =
  | "scheduled"
  | "leased"
  | "completed"
  | "failed"
  | "quarantined"
  | "waiting_approval";

/** Step kinds a bundle may declare, plus the kernel-scheduled compensation kind. */
export type StepKind = "model" | "tool" | "action" | "compensation";

/** Model tiers (05-BUNDLE, 07-REASONING-SDK). */
export type Tier = "deep" | "standard" | "cheap";

/** Model effort levels (05-BUNDLE). */
export type Effort = "low" | "medium" | "high" | "xhigh";

/** Reasons a run can be parked (01-EVENTS, run.parked). */
export type ParkReason =
  | "approval"
  | "budget"
  | "quarantine"
  | "connector_quarantined"
  | "refusal"
  | "human";

// ---------------------------------------------------------------------------
// Health and keys
// ---------------------------------------------------------------------------

export interface Health {
  ok: boolean;
  version: string;
  uptime_s: number;
  runs: { running: number; parked: number; [state: string]: number };
}

export interface Keys {
  key_id: string;
  algorithm: "ed25519" | string;
  public_key: string;
}

// ---------------------------------------------------------------------------
// Bundles (05-BUNDLE)
// ---------------------------------------------------------------------------

export interface BundleTool {
  id: string;
  description?: string;
  writes: boolean;
}

export interface BundlePrompt {
  system: string;
  user: string;
}

/** A `{"$ref": "path"}` placeholder inside args or actions. */
export interface Ref {
  $ref: string;
}

/** A templated value: a literal, a `$ref`, or a nested structure of either. */
export type Templated = Json | Ref | Templated[] | { [key: string]: Templated };

export interface BundleStepBase {
  id: string;
  kind: StepKind;
  description?: string;
  timeout_seconds?: number;
}

export interface BundleModelStep extends BundleStepBase {
  kind: "model";
  tier: Tier;
  effort?: Effort;
  prompt: string;
  output_schema: JsonObject;
  max_output_tokens?: number;
  on_refusal?: "park" | "escalate" | "fail";
  escalate?: { when_confidence_below: number; to_tier: Tier };
  data_classes?: string[];
}

export interface BundleCompensation {
  tool: string;
  args: { [key: string]: Templated };
}

export interface BundleToolStep extends BundleStepBase {
  kind: "tool";
  tool: string;
  args: { [key: string]: Templated };
  idempotency_key?: string;
  compensation?: BundleCompensation;
  scope?: string;
}

export interface BundleActionStep extends BundleStepBase {
  kind: "action";
  action: { [key: string]: Templated } & { kind: string };
}

export type BundleStep = BundleModelStep | BundleToolStep | BundleActionStep;

export interface BundleWorkflow {
  description?: string;
  input_schema: JsonObject;
  steps: BundleStep[];
}

export interface Bundle {
  format: "kernos.bundle/1";
  name: string;
  version: string;
  department?: string;
  description?: string;
  policies: string[];
  tools: BundleTool[];
  prompts: { [name: string]: BundlePrompt };
  mock?: { [prompt: string]: Templated };
  workflows: { [name: string]: BundleWorkflow };
}

/** Signature file produced by `kernos bundle sign` (05-BUNDLE). */
export interface BundleSignature {
  key_id: string;
  signature: string;
  algorithm?: "ed25519" | string;
  sha256?: string;
}

export interface BundleApplyRequest {
  bundle: Bundle;
  signature: BundleSignature;
}

export interface BundleApplyResponse {
  bundle_id: string;
  name: string;
  version: string;
}

export interface BundleSummary {
  bundle_id: string;
  name: string;
  version: string;
  department: string | null;
  workflows: string[];
  created_at: string;
}

export interface BundleRecord extends Bundle {
  bundle_id: string;
  signature: BundleSignature;
  created_at?: string;
}

// ---------------------------------------------------------------------------
// Policies (04-POLICY)
// ---------------------------------------------------------------------------

export interface PolicyApplyRequest {
  name: string;
  version: number;
  source: string;
}

export interface PolicyApplyResponse {
  policy_id: string;
  name: string;
  version: number;
}

export interface PolicySummary {
  policy_id: string;
  name: string;
  version: number;
  created_at: string;
}

export interface PolicyVersion extends PolicySummary {
  source: string;
}

export interface PolicyRefByName {
  name: string;
  version: number;
}

export interface PolicyRefBySource {
  source: string;
}

/** The action object policies evaluate (04-POLICY, context). */
export interface Action {
  kind: string;
  amount?: number | null;
  currency?: string | null;
  writes_to_system_of_record: boolean;
  target?: string | null;
  data_classes?: string[];
  paths?: string[];
  idempotency_key?: string | null;
  summary?: string;
  [extra: string]: unknown;
}

/** The run half of a policy corpus entry (04-POLICY, context). */
export interface PolicyRunContext {
  id?: string;
  department?: string | null;
  bundle?: { name: string; version: string };
  workflow?: string;
  remit?: {
    autonomy?: Autonomy;
    grants?: string[];
    tools?: string[];
    scopes?: string[];
  };
  requested_by?: RequestedBy;
  [extra: string]: unknown;
}

export interface PolicyCorpusEntry {
  action: Action;
  run: PolicyRunContext;
}

export interface PolicyTestRequest {
  policy_a: PolicyRefByName | PolicyRefBySource;
  policy_b: PolicyRefByName | PolicyRefBySource;
  corpus: PolicyCorpusEntry[];
}

export interface PolicyFlip {
  index: number;
  a: Decision;
  b: Decision;
  rule_a: string;
  rule_b: string;
}

export interface PolicyTestResponse {
  cases: number;
  flips: PolicyFlip[];
}

/** One entry of `RunState.decisions`, and the core of a `policy.decided` payload. */
export interface PolicyDecision {
  action_id: string;
  decision: Decision;
  rule: string;
  policy: string | null;
  policy_version: number | null;
}

// ---------------------------------------------------------------------------
// Remits (03-REMIT)
// ---------------------------------------------------------------------------

export interface Spend {
  tokens: number;
  usd: number;
}

export interface RemitIssueRequest {
  tools: string[];
  scopes: string[];
  grants?: string[];
  spend: Spend;
  autonomy: Autonomy;
  ttl_seconds: number;
  policy_set: string[];
  requested_by: RequestedBy;
}

export interface RemitIssueResponse {
  remit_id: string;
  token: string;
  expires_at: string;
}

export interface RemitDeriveRequest {
  tools?: string[];
  scopes?: string[];
  grants?: string[];
  spend?: Partial<Spend>;
  autonomy?: Autonomy;
  ttl_seconds?: number;
}

export interface RemitDeriveResponse extends RemitIssueResponse {
  parent_id: string;
}

/** `GET /v1/remits/{id}`: the token payload without its signature. */
export interface Remit {
  rid: string;
  parent?: string | null;
  run?: string | null;
  iss: string;
  iat: number;
  nbf: number;
  exp: number;
  tools: string[];
  scopes: string[];
  grants: string[];
  spend: Spend;
  autonomy: Autonomy;
  policy_set: string[];
  requested_by: RequestedBy;
  parent_id: string | null;
  run_id: string | null;
}

// ---------------------------------------------------------------------------
// Events (01-EVENTS)
// ---------------------------------------------------------------------------

export interface ModelUsage {
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
}

export interface StepError {
  code: string;
  message: string;
}

export interface Budget {
  tokens: number;
  usd: number;
  soft_ratio: number;
}

/** Payload of each event kind, keyed by kind. */
export interface EventPayloads {
  "run.created": {
    bundle_id: string;
    bundle_name: string;
    bundle_version: string;
    workflow: string;
    input: JsonObject;
    remit_id: string;
    requested_by: RequestedBy;
    budget: Budget;
  };
  "step.scheduled": { step: string; index: number; kind: StepKind };
  "step.leased": { step: string; lease_id: string; worker_id: string; attempt: number; expires_at: string };
  "step.lease_expired": { step: string; lease_id: string; worker_id: string };
  "step.completed": { step: string; lease_id: string; attempt: number; output: Json };
  "step.failed": { step: string; lease_id: string; attempt: number; error: StepError; deterministic: boolean };
  "step.retry_scheduled": { step: string; attempt: number; delay_ms: number };
  "step.quarantined": { step: string; reason: string; attempts: number };
  "step.escalated": { step: string; from_tier: Tier; to_tier: Tier; reason: string };
  "step.waiting_approval": { step: string; action_id: string; approval_id: string };
  "model.called": {
    step: string;
    model: string;
    tier: Tier;
    effort: Effort;
    provider: string;
    prefix_hash: string;
    input_hash: string;
    max_tokens: number;
  };
  "model.responded": {
    step: string;
    output: Json;
    usage: ModelUsage;
    cost_usd: number;
    stop_reason: string;
    refusal: boolean;
    latency_ms: number;
  };
  "tool.called": { step: string; tool: string; args: JsonObject; scope: string | null; idempotency_key: string | null };
  "tool.result": { step: string; tool: string; ok: boolean; result: Json; replayed: boolean; latency_ms: number };
  "tool.refused": { step: string; tool: string; reason: string; remit_id: string; detail: string };
  "action.proposed": { action_id: string; step: string; action: Action };
  "policy.decided": PolicyDecision & {
    approver: Approver | null;
    sla_seconds: number | null;
    escalate_to: Approver | string | null;
  };
  "approval.requested": {
    approval_id: string;
    action_id: string;
    approver: Approver;
    sla_seconds: number;
    escalate_to: Approver | string | null;
    due_at: string;
  };
  "approval.decided": {
    approval_id: string;
    action_id: string;
    decision: "approved" | "rejected";
    actor: { id: string; role: string };
    reason: string;
  };
  "approval.escalated": { approval_id: string; from: Approver; to: Approver; reason: string };
  "usage.recorded": { step: string; tokens: number; usd: number; cumulative_tokens: number; cumulative_usd: number };
  "budget.soft_threshold": { cumulative_usd: number; ceiling_usd: number; ratio: number };
  "budget.exceeded": { cumulative_usd?: number; ceiling_usd?: number; cumulative_tokens?: number; ceiling_tokens?: number };
  "run.parked": { reason: ParkReason; detail: Json };
  "run.resumed": { reason: string };
  "run.abandoned": { reason: string; actor: Actor };
  "compensation.scheduled": { step: string; for_step: string; tool: string; args: JsonObject };
  "compensation.completed": { step: string; for_step: string; result: Json };
  "compensation.failed": { step: string; for_step: string; error: StepError };
  "run.completed": { output: Json };
  "run.failed": { error: StepError; needs_human: boolean };
  note: { text?: string; data?: Json; [extra: string]: unknown };
}

/** Every event kind of kernos.events/1. */
export type EventKind = keyof EventPayloads;

/** Kinds that external actors (workers, the gateway) may append. */
export type ExternalEventKind =
  | "step.escalated"
  | "model.called"
  | "model.responded"
  | "tool.called"
  | "tool.result"
  | "tool.refused"
  | "note";

/** Kinds after which a run no longer changes. */
export type TerminalEventKind = "run.completed" | "run.failed" | "run.abandoned";

/** An event record with a payload typed by its kind. */
export type Event<K extends EventKind = EventKind> = {
  [Kind in K]: {
    schema: "kernos.events/1";
    run_id: string;
    seq: number;
    ts: string;
    kind: Kind;
    actor: EventActor;
    payload: EventPayloads[Kind];
    prev_hash: string;
    hash: string;
  };
}[K];

export interface EventsQuery {
  from_seq?: number;
  limit?: number;
}

export interface EventsPage {
  events: Event[];
  next_seq: number | null;
}

export interface AppendEventRequest<K extends ExternalEventKind = ExternalEventKind> {
  kind: K;
  payload: EventPayloads[K];
  actor: EventActor;
}

export interface AppendEventResponse {
  seq: number;
  hash: string;
}

/** Credentials that authorise an external append (02-KERNEL-API, POST events). */
export interface AppendEventAuth {
  /** `X-Kernos-Lease`: the lease the poster currently holds on the step. */
  lease?: string;
  /** `X-Kernos-Remit`: a remit token valid for the run (the gateway). */
  remit?: string;
}

// ---------------------------------------------------------------------------
// Runs (01-EVENTS fold, 02-KERNEL-API)
// ---------------------------------------------------------------------------

export interface RunStartRequest {
  bundle_id: string;
  workflow: string;
  input: JsonObject;
  remit_id: string;
  requested_by: RequestedBy;
}

export interface RunStartResponse {
  run_id: string;
  state: RunStateName;
}

export interface RunStep {
  id: string;
  index: number;
  kind: StepKind;
  state: StepStateName;
  attempts: number;
  lease: { lease_id: string; worker_id: string; expires_at: string } | null;
  output: Json | null;
  error: StepError | null;
  action_id: string | null;
  approval_id: string | null;
}

export interface RunBudget {
  ceiling_tokens: number;
  ceiling_usd: number;
  soft_ratio: number;
  used_tokens: number;
  used_usd: number;
  soft_hit: boolean;
  exceeded: boolean;
}

export interface PendingApproval {
  approval_id: string;
  action_id: string;
  approver: Approver;
  due_at: string;
}

export interface Compensation {
  for_step: string;
  state: StepStateName;
}

/** `fold(events)` of 01-EVENTS plus the bundle reference added by `GET /v1/runs/{id}`. */
export interface RunState {
  run_id: string;
  state: RunStateName;
  bundle: { id: string; name: string; version: string };
  workflow: string;
  input: JsonObject;
  remit_id: string;
  requested_by: RequestedBy;
  steps: RunStep[];
  budget: RunBudget;
  pending_approval: PendingApproval | null;
  decisions: PolicyDecision[];
  compensations: Compensation[];
  output: Json | null;
  error: StepError | null;
  needs_human: boolean;
  last_seq: number;
  department?: string | null;
  created_at?: string;
  updated_at?: string;
}

export interface RunListQuery {
  state?: RunStateName;
  department?: string;
  limit?: number;
  after?: string;
}

/** One row of `GET /v1/runs`. The kernel returns at least these fields. */
export interface RunSummary {
  run_id: string;
  state: RunStateName;
  bundle?: { id: string; name: string; version: string };
  workflow?: string;
  department?: string | null;
  requested_by?: RequestedBy;
  created_at?: string;
  [extra: string]: unknown;
}

export interface RunListResponse {
  runs: RunSummary[];
  next: string | null;
}

export interface ChainError {
  seq: number;
  message?: string;
  [extra: string]: unknown;
}

export interface DecisionMismatch {
  seq?: number;
  action_id?: string;
  recorded?: Decision;
  recomputed?: Decision;
  [extra: string]: unknown;
}

export interface ReplayResult {
  chain_valid: boolean;
  events: number;
  state_matches: boolean;
  decisions: number;
  decision_mismatches: DecisionMismatch[];
  chain_errors: ChainError[];
  state: RunState;
}

export interface AbandonRequest {
  reason: string;
  actor: Actor;
}

export interface AbandonResponse {
  compensations_scheduled: number;
}

export interface ResumeRequest {
  actor: Actor;
}

export interface ResumeResponse {
  run_id?: string;
  run_state?: RunStateName;
  [extra: string]: unknown;
}

// ---------------------------------------------------------------------------
// Leases (02-KERNEL-API, workers)
// ---------------------------------------------------------------------------

export interface LeaseRequest {
  worker_id: string;
  kinds: StepKind[];
  ttl_seconds?: number;
}

export interface LeaseContext {
  input: JsonObject;
  steps: { [stepId: string]: { output: Json } };
  run: {
    id: string;
    bundle: { name: string; version: string };
    workflow: string;
    requested_by: RequestedBy;
    department: string | null;
  };
  remit_token: string;
  remit: { autonomy: Autonomy; grants: string[]; tools: string[]; scopes: string[] };
  prompts: { [name: string]: BundlePrompt };
  mock: { [prompt: string]: Templated };
  tools: BundleTool[];
  pacing: boolean;
  approved_actions: string[];
  prior_events: Event[];
}

export interface Lease {
  lease_id: string;
  run_id: string;
  step: string;
  attempt: number;
  expires_at: string;
  heartbeat_seconds: number;
  step_def: BundleStep | (JsonObject & { id: string; kind: StepKind });
  context: LeaseContext;
}

export interface HeartbeatResponse {
  expires_at: string;
}

export interface LeaseCompleteRequest {
  output: Json;
  usage?: { tokens: number; usd: number };
}

export interface LeaseCompleteResponse {
  run_state: RunStateName;
  next_step: string | null;
}

export interface LeaseFailRequest {
  error: StepError;
  deterministic: boolean;
}

export interface LeaseFailResponse {
  outcome: "retry_scheduled" | "quarantined";
  delay_ms?: number;
}

export interface ActionProposeRequest {
  action: Action;
}

export interface ActionProposeResponse {
  action_id: string;
  decision: Decision;
  rule: string;
  approval_id?: string;
}

// ---------------------------------------------------------------------------
// Approvals (02-KERNEL-API, 04-POLICY)
// ---------------------------------------------------------------------------

export interface ApprovalListQuery {
  state?: "pending" | "approved" | "rejected" | "escalated" | string;
  /** `role:finance_admin` or a user id. */
  approver?: string;
}

export interface Approval {
  approval_id: string;
  run_id: string;
  action_id: string;
  action: Action;
  approver: Approver;
  requested_at: string;
  due_at: string;
  escalations: number;
  state?: string;
  [extra: string]: unknown;
}

export interface ApprovalDecideRequest {
  decision: "approved" | "rejected";
  actor: { id: string; role: string };
  reason: string;
}

export interface ApprovalDecideResponse {
  run_id: string;
  run_state: RunStateName;
}
