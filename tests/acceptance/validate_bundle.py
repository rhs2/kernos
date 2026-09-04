#!/usr/bin/env python3
"""Validate a Kernos bundle against the rules of https://rhs2.github.io/kernos/reference/bundle-format/.

Usage: python3 tests/acceptance/validate_bundle.py [path/to/bundle.json]

Checks, each reported with the JSON path of the offence:
  format and version strings, the name pattern, unique tool ids,
  system prompts free of templates, prompt and tool references that exist,
  step ids that match [a-z][a-z0-9_]* and are unique per workflow,
  idempotency keys on every tool step whose tool writes,
  escalate needing confidence in the output schema,
  $ref and {{template}} paths rooted at input, steps or run,
  references only to earlier steps (a compensation may reference its own step),
  and a canonical size of at most 1 MiB.

Exit code 0 when the bundle is valid, 1 otherwise, 2 on a usage error.
Standard library only.
"""
from __future__ import annotations

import json
import re
import sys
from pathlib import Path
from typing import Any

NAME_RE = re.compile(r"^[a-z0-9_]+(\.[a-z0-9_]+)*$")
STEP_ID_RE = re.compile(r"^[a-z][a-z0-9_]*$")
TOOL_ID_RE = re.compile(r"^[a-z0-9_]+\.[a-z0-9_]+$")
SEMVER_RE = re.compile(r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$")
TEMPLATE_RE = re.compile(r"\{\{(.*?)\}\}")
TIERS = {"deep", "standard", "cheap"}
EFFORTS = {"low", "medium", "high", "xhigh"}
ON_REFUSAL = {"park", "escalate", "fail"}
STEP_KINDS = {"model", "tool", "action"}
RUN_FIELDS = {"id", "workflow", "department", "requested_by"}
MAX_CANONICAL_BYTES = 1024 * 1024
DEFAULT_BUNDLE = Path(__file__).resolve().parents[2] / "bundles" / "reference" / "halcyon" / "bundle.json"


class Report:
    def __init__(self) -> None:
        self.problems: list[tuple[str, str]] = []

    def error(self, path: str, message: str) -> None:
        self.problems.append((path, message))

    @property
    def ok(self) -> bool:
        return not self.problems


def canonical_bytes(obj: Any) -> bytes:
    return json.dumps(obj, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8")


def collect_paths(value: Any, json_path: str, out: list[tuple[str, str, str]]) -> None:
    """Collect every $ref and every {{template}} path inside value.

    Emits (path, form, json_path) where form is "ref" or "template".
    """
    if isinstance(value, dict):
        if set(value.keys()) == {"$ref"}:
            ref = value["$ref"]
            if not isinstance(ref, str):
                out.append(("", "ref", json_path + ".$ref"))
            else:
                out.append((ref, "ref", json_path + ".$ref"))
            return
        for key, item in value.items():
            collect_paths(item, f"{json_path}.{key}", out)
    elif isinstance(value, list):
        for i, item in enumerate(value):
            collect_paths(item, f"{json_path}[{i}]", out)
    elif isinstance(value, str):
        for m in TEMPLATE_RE.finditer(value):
            out.append((m.group(1).strip(), "template", json_path))


def check_path(
    path: str,
    form: str,
    json_path: str,
    report: Report,
    *,
    all_steps: set[str],
    earlier_steps: set[str],
    own_step: str | None,
    allow_own: bool,
) -> None:
    label = "$ref" if form == "ref" else "template"
    if not path:
        report.error(json_path, f"empty {label} path")
        return
    segments = path.split(".")
    if any(seg == "" for seg in segments):
        report.error(json_path, f"{label} path {path!r} has an empty segment")
        return
    root = segments[0]
    if root not in ("input", "steps", "run"):
        report.error(json_path, f"{label} path {path!r} must start with input, steps or run")
        return
    if root == "steps":
        if len(segments) < 3 or segments[2] != "output":
            report.error(json_path, f"{label} path {path!r} must have the form steps.<id>.output...")
            return
        step_id = segments[1]
        if step_id not in all_steps:
            report.error(json_path, f"{label} path {path!r} names an unknown step {step_id!r}")
        elif step_id == own_step:
            if not allow_own:
                report.error(json_path, f"{label} path {path!r} references the step's own output")
        elif step_id not in earlier_steps:
            report.error(json_path, f"{label} path {path!r} references a later step {step_id!r}")
    elif root == "run":
        if len(segments) < 2 or segments[1] not in RUN_FIELDS:
            report.error(json_path, f"{label} path {path!r} must name one of run.{{{', '.join(sorted(RUN_FIELDS))}}}")


def check_templated(value: Any, json_path: str, report: Report, **ctx: Any) -> None:
    found: list[tuple[str, str, str]] = []
    collect_paths(value, json_path, found)
    for path, form, at in found:
        check_path(path, form, at, report, **ctx)


def check_schema(schema: Any, json_path: str, report: Report) -> None:
    if not isinstance(schema, dict):
        report.error(json_path, "must be a JSON Schema object")
        return
    if "type" in schema and not isinstance(schema["type"], (str, list)):
        report.error(json_path + ".type", "must be a string or list")
    props = schema.get("properties")
    if props is not None and not isinstance(props, dict):
        report.error(json_path + ".properties", "must be an object")
    req = schema.get("required")
    if req is not None:
        if not isinstance(req, list) or not all(isinstance(r, str) for r in req):
            report.error(json_path + ".required", "must be a list of strings")
        elif isinstance(props, dict):
            for r in req:
                if r not in props:
                    report.error(json_path + ".required", f"requires {r!r} which is not in properties")


def validate(bundle: Any, report: Report) -> dict[str, Any]:
    stats = {"workflows": 0, "steps": 0}
    if not isinstance(bundle, dict):
        report.error("$", "bundle must be a JSON object")
        return stats

    if bundle.get("format") != "kernos.bundle/1":
        report.error("$.format", "must be 'kernos.bundle/1'")
    name = bundle.get("name")
    if not isinstance(name, str) or not NAME_RE.match(name) or len(name) > 120:
        report.error("$.name", "must match [a-z0-9_]+(\\.[a-z0-9_]+)* and be at most 120 characters")
    version = bundle.get("version")
    if not isinstance(version, str) or not SEMVER_RE.match(version):
        report.error("$.version", "must be a semantic version")
    if "department" in bundle and not isinstance(bundle["department"], str):
        report.error("$.department", "must be a string")
    policies = bundle.get("policies")
    if not isinstance(policies, list) or not all(isinstance(p, str) and p for p in policies):
        report.error("$.policies", "must be a list of policy names")

    tools_decl: dict[str, bool] = {}
    tools = bundle.get("tools")
    if not isinstance(tools, list):
        report.error("$.tools", "must be a list")
        tools = []
    for i, tool in enumerate(tools):
        at = f"$.tools[{i}]"
        if not isinstance(tool, dict):
            report.error(at, "must be an object")
            continue
        tid = tool.get("id")
        if not isinstance(tid, str) or not TOOL_ID_RE.match(tid):
            report.error(at + ".id", "must be connector.operation in lowercase")
            continue
        if tid in tools_decl:
            report.error(at + ".id", f"duplicate tool id {tid!r}")
        if not isinstance(tool.get("writes"), bool):
            report.error(at + ".writes", "must be a boolean")
        tools_decl[tid] = bool(tool.get("writes"))

    prompts = bundle.get("prompts")
    if not isinstance(prompts, dict):
        report.error("$.prompts", "must be an object")
        prompts = {}
    for pname, prompt in prompts.items():
        at = f"$.prompts.{pname}"
        if not isinstance(prompt, dict):
            report.error(at, "must be an object with system and user")
            continue
        system = prompt.get("system")
        if not isinstance(system, str) or not system.strip():
            report.error(at + ".system", "must be a non-empty string")
        elif "{{" in system:
            report.error(at + ".system", "system prompts are frozen and may not contain '{{'")
        user = prompt.get("user")
        if not isinstance(user, str):
            report.error(at + ".user", "must be a string")

    mock = bundle.get("mock", {})
    if mock is not None and not isinstance(mock, dict):
        report.error("$.mock", "must be an object keyed by prompt name")
        mock = {}
    for mname in mock or {}:
        if mname not in prompts:
            report.error(f"$.mock.{mname}", f"no prompt named {mname!r}")

    workflows = bundle.get("workflows")
    if not isinstance(workflows, dict) or not workflows:
        report.error("$.workflows", "must be a non-empty object")
        workflows = {}

    for wname, wf in workflows.items():
        wat = f"$.workflows.{wname}"
        stats["workflows"] += 1
        if not STEP_ID_RE.match(wname):
            report.error(wat, "workflow name must match [a-z][a-z0-9_]*")
        if not isinstance(wf, dict):
            report.error(wat, "must be an object")
            continue
        check_schema(wf.get("input_schema"), wat + ".input_schema", report)
        steps = wf.get("steps")
        if not isinstance(steps, list) or not steps:
            report.error(wat + ".steps", "must be a non-empty list")
            continue
        ids: list[str] = []
        for i, step in enumerate(steps):
            sid = step.get("id") if isinstance(step, dict) else None
            if isinstance(sid, str) and STEP_ID_RE.match(sid):
                if sid in ids:
                    report.error(f"{wat}.steps[{i}].id", f"duplicate step id {sid!r}")
                ids.append(sid)
        all_steps = set(ids)
        earlier: set[str] = set()
        for i, step in enumerate(steps):
            sat = f"{wat}.steps[{i}]"
            stats["steps"] += 1
            if not isinstance(step, dict):
                report.error(sat, "must be an object")
                continue
            sid = step.get("id")
            if not isinstance(sid, str) or not STEP_ID_RE.match(sid):
                report.error(sat + ".id", "must match [a-z][a-z0-9_]*")
                sid = None
            kind = step.get("kind")
            if kind not in STEP_KINDS:
                report.error(sat + ".kind", "must be one of model, tool, action")
            if "timeout_seconds" in step and (not isinstance(step["timeout_seconds"], (int, float)) or step["timeout_seconds"] <= 0):
                report.error(sat + ".timeout_seconds", "must be a positive number")
            if "description" in step and not isinstance(step["description"], str):
                report.error(sat + ".description", "must be a string")
            ctx = dict(all_steps=all_steps, earlier_steps=set(earlier), own_step=sid, allow_own=False)

            if kind == "model":
                if step.get("tier") not in TIERS:
                    report.error(sat + ".tier", "must be deep, standard or cheap")
                if "effort" in step and step["effort"] not in EFFORTS:
                    report.error(sat + ".effort", "must be low, medium, high or xhigh")
                pname = step.get("prompt")
                if not isinstance(pname, str) or pname not in prompts:
                    report.error(sat + ".prompt", f"references an unknown prompt {pname!r}")
                else:
                    prompt = prompts[pname]
                    if isinstance(prompt, dict) and isinstance(prompt.get("user"), str):
                        check_templated(prompt["user"], f"$.prompts.{pname}.user (used by {sat})", report, **ctx)
                    if isinstance(mock, dict) and pname in mock:
                        check_templated(mock[pname], f"$.mock.{pname} (used by {sat})", report, **ctx)
                schema = step.get("output_schema")
                if schema is None:
                    report.error(sat + ".output_schema", "model steps need an output_schema")
                else:
                    check_schema(schema, sat + ".output_schema", report)
                if "max_output_tokens" in step and (not isinstance(step["max_output_tokens"], int) or step["max_output_tokens"] <= 0):
                    report.error(sat + ".max_output_tokens", "must be a positive integer")
                if "on_refusal" in step and step["on_refusal"] not in ON_REFUSAL:
                    report.error(sat + ".on_refusal", "must be park, escalate or fail")
                esc = step.get("escalate")
                if esc is not None:
                    if not isinstance(esc, dict):
                        report.error(sat + ".escalate", "must be an object")
                    else:
                        thr = esc.get("when_confidence_below")
                        if not isinstance(thr, (int, float)) or isinstance(thr, bool) or not 0 <= thr <= 1:
                            report.error(sat + ".escalate.when_confidence_below", "must be a number between 0 and 1")
                        if esc.get("to_tier") not in TIERS:
                            report.error(sat + ".escalate.to_tier", "must be deep, standard or cheap")
                        props = schema.get("properties") if isinstance(schema, dict) else None
                        if not isinstance(props, dict) or "confidence" not in props:
                            report.error(sat + ".escalate", "requires 'confidence' in output_schema.properties")
                if "data_classes" in step and (not isinstance(step["data_classes"], list) or not all(isinstance(d, str) for d in step["data_classes"])):
                    report.error(sat + ".data_classes", "must be a list of strings")

            elif kind == "tool":
                tid = step.get("tool")
                writes = False
                if not isinstance(tid, str) or tid not in tools_decl:
                    report.error(sat + ".tool", f"references an undeclared tool {tid!r}")
                else:
                    writes = tools_decl[tid]
                args = step.get("args")
                if not isinstance(args, dict):
                    report.error(sat + ".args", "must be an object")
                else:
                    check_templated(args, sat + ".args", report, **ctx)
                key = step.get("idempotency_key")
                if writes and key is None:
                    report.error(sat + ".idempotency_key", f"required because {tid} writes")
                if key is not None:
                    if not isinstance(key, str) or not key.strip():
                        report.error(sat + ".idempotency_key", "must be a non-empty templated string")
                    else:
                        check_templated(key, sat + ".idempotency_key", report, **ctx)
                if "scope" in step and not isinstance(step["scope"], str):
                    report.error(sat + ".scope", "must be a string")
                comp = step.get("compensation")
                if comp is not None:
                    if not isinstance(comp, dict):
                        report.error(sat + ".compensation", "must be an object with tool and args")
                    else:
                        ctool = comp.get("tool")
                        if not isinstance(ctool, str) or ctool not in tools_decl:
                            report.error(sat + ".compensation.tool", f"references an undeclared tool {ctool!r}")
                        cargs = comp.get("args")
                        if not isinstance(cargs, dict):
                            report.error(sat + ".compensation.args", "must be an object")
                        else:
                            own_ctx = dict(ctx, allow_own=True)
                            check_templated(cargs, sat + ".compensation.args", report, **own_ctx)

            elif kind == "action":
                action = step.get("action")
                if not isinstance(action, dict):
                    report.error(sat + ".action", "must be an object")
                else:
                    if not isinstance(action.get("kind"), str) or not action["kind"]:
                        report.error(sat + ".action.kind", "must be a non-empty string")
                    if "writes_to_system_of_record" in action and not isinstance(action["writes_to_system_of_record"], bool):
                        report.error(sat + ".action.writes_to_system_of_record", "must be a boolean")
                    check_templated(action, sat + ".action", report, **ctx)

            if sid:
                earlier.add(sid)

    size = len(canonical_bytes(bundle))
    if size > MAX_CANONICAL_BYTES:
        report.error("$", f"canonical size {size} bytes exceeds 1 MiB")
    stats["canonical_bytes"] = size
    return stats


def main(argv: list[str]) -> int:
    path = Path(argv[1]) if len(argv) > 1 else DEFAULT_BUNDLE
    if len(argv) > 2:
        print(__doc__)
        return 2
    try:
        bundle = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        print(f"validate_bundle: {path} not found")
        return 2
    except json.JSONDecodeError as e:
        print(f"validate_bundle: {path} is not valid JSON: {e}")
        return 1
    report = Report()
    stats = validate(bundle, report)
    for at, msg in report.problems:
        print(f"ERROR {at}: {msg}")
    if report.ok:
        print(
            f"OK {path}: {bundle.get('name')}@{bundle.get('version')}, "
            f"{stats['workflows']} workflows, {stats['steps']} steps, "
            f"{stats.get('canonical_bytes', 0)} canonical bytes"
        )
        return 0
    print(f"FAIL {path}: {len(report.problems)} problem(s)")
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
