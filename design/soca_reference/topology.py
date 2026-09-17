"""Executable specification for one subject's elastic topology, not a runtime.

Resource envelopes are supplied by the OS adapter. No devices, services,
model downloads, or persistent actors are created by this module.
"""

import argparse
from dataclasses import asdict, dataclass
import json
import math


LEAF_PROFILES = (8, 16, 32, 64, 128, 256, 512)


@dataclass(frozen=True)
class ResourceEnvelope:
    ram_limit_mib: int
    cpu_slots: int
    gpu_allocatable_mib: int = 0
    telemetry_age_seconds: float = 0.

    def __post_init__(self):
        for value in (self.ram_limit_mib, self.cpu_slots, self.gpu_allocatable_mib):
            if not isinstance(value, int) or isinstance(value, bool) or value < 0:
                raise ValueError("Resource amounts must be nonnegative integers.")
        if not math.isfinite(self.telemetry_age_seconds) or self.telemetry_age_seconds < 0:
            raise ValueError("Telemetry age must be finite and nonnegative.")


@dataclass(frozen=True)
class ModelReservation:
    ram_mib: int = 1024
    vram_mib: int = 0
    max_parallel_calls: int = 1
    backend: str = "cpu"
    remote_authorized: bool = False

    def __post_init__(self):
        for value in (self.ram_mib, self.vram_mib, self.max_parallel_calls):
            if not isinstance(value, int) or isinstance(value, bool) or value < 0:
                raise ValueError("Model reservations must be nonnegative integers.")
        if self.max_parallel_calls < 1 or self.backend not in ("cpu", "gpu", "remote"):
            raise ValueError("Use a known backend and at least one allowed model call.")
        if self.backend != "gpu" and self.vram_mib:
            raise ValueError("VRAM is only reserved for a GPU backend.")


@dataclass(frozen=True)
class PlannerPolicy:
    max_leaves: int = 512
    max_workers: int = 32
    core_mib: int = 256
    cache_mib: int = 256
    controller_mib: int = 2
    catalog_kib_per_leaf: int = 64
    hot_state_mib_per_leaf: int = 16
    worker_peak_mib: int = 256
    max_telemetry_age_seconds: float = 3.

    def __post_init__(self):
        if self.max_leaves not in LEAF_PROFILES or self.max_workers < 1:
            raise ValueError("Use a supported leaf cap and positive worker cap.")
        for name in ("core_mib", "cache_mib", "controller_mib", "catalog_kib_per_leaf", "hot_state_mib_per_leaf", "worker_peak_mib"):
            value = getattr(self, name)
            if not isinstance(value, int) or isinstance(value, bool) or value < 0:
                raise ValueError("Cost estimates must be nonnegative integer amounts.")
        if not math.isfinite(self.max_telemetry_age_seconds) or self.max_telemetry_age_seconds <= 0:
            raise ValueError("Telemetry freshness must have a finite positive bound.")


@dataclass(frozen=True)
class TopologyPlan:
    state: str
    leaves: int
    clusters: int
    coordinators: int
    subjects: int
    hot_leaves: int
    workers: int
    llm_parallel_calls: int
    ram_required_mib: int
    ram_limit_mib: int
    reason: str

    @property
    def logical_units(self):
        return self.leaves + self.clusters + self.coordinators + self.subjects

    @property
    def composition_depth(self):
        return 0 if self.state != "RUNNING" else 3 + int(self.coordinators > 0)


def candidate_plan(leaves, envelope, model, policy):
    if leaves not in LEAF_PROFILES:
        raise ValueError("Unsupported topology profile.")
    clusters = math.ceil(leaves / 8)
    coordinators = math.ceil(clusters / 8) if clusters > 16 else 0
    hot = leaves // 4
    workers = math.ceil(hot / 4)
    controllers = clusters + coordinators + 1
    required = (policy.core_mib + policy.cache_mib + model.ram_mib
                + math.ceil(leaves * policy.catalog_kib_per_leaf / 1024)
                + hot * policy.hot_state_mib_per_leaf
                + controllers * policy.controller_mib + workers * policy.worker_peak_mib)
    return TopologyPlan("RUNNING", leaves, clusters, coordinators, 1, hot, workers,
                        min(workers, model.max_parallel_calls), required, envelope.ram_limit_mib, "ADMITTED")


