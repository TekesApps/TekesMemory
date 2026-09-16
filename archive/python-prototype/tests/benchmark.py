"""Reproducible local warm retrieval benchmark; does not measure model quality."""
from pathlib import Path
import json
import statistics
import sys
import tempfile
import time
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from tekesmemory.store import Store
from test_store import SCOPE, SOURCE, principal, save_args, search_args

with tempfile.TemporaryDirectory() as directory:
    store = Store(Path(directory) / 'memory.sqlite3')
    start = time.perf_counter()
    # Bulk fixture insertion isolates query performance from per-operation fsync latency.
    store.db.execute('BEGIN IMMEDIATE')
    for i in range(10000):
        store._save(save_args(str(i), content=f'项目测试 project tests historical episode {i}'))
    for i in range(5000):
        store._save(save_args('f'+str(i), kind='fact', content=f'项目测试 project test fact {i}',
                             fact={'subject': f'project-{i}', 'predicate': 'runner', 'value': 'unittest'}))
    store.db.execute('COMMIT')
    load_seconds = time.perf_counter() - start
    timings = []
    for i in range(40):
        start = time.perf_counter()
        result = store.call(principal(), 'memory.search', search_args('项目测试 project tests'))
        timings.append((time.perf_counter() - start) * 1000)
        assert result['items']
    ordered = sorted(timings)
    print(json.dumps({'episodes': 10000, 'facts': 5000, 'queries': 40, 'fixture_load_seconds': round(load_seconds, 3),
                      'warm_p50_ms': round(statistics.median(timings), 3), 'warm_p95_ms': round(ordered[37], 3),
                      'database_bytes': sum(p.stat().st_size for p in Path(directory).glob('memory.sqlite3*'))}, indent=2))
    store.close()
