#!/usr/bin/env python3
"""Pure evidence regressions; these do not execute an application or model."""
import importlib.util
import json
from pathlib import Path
import sqlite3
import unittest

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("whole_process_driver", HERE / "bk-cut-whole-process-crash.py")
driver = importlib.util.module_from_spec(spec)
spec.loader.exec_module(driver)


class EvidenceBinding(unittest.TestCase):
    def setUp(self):
        self.database = sqlite3.connect(":memory:")
        self.database.executescript("""
            CREATE TABLE submissions(id INTEGER PRIMARY KEY, conversation_id INTEGER, record TEXT);
            CREATE TABLE entries(id INTEGER PRIMARY KEY, conversation_id INTEGER, record TEXT);
            CREATE TABLE tasks(id INTEGER PRIMARY KEY, conversation_id INTEGER, record TEXT, kind TEXT);
        """)
        self.original = {"conversationId": 7, "submissionId": 3, "taskId": "original-request"}
        self.audit = {"taskId": 6, "conversationId": 7, "callId": "read-current", "name": "project_get", "outcome": "completed"}
        self.input(3, 2, "original-request")
        self.chain(4, 5, 6, "read-current")

    def tearDown(self):
        self.database.close()

    def record(self, table, value):
        common = (value["id"], value["conversationId"], json.dumps(value))
        if table == "tasks":
            self.database.execute("INSERT INTO tasks VALUES(?,?,?,?)", (*common, json.dumps(value["kind"])))
        else:
            self.database.execute("INSERT INTO " + table + " VALUES(?,?,?)", common)

    def input(self, ident, entry, request):
        self.record("submissions", {"id": ident, "conversationId": 7, "type": "input", "entry": entry,
            "requestId": request, "status": "done"})

    def chain(self, generation, assistant, tool, call):
        state = {"status": "terminal", "outcome": {"status": "completed", "result": {"entryId": assistant}}}
        self.record("tasks", {"id": generation, "conversationId": 7, "kind": "pi.generation", "version": 3, "input": {}, "state": state})
        self.record("entries", {"id": assistant, "conversationId": 7, "kind": "pi.assistant", "byTaskId": generation,
            "model": [{"role": "assistant", "content": [{"type": "toolCall", "id": call, "name": "project_get", "arguments": {}}]}]})
        self.record("tasks", {"id": tool, "conversationId": 7, "kind": "pi.tool", "version": 4, "owner": generation,
            "input": {"kind": "model", "assistant": assistant, "callId": call}, "state": {"status": "terminal", "outcome": {"status": "completed"}}})

    def test_exact_original_tool_binding(self):
        result = driver.bind_native_audit_to_submission(self.database, self.original, [self.audit])
        self.assertEqual(result[0]["submissionId"], 3)
        self.assertEqual(result[0]["generationTaskId"], 4)
        self.assertEqual(result[0]["assistantEntryId"], 5)
        self.assertEqual(result[0]["nativeRequestId"], "original-request")

    def test_same_conversation_and_call_but_another_input_is_rejected(self):
        self.input(12, 11, "another-request")
        self.chain(13, 14, 15, "read-current")
        with self.assertRaisesRegex(AssertionError, "different original input"):
            driver.bind_native_audit_to_submission(self.database, self.original, [{**self.audit, "taskId": 15}])

    def test_mismatched_original_call_is_rejected(self):
        with self.assertRaisesRegex(AssertionError, "original tool invocation"):
            driver.bind_native_audit_to_submission(self.database, self.original, [{**self.audit, "callId": "another-call"}])

    def test_index_and_record_task_identity_must_match(self):
        row = self.database.execute("SELECT record FROM tasks WHERE id=6").fetchone()
        record = json.loads(row[0]); record["id"] = 16
        self.database.execute("UPDATE tasks SET record=? WHERE id=6", (json.dumps(record),))
        with self.assertRaisesRegex(AssertionError, "identity differs"):
            driver.bind_native_audit_to_submission(self.database, self.original, [self.audit])

    def test_generation_must_produce_the_exact_assistant(self):
        row = self.database.execute("SELECT record FROM tasks WHERE id=4").fetchone()
        record = json.loads(row[0]); record["state"]["outcome"]["result"]["entryId"] = 500
        self.database.execute("UPDATE tasks SET record=? WHERE id=4", (json.dumps(record),))
        with self.assertRaisesRegex(AssertionError, "generation did not produce"):
            driver.bind_native_audit_to_submission(self.database, self.original, [self.audit])


if __name__ == "__main__":
    unittest.main(verbosity=2)
