"""Kernos Python SDK: worker, clients, model router, providers and evaluation.

The contracts this package implements are the Python SDK reference at https://rhs2.github.io/kernos/reference/python-sdk/
and the kernel, bundle and gateway specifications it refers to.
"""

from __future__ import annotations

from kernos import providers
from kernos.client import GatewayClient, KernelClient, KernosError, KernosNetworkError
from kernos.eval import gate, run_golden
from kernos.router import ModelRouter
from kernos.worker import Worker, WorkerConfig

__version__ = "0.1.0"

__all__ = [
    "GatewayClient",
    "KernelClient",
    "KernosError",
    "KernosNetworkError",
    "ModelRouter",
    "Worker",
    "WorkerConfig",
    "__version__",
    "gate",
    "providers",
    "run_golden",
]
