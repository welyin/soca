import unittest

from topology import (LEAF_PROFILES, ModelReservation, PlannerPolicy, ResourceEnvelope,
                      ScaleGate, candidate_plan, plan_topology)


class TopologyTests(unittest.TestCase):
    def test_same_demand_scales_all_topology_levels_with_hardware(self):
        model = ModelReservation()
        settings = [(2048, 2, 16), (4096, 4, 64), (8192, 8, 128), (16384, 16, 256), (32768, 32, 512)]
        for memory, cpu, expected in settings:
            plan = plan_topology(ResourceEnvelope(memory, cpu), model, 512)
            self.assertEqual(plan.leaves, expected)
            self.assertEqual(plan.clusters, expected // 8)
            self.assertEqual(plan.coordinators, expected // 64 if expected > 128 else 0)
            self.assertLessEqual(plan.ram_required_mib, memory)
            self.assertLessEqual(plan.workers, cpu)

    def test_exact_fit_is_admitted_but_one_less_mib_is_not(self):
        model, policy = ModelReservation(), PlannerPolicy()
        required = candidate_plan(8, ResourceEnvelope(65536, 32), model, policy).ram_required_mib
        self.assertEqual(plan_topology(ResourceEnvelope(required, 1), model, 8).state, "RUNNING")
        self.assertEqual(plan_topology(ResourceEnvelope(required - 1, 1), model, 8).state, "PAUSED")

    def test_no_task_demand_does_not_fill_available_hardware(self):
        plan = plan_topology(ResourceEnvelope(65536, 64), ModelReservation(), 0)
        self.assertEqual((plan.leaves, plan.clusters, plan.subjects, plan.logical_units), (8, 1, 1, 10))

    def test_cpu_bottleneck_limits_topology_even_with_abundant_ram(self):
        self.assertEqual(plan_topology(ResourceEnvelope(65536, 1), ModelReservation(), 512).leaves, 16)

    def test_selected_gpu_model_cannot_silently_fall_back_to_cloud(self):
        model = ModelReservation(vram_mib=8192, backend="gpu")
        plan = plan_topology(ResourceEnvelope(32768, 16, 4096), model, 128)
        self.assertEqual((plan.state, plan.reason), ("PAUSED", "MODEL_VRAM_UNAVAILABLE"))

    def test_unauthorized_cloud_and_stale_telemetry_pause(self):
        remote = ModelReservation(backend="remote")
        self.assertEqual(plan_topology(ResourceEnvelope(8192, 8), remote, 64).reason, "REMOTE_NOT_AUTHORIZED")
        stale = ResourceEnvelope(8192, 8, telemetry_age_seconds=4.)
        self.assertEqual(plan_topology(stale, ModelReservation(), 64).reason, "STALE_TELEMETRY")

    def test_no_resources_never_creates_workers_or_discards_identity(self):
        plan = plan_topology(ResourceEnvelope(0, 0), ModelReservation(), 64)
        self.assertEqual((plan.subjects, plan.workers, plan.leaves), (1, 0, 0))
        self.assertEqual(plan.reason, "CONTROL_RESERVE_UNAVAILABLE")
        self.assertGreater(plan.ram_required_mib, plan.ram_limit_mib)

    def test_parent_fanout_and_totals_are_consistent(self):
        for leaves in LEAF_PROFILES:
            plan = candidate_plan(leaves, ResourceEnvelope(65536, 64), ModelReservation(), PlannerPolicy())
            self.assertLessEqual(leaves / plan.clusters, 8)
            if plan.coordinators:
                self.assertLessEqual(plan.clusters / plan.coordinators, 8)
                self.assertLessEqual(plan.coordinators, 16)
            else:
                self.assertLessEqual(plan.clusters, 16)
            self.assertEqual(plan.logical_units, plan.leaves + plan.clusters + plan.coordinators + 1)

    def test_model_calls_and_manual_cap_are_independent_from_leaf_count(self):
        plan = plan_topology(ResourceEnvelope(65536, 64), ModelReservation(max_parallel_calls=1), 512,
                             PlannerPolicy(max_leaves=64))
        self.assertEqual((plan.leaves, plan.llm_parallel_calls), (64, 1))

    def test_expansion_waits_and_changes_at_most_one_profile(self):
        gate = ScaleGate()
        desired = plan_topology(ResourceEnvelope(65536, 64), ModelReservation(), 256)
        self.assertEqual(gate.observe(32, desired, 0.).action, "KEEP")
        self.assertEqual(gate.observe(32, desired, 59.).action, "KEEP")
        decision = gate.observe(32, desired, 60.)
        self.assertEqual((decision.action, decision.target_leaves), ("PROPOSE_TRANSACTION", 64))
        gate.acknowledge(61.)
        gate.observe(64, desired, 62.)
        self.assertEqual(gate.observe(64, desired, 122.).action, "KEEP")
        self.assertEqual(gate.observe(64, desired, 182.).target_leaves, 128)

    def test_sustained_reduction_can_skip_profiles_but_emergency_is_immediate(self):
        gate = ScaleGate()
        desired = plan_topology(ResourceEnvelope(2048, 2), ModelReservation(), 512)
        gate.observe(256, desired, 0.)
        self.assertEqual(gate.observe(256, desired, 10.).target_leaves, 16)
        emergency = gate.observe(256, desired, 11., emergency=True)
        self.assertEqual(emergency.action, "FREEZE_AND_RECONCILE")

    def test_alternating_targets_reset_hold_time(self):
        gate = ScaleGate()
        low = plan_topology(ResourceEnvelope(4096, 4), ModelReservation(), 64)
        high = plan_topology(ResourceEnvelope(8192, 8), ModelReservation(), 128)
        for now, desired in ((0., high), (30., low), (60., high), (90., low)):
            self.assertEqual(gate.observe(32, desired, now).action, "KEEP")

    def test_invalid_inputs_are_rejected(self):
        with self.assertRaises(ValueError):
            ResourceEnvelope(-1, 4)
        with self.assertRaises(ValueError):
            ModelReservation(backend="cpu", vram_mib=1)
        with self.assertRaises(ValueError):
            plan_topology(ResourceEnvelope(4096, 4), ModelReservation(), -1)
        gate = ScaleGate()
        desired = plan_topology(ResourceEnvelope(4096, 4), ModelReservation(), 64)
        gate.observe(32, desired, 2.)
        with self.assertRaises(ValueError):
            gate.observe(32, desired, 1.)


if __name__ == "__main__":
    unittest.main(verbosity=2)