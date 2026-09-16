import json
import os
from pathlib import Path
import tempfile
import unittest
from tekesmemory.cli import write_private
from tekesmemory.common import MemoryError, decode, private_json, validate
from tekesmemory.config import SERVICE, ADAPTER
from tekesmemory.schema import SCHEMAS


class Contracts(unittest.TestCase):
    def test_exported_schemas_match_live_catalog(self):
        directory = Path(__file__).resolve().parents[1] / 'schemas'
        for name, schema in (SCHEMAS | {'service-config': SERVICE, 'adapter-config': ADAPTER}).items():
            expected = {'$schema': 'https://json-schema.org/draft/2020-12/schema', **schema}
            self.assertEqual(json.loads((directory / (name + '.schema.json')).read_text()), expected)

    def test_private_atomic_publication_and_no_overwrite(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'private.json'
            write_private(path, b'{"safe":true}\n')
            self.assertEqual(private_json(path), {'safe': True})
            with self.assertRaises(FileExistsError): write_private(path, b'{}')
            self.assertEqual(private_json(path), {'safe': True})
            os.chmod(path, 0o644)
            with self.assertRaises(MemoryError): private_json(path)

    def test_wire_rejects_duplicate_keys_nan_and_invalid_utf8(self):
        for data in [b'{"key":1,"key":2}', b'{"key":NaN}', b'{"key":"\xff"}']:
            with self.assertRaises(MemoryError): decode(data)

    def test_boolean_is_not_schema_version(self):
        with self.assertRaises(MemoryError):
            validate({'schema_version': True, 'operation_id': 'a'}, SCHEMAS['memory.status'])
