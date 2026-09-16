"""Smoke-test an installed package outside its checkout, with an empty environment."""
import argparse
import json
from pathlib import Path
import plistlib
import subprocess
import sys
import tempfile
import time
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from tekesmemory.client import Client
from tekesmemory.common import canonical

parser = argparse.ArgumentParser()
parser.add_argument('--python', required=True)
python = parser.parse_args().python
with tempfile.TemporaryDirectory() as directory:
    root = Path(directory)
    threads = root / 'threads'
    threads.mkdir()
    prefix = [python, '-m', 'tekesmemory']
    setup = subprocess.run(prefix + ['init', '--directory', str(root / 'config'), '--workspace', 'ws', '--thread-root', str(threads)],
                           check=True, capture_output=True, text=True, cwd=root, env={})
    paths = json.loads(setup.stdout)
    config_path = Path(paths['service_config'])
    config = json.loads(config_path.read_text())
    config['port'] = 0
    config_path.write_text(canonical(config))
    ready = root / 'ready.json'
    process = subprocess.Popen(prefix + ['serve', '--config', str(config_path), '--ready-file', str(ready)],
                               cwd=root, env={}, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    try:
        for _ in range(100):
            if ready.exists(): break
            if process.poll() is not None: raise AssertionError(process.stderr.read().decode())
            time.sleep(.02)
        url = json.loads(ready.read_text())['endpoint']
        adapter_path = Path(paths['adapter_config'])
        adapter = json.loads(adapter_path.read_text())
        adapter['endpoint'] = url
        adapter_path.write_text(canonical(adapter))
        token = json.loads((root / 'config/model-credential.json').read_text())['token']
        with Client(url, token, 3) as client:
            client.call('memory.save', {'schema_version': 1, 'scope': config['principals'][0]['scopes'][0],
                'kind': 'episode', 'content': 'package smoke marker', 'sources': [{'host': 'test', 'ref': 'package', 'digest': 'a'*64}], 'idempotency_key': 'package-smoke'})
        binding = json.loads((root / 'config/hooks/tekes-memory-context-prepare.json').read_text())
        request = {'format': 2, 'hook_id': binding['id'], 'event_id': 'package', 'event': 'context.prepare',
                   'workspace_id': 'ws', 'thread_id': 'thread', 'turn_id': 1,
                   'data': {'source': {'ledger_path': str(threads / 'unused.jsonl')}, 'payload': {'items': [{'role': 'user', 'content': 'package smoke'}], 'through_seq': 1}}}
        result = subprocess.run(binding['argv'], input=(canonical(request)+'\n').encode(), capture_output=True, check=True, cwd=root, env={}, timeout=3)
        assert 'package smoke marker' in result.stdout.decode()
        output = root / 'service.plist'
        subprocess.run(prefix + ['launchd-plist', '--config', str(config_path), '--output', str(output)], cwd=root, env={}, check=True)
        assert plistlib.loads(output.read_bytes())['ProgramArguments'][1:3] == ['-m', 'tekesmemory']
        print('installed package: isolated init/serve/real hook/launchd generation passed')
    finally:
        process.terminate()
        process.wait(timeout=5)
        stderr = process.stderr.read().decode()
        process.stderr.close()
        assert 'Traceback' not in stderr
