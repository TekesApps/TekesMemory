"""Controlled fixture for Kernel's opt-in real Memory regression; stdout is control only."""
import json
import os
from pathlib import Path
import sys
import threading
import time
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from tekesmemory.cli import setup
from tekesmemory.common import canonical, private_json
from tekesmemory.server import Server

os.umask(0o077)
root = Path(sys.argv[1])
paths = setup(root / 'real-memory', 'ws', root)
config_path = Path(paths['service_config'])
config = private_json(config_path)
config['port'] = 0
config_path.write_text(canonical(config))
server = Server(config_path)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
adapter_path = Path(paths['adapter_config'])
adapter = private_json(adapter_path)
adapter['endpoint'] = f'http://127.0.0.1:{server.server_port}/mcp'
adapter['retrieval']['estimated_tokens'] = 8192
adapter_path.write_text(canonical(adapter))
principal = config['principals'][0]
scope = principal['scopes'][0]
server.store.call(principal, 'memory.save', {'schema_version': 1, 'scope': scope, 'kind': 'episode',
    'content': 'first second LIFECYCLE_CONTEXT_MARKER', 'sources': [{'host': 'fixture', 'ref': 'seed', 'digest': 'a'*64}], 'idempotency_key': 'seed'})
print(canonical({'hook_root': str(config_path.parent)}), flush=True)
try:
    line = sys.stdin.readline()
    if line.strip() == 'verify':
        for _ in range(100):
            with server.store.lock:
                records = [json.loads(r[0]) for r in server.store.db.execute("SELECT payload FROM observations WHERE state='completed'")]
            if any(r.get('type') == 'turn_outcome' for r in records): break
            time.sleep(.02)
        assert sum(r.get('type') == 'turn_outcome' for r in records) == 1
        assert sum(r.get('type') == 'tool_result' for r in records) == 1
        assert next(r for r in records if r.get('type') == 'tool_result')['payload']['outcome'] == 'ok'
        assert 'Still thinking' not in canonical(records)
        print(canonical({'verified': True, 'types': [r['type'] for r in records]}), flush=True)
finally:
    server.shutdown()
    thread.join(timeout=3)
    server.server_close()