def plan_topology(envelope, model, desired_leaf_slots, policy=PlannerPolicy()):
    if not isinstance(desired_leaf_slots, int) or isinstance(desired_leaf_slots, bool) or desired_leaf_slots < 0:
        raise ValueError("Demand is a nonnegative admitted logical-slot count.")

    def paused(reason):
        return TopologyPlan("PAUSED", 0, 0, 0, 1, 0, 0, 0, policy.core_mib,
                            envelope.ram_limit_mib, reason)

    if envelope.ram_limit_mib < policy.core_mib:
        return paused("CONTROL_RESERVE_UNAVAILABLE")
    if envelope.telemetry_age_seconds > policy.max_telemetry_age_seconds:
        return paused("STALE_TELEMETRY")
    if model.backend == "remote" and not model.remote_authorized:
        return paused("REMOTE_NOT_AUTHORIZED")
    if model.vram_mib > envelope.gpu_allocatable_mib:
        return paused("MODEL_VRAM_UNAVAILABLE")
    requested = min(max(8, desired_leaf_slots), policy.max_leaves)
    ceiling = next(profile for profile in LEAF_PROFILES if profile >= requested)
    feasible = []
    for leaves in LEAF_PROFILES:
        if leaves > ceiling or leaves > policy.max_leaves:
            break
        candidate = candidate_plan(leaves, envelope, model, policy)
        if (candidate.ram_required_mib <= envelope.ram_limit_mib
                and candidate.workers <= min(envelope.cpu_slots, policy.max_workers)):
            feasible.append(candidate)
    return feasible[-1] if feasible else paused("MINIMUM_TOPOLOGY_UNAVAILABLE")


@dataclass(frozen=True)
class ScaleDecision:
    action: str
    target_leaves: int
    reason: str


class ScaleGate:
    """Recommendation gate; only an acknowledged transaction changes topology."""

    def __init__(self, upscale_hold=60., downscale_hold=10., cooldown=120.):
        if any(not math.isfinite(value) or value < 0 for value in (upscale_hold, downscale_hold, cooldown)):
            raise ValueError("Hysteresis durations must be finite and nonnegative.")
        self.upscale_hold = upscale_hold
        self.downscale_hold = downscale_hold
        self.cooldown = cooldown
        self.pending_target = None
        self.pending_since = None
        self.last_applied = -math.inf
        self.last_observed = -math.inf

    def observe(self, current_leaves, desired, now, emergency=False):
        if current_leaves not in (0, *LEAF_PROFILES) or not math.isfinite(now) or now < self.last_observed:
            raise ValueError("Use a valid current topology and monotonic observation time.")
        self.last_observed = now
        target = desired.leaves
        if emergency or desired.state != "RUNNING":
            self.pending_target, self.pending_since = None, None
            return ScaleDecision("FREEZE_AND_RECONCILE", target, desired.reason if not emergency else "PRESSURE_EMERGENCY")
        if target == current_leaves:
            self.pending_target, self.pending_since = None, None
            return ScaleDecision("KEEP", current_leaves, "AT_TARGET")
        if target != self.pending_target:
            self.pending_target, self.pending_since = target, now
        growing = target > current_leaves
        required_hold = self.upscale_hold if growing else self.downscale_hold
        if now - self.pending_since < required_hold or (growing and now - self.last_applied < self.cooldown):
            return ScaleDecision("KEEP", current_leaves, "HYSTERESIS")
        if growing:
            target = min(target, 8 if current_leaves == 0 else current_leaves * 2)
        return ScaleDecision("PROPOSE_TRANSACTION", target, "STABLE_GROWTH" if growing else "STABLE_REDUCTION")

    def acknowledge(self, now):
        if not math.isfinite(now) or now < self.last_observed or now < self.last_applied:
            raise ValueError("Acknowledgements must use monotonic time.")
        self.last_applied = now
        self.pending_target, self.pending_since = None, None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ram-mib", type=int, required=True, help="Normalized absolute SoCA RAM allowance, not total hardware RAM")
    parser.add_argument("--cpu-slots", type=int, required=True)
    parser.add_argument("--gpu-mib", type=int, default=0)
    parser.add_argument("--model-ram-mib", type=int, default=1024)
    parser.add_argument("--model-vram-mib", type=int, default=0)
    parser.add_argument("--backend", choices=("cpu", "gpu", "remote"), default="cpu")
    parser.add_argument("--remote-authorized", action="store_true", help="Simulation input only; this program makes no network calls")
    parser.add_argument("--demand", type=int, default=64)
    parser.add_argument("--max-leaves", type=int, choices=LEAF_PROFILES, default=512)
    arguments = parser.parse_args()
    envelope = ResourceEnvelope(arguments.ram_mib, arguments.cpu_slots, arguments.gpu_mib)
    reservation = ModelReservation(ram_mib=arguments.model_ram_mib, vram_mib=arguments.model_vram_mib,
                                   backend=arguments.backend, remote_authorized=arguments.remote_authorized)
    plan = plan_topology(envelope, reservation, arguments.demand, PlannerPolicy(max_leaves=arguments.max_leaves))
    print(json.dumps(dict(asdict(plan), logical_units=plan.logical_units,
                          composition_depth=plan.composition_depth, reference_simulation_only=True), indent=2))


if __name__ == "__main__":
    main()