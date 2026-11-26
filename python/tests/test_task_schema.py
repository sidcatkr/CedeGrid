"""003 FR-060: `command_task` takes exactly the task schema's fields, by their Python names."""
import inspect
import json
import unittest
from pathlib import Path

from cedegrid.client import command_task

FIXTURE = Path(__file__).resolve().parents[2] / "tests" / "contracts" / "task-schema.json"


class TaskSchemaTests(unittest.TestCase):
    def test_command_task_takes_every_schema_field(self):
        fields = [field["name"] for field in json.loads(FIXTURE.read_text())["fields"]]
        parameters = list(inspect.signature(command_task).parameters)
        self.assertEqual(sorted(parameters), sorted(fields))


if __name__ == "__main__":
    unittest.main()
