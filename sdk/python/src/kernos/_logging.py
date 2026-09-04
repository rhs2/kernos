"""Logging helpers: every record carries ``run_id``, ``step`` and ``lease_id`` when known."""

from __future__ import annotations

import json
import logging
from collections.abc import Mapping, MutableMapping
from typing import Any

__all__ = ["ContextFilter", "JsonFormatter", "StepLogger", "configure_logging"]

CONTEXT_FIELDS = ("run_id", "step", "lease_id", "worker_id")
TEXT_FORMAT = (
    "%(asctime)s %(levelname)s %(name)s run_id=%(run_id)s step=%(step)s "
    "lease_id=%(lease_id)s %(message)s"
)


class ContextFilter(logging.Filter):
    """Give every record the context attributes so formatters never fail."""

    def filter(self, record: logging.LogRecord) -> bool:
        for field in CONTEXT_FIELDS:
            if not hasattr(record, field):
                setattr(record, field, "-")
        return True


class JsonFormatter(logging.Formatter):
    """One JSON object per line, for ``KERNOS_LOG=json``."""

    def format(self, record: logging.LogRecord) -> str:
        payload: dict[str, Any] = {
            "ts": self.formatTime(record, "%Y-%m-%dT%H:%M:%S"),
            "level": record.levelname,
            "logger": record.name,
            "message": record.getMessage(),
        }
        for field in CONTEXT_FIELDS:
            value = getattr(record, field, "-")
            if value != "-":
                payload[field] = value
        if record.exc_info:
            payload["exception"] = self.formatException(record.exc_info)
        return json.dumps(payload, ensure_ascii=False)


class StepLogger(logging.LoggerAdapter):  # type: ignore[type-arg]
    """A logger adapter that stamps the lease context on every record."""

    def process(
        self, msg: Any, kwargs: MutableMapping[str, Any]
    ) -> tuple[Any, MutableMapping[str, Any]]:
        extra = dict(self.extra or {})
        extra.update(kwargs.get("extra") or {})
        kwargs["extra"] = extra
        return msg, kwargs


def step_logger(logger: logging.Logger, context: Mapping[str, Any]) -> StepLogger:
    """Bind ``run_id``, ``step``, ``lease_id`` and ``worker_id`` onto ``logger``."""
    return StepLogger(logger, {k: v for k, v in context.items() if k in CONTEXT_FIELDS})


def configure_logging(fmt: str = "text", level: int = logging.INFO) -> None:
    """Configure the root logger once for a command-line entry point."""
    handler = logging.StreamHandler()
    handler.addFilter(ContextFilter())
    handler.setFormatter(JsonFormatter() if fmt == "json" else logging.Formatter(TEXT_FORMAT))
    root = logging.getLogger()
    root.handlers[:] = [handler]
    root.setLevel(level)
