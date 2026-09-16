"""Run official SDK interoperability in a temporary, isolated service instance."""
import os
from pathlib import Path
import subprocess
from test_process import ProcessTests, ROOT

fixture = ProcessTests()
fixture.setUp()
try:
    completed = subprocess.run([os.environ.get('NODE', 'node'), str(ROOT / 'tests/official_client.mjs'),
                               fixture.url, str(fixture.root / 'config/model-credential.json'), fixture.owner],
                              check=True, timeout=30)
finally:
    fixture.tearDown()
