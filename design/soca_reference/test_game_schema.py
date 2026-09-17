"""Structural schema checks, not a replacement for a JSON Schema validator."""

import json
from pathlib import Path
import unittest


def objects(value):
    if isinstance(value, dict):
        yield value
        for child in value.values():
            yield from objects(child)
    elif isinstance(value, list):
        for child in value:
            yield from objects(child)


class GameSchemaStructureTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.schema = json.loads(Path(__file__).with_name("game-protocol.schema.json").read_text(encoding="utf-8"))

    def test_draft_and_all_local_references_resolve(self):
        self.assertIn("2020-12", self.schema["$schema"])
        references = [node["$ref"] for node in objects(self.schema) if "$ref" in node]
        self.assertGreater(len(references), 10)
        for reference in references:
            self.assertTrue(reference.startswith("#/"))
            target = self.schema
            for part in reference[2:].split("/"):
                target = target[part.replace("~1", "/").replace("~0", "~")]
            self.assertIsInstance(target, dict)

    def test_every_message_object_rejects_extra_fields(self):
        shapes = [node for node in objects(self.schema) if node.get("type") == "object"]
        self.assertGreater(len(shapes), 5)
        for shape in shapes:
            self.assertIs(shape.get("additionalProperties"), False)
            self.assertTrue(set(shape.get("required", [])) <= set(shape["properties"]))

    def test_hidden_world_fields_are_not_part_of_public_shapes(self):
        fields = set().union(*(set(node["properties"]) for node in objects(self.schema) if "properties" in node))
        self.assertFalse(fields & {"info", "seed", "rng_state", "mine_positions", "safe_cells", "full_map", "solution"})

    def test_actions_require_versioned_permit_and_exclude_debug_commands(self):
        action = self.schema["$defs"]["ActionRequest"]
        self.assertTrue({"permit_id", "topology_epoch", "expected_observation_id", "request_id"} <= set(action["required"]))
        operations = set()
        for variant in action["properties"]["action"]["oneOf"]:
            operation = variant["properties"]["op"]
            operations.update(operation.get("enum", [operation.get("const")]))
        self.assertEqual(operations, {"turn_left", "turn_right", "forward", "pickup", "toggle", "reveal", "chord", "set_flag"})

    def test_pixel_view_cannot_also_contain_symbolic_board(self):
        pixel = self.schema["$defs"]["PixelView"]
        self.assertNotIn("board", pixel["properties"])
        self.assertNotIn("view", pixel["properties"])
        self.assertEqual(pixel["properties"]["mode"]["const"], "pixels")
        self.assertEqual(len(self.schema["$defs"]["Observation"]["allOf"]), 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)