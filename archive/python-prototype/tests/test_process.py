import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.error
import urllib.request

from tekesmemory.cli import setup
from tekesmemory.client import Client
from tekesmemory.common import PROTOCOL, canonical, digest, private_json
from tekesmemory.schema import SCHEMAS

ROOT = Path(__file__).resolve().parents[1]


class ProcessTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.threads = self.root / 'threads'
        self.threads.mkdir()
        paths = setup(self.root / 'config', 'ws', self.threads)
        self.config = Path(paths['service_config'])
        config = private_json(self.config)
        config['port'] = 0
        self.config.write_text(canonical(config))
        self.adapter = Path(paths['adapter_config'])
        self.start()
        self.owner = config['principals'][0]['owner_id']
        self.scope = {'kind': 'workspace', 'owner_id': self.owner, 'workspace_id': 'ws'}
        self.model_token = private_json(self.root / 'config/model-credential.json')['token']
        self.adapter_token = private_json(self.root / 'config/adapter-credential.json')['token']
        self.ledger = self.threads / 't/main.jsonl'
        self.ledger.parent.mkdir()
        self.records = [
            {'kind': 'genesis', 'seq': 1, 'workspace': 'ws', 'thread': 't'},
            {'kind': 'input', 'seq': 2, 'content': [{'type': 'text', 'text': 'How to run project tests'}]},
            {'kind': 'turn_open', 'seq': 3, 'turn': 1, 'trigger': {'inputs': [2]}},
            {'kind': 'tool_call', 'seq': 4, 'turn': 1, 'call': 'c', 'name': 'shell'},
            {'kind': 'tool_result', 'seq': 5, 'turn': 1, 'call': 'c', 'status': 'ok'},
            {'kind': 'output', 'seq': 6, 'turn': 1, 'content': [{'type': 'text', 'text': 'Project tests passed'}, {'type': 'thinking', 'thinking': 'HIDDEN_REASONING'}]},
            {'kind': 'settle', 'seq': 7, 'turn': 1, 'outcome': 'completed'}]
        self.ledger.write_text(''.join(canonical(r) + '\n' for r in self.records))

    def start(self):
        ready = self.root / 'ready.json'
        ready.unlink(missing_ok=True)
        self.proc = subprocess.Popen([sys.executable, str(ROOT / 'run.py'), 'serve', '--config', str(self.config), '--ready-file', str(ready)],
                                     stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        for _ in range(100):
            if ready.exists(): break
            if self.proc.poll() is not None:
                raise AssertionError(self.proc.stderr.read().decode())
            time.sleep(.03)
        else: raise AssertionError('service startup timeout')
        self.url = json.loads(ready.read_text())['endpoint']
        if hasattr(self, 'adapter'):
            adapter = private_json(self.adapter)
            adapter['endpoint'] = self.url
            self.adapter.write_text(canonical(adapter))

    def tearDown(self):
        self.proc.terminate()
        self.proc.wait(timeout=5)
        error = self.proc.stderr.read().decode()
        self.proc.stderr.close()
        self.tmp.cleanup()
        self.assertNotIn('Traceback', error)

    def call(self, name, args, adapter=False):
        with Client(self.url, self.adapter_token if adapter else self.model_token, 3) as client:
            return client.call(name, {'schema_version': 1, 'scope': self.scope, **args})

    def hook(self, event, payload, *, ledger=None, identity=None):
        request = {'format': 2, 'hook_id': 'memory.json', 'event_id': identity or event, 'event': event,
                   'workspace_id': 'ws', 'thread_id': 't', 'turn_id': 1,
                   'data': {'source': {'ledger_path': str(ledger or self.ledger)}, 'payload': payload}}
        return subprocess.run([sys.executable, str(ROOT / 'run.py'), 'kernel', '--config', str(self.adapter)],
                              input=(canonical(request) + '\n').encode(), capture_output=True, env={}, timeout=3)

    def search(self, query=''):
        return self.call('memory.search', {'query': query, 'kinds': ['episode', 'fact', 'procedure'],
                         'budget': {'max_items': 10, 'max_utf8_bytes': 32768, 'estimated_tokens': 8192}})

    def test_http_mcp_and_five_real_hook_processes(self):
        self.call('memory.save', {'kind': 'episode', 'content': 'Project tests: use python -m unittest',
                   'sources': [{'host': 'test', 'ref': 'guide', 'digest': 'a' * 64}], 'idempotency_key': 'save'})
        before = self.hook('turn.before', {'source_seq': 3, 'turn': self.records[2]})
        self.assertEqual(before.returncode, 0, before.stderr)
        context = self.hook('context.prepare', {'through_seq': 3, 'items': [{'role': 'user', 'content': 'project tests'}]})
        self.assertEqual(context.returncode, 0, context.stderr)
        self.assertIn('python -m unittest', json.loads(context.stdout)['context'][0])
        for event, payload in [
            ('tool.completed', {'source_seq': 5, 'record': self.records[4]}),
            ('context.before_compact', {'through_seq': 6, 'covers': [2, 3, 4, 5, 6], 'manual': False}),
            ('turn.settled', {'source_seq': 7, 'record': self.records[6]})]:
            for _ in range(2):
                result = self.hook(event, payload)
                self.assertEqual(result.returncode, 0, result.stderr)
        for _ in range(100):
            result = self.search()
            if len(result['items']) == 4: break
            time.sleep(.03)
        self.assertEqual(len(result['items']), 4)
        self.assertNotIn('HIDDEN_REASONING', canonical(result))
        # Actual process restart preserves records and gives the next client the same record ids.
        ids = {r['id'] for r in result['items']}
        self.proc.terminate(); self.proc.wait(timeout=5); self.proc.stderr.close()
        self.start()
        self.assertEqual(ids, {r['id'] for r in self.search()['items']})

    def test_auth_origin_host_and_tool_filter(self):
        with Client(self.url, self.model_token, 3) as client:
            names = [t['name'] for t in client.rpc('tools/list', {})['tools']]
            self.assertNotIn('memory.observe', names)
        request = urllib.request.Request(self.url, data=b'{}', headers={'Authorization': 'Bearer ' + self.model_token, 'Origin': 'https://evil.invalid'})
        with self.assertRaises(urllib.error.HTTPError) as error: urllib.request.urlopen(request)
        self.assertEqual(error.exception.code, 403)
        error.exception.close()
        with self.assertRaises(urllib.error.HTTPError) as error: Client(self.url, 'wrong', 1)
        self.assertEqual(error.exception.code, 401)
        error.exception.close()
        request = urllib.request.Request(self.url, headers={'Authorization': 'Bearer ' + self.model_token})
        with self.assertRaises(urllib.error.HTTPError) as error: urllib.request.urlopen(request)
        self.assertEqual(error.exception.code, 405)
        error.exception.close()

    def test_live_credential_revocation(self):
        with Client(self.url, self.model_token, 3) as client:
            config = private_json(self.config)
            config['principals'] = [p for p in config['principals'] if p['role'] != 'model']
            self.config.write_text(canonical(config))
            with self.assertRaises(urllib.error.HTTPError) as error: client.rpc('tools/list', {})
            self.assertEqual(error.exception.code, 401)
        error.exception.close()

    def test_path_escape_symlink_and_torn_prefix_rejected(self):
        outside = self.root / 'outside.jsonl'
        outside.write_bytes(self.ledger.read_bytes())
        for path in [outside, self.ledger.parent / 'alias.jsonl']:
            if path.name == 'alias.jsonl': path.symlink_to(self.ledger)
            result = self.hook('turn.settled', {'source_seq': 7, 'record': self.records[6]}, ledger=path)
            self.assertNotEqual(result.returncode, 0)
        self.ledger.write_text(canonical(self.records[0]))
        self.assertNotEqual(self.hook('turn.settled', {'source_seq': 7, 'record': self.records[6]}).returncode, 0)

    def test_future_tail_not_imported(self):
        self.records.append({'kind': 'output', 'seq': 8, 'turn': 1, 'content': [{'type': 'text', 'text': 'FUTURE_TEXT'}]})
        self.ledger.write_text(''.join(canonical(r) + '\n' for r in self.records))
        result = self.hook('turn.settled', {'source_seq': 7, 'record': self.records[6]})
        self.assertEqual(result.returncode, 0, result.stderr)
        for _ in range(50):
            result = self.search()
            if result['items']: break
            time.sleep(.03)
        self.assertNotIn('FUTURE_TEXT', canonical(result))

    def test_crash_after_commit_and_offline_hook(self):
        saved = self.call('memory.save', {'kind': 'episode', 'content': 'durable after kill',
                   'sources': [{'host': 'test', 'ref': 'guide', 'digest': 'a' * 64}], 'idempotency_key': 'crash-save'})
        self.proc.kill(); self.proc.wait(timeout=5); self.proc.stderr.close()
        before = time.monotonic()
        failed = self.hook('turn.before', {'source_seq': 3, 'turn': self.records[2]})
        self.assertNotEqual(failed.returncode, 0)
        self.assertLess(time.monotonic() - before, 2)
        self.start()
        self.assertEqual(self.call('memory.get', {'id': saved['id']})['item']['content'], 'durable after kill')

    def test_context_continuation_recovers_admitted_task(self):
        self.call('memory.save', {'kind': 'episode', 'content': 'Project tests: use unittest',
                   'sources': [{'host': 'test', 'ref': 'guide', 'digest': 'a' * 64}], 'idempotency_key': 'continuation'})
        result = self.hook('context.prepare', {'through_seq': 5, 'items': [{'role': 'tool', 'content': []}]})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('use unittest', result.stdout.decode())

    def test_service_single_writer_and_stdio_relay(self):
        other = subprocess.run([sys.executable, str(ROOT / 'run.py'), 'serve', '--config', str(self.config)], capture_output=True, timeout=3)
        self.assertNotEqual(other.returncode, 0)
        messages = [
            {'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {'protocolVersion': PROTOCOL, 'capabilities': {}, 'clientInfo': {'name': 'test', 'version': '1'}}},
            {'jsonrpc': '2.0', 'method': 'notifications/initialized'},
            {'jsonrpc': '2.0', 'id': 2, 'method': 'tools/list'}]
        result = subprocess.run([sys.executable, str(ROOT / 'run.py'), 'stdio', '--endpoint', self.url,
                                 '--credential-file', str(self.root / 'config/model-credential.json')],
                                input=''.join(canonical(m) + '\n' for m in messages).encode(), capture_output=True, timeout=3)
        self.assertEqual(result.returncode, 0, result.stderr)
        replies = [json.loads(line) for line in result.stdout.splitlines()]
        self.assertEqual([r['id'] for r in replies], [1, 2])
        self.assertEqual(len(replies[1]['result']['tools']), 6)


if __name__ == '__main__': unittest.main()
