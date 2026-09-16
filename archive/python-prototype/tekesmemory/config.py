"""Closed deployment configuration. Identities come from this file, never tool args."""
from pathlib import Path
from urllib.parse import urlparse
from .common import MemoryError, authorize, private_json, validate
from .schema import SCHEMAS, SCOPE, BUDGET, array, integer, obj, string

HASH = {'type': 'string', 'pattern': '[a-f0-9]{64}'}
PRINCIPAL = obj({'id': string(), 'role': {'enum': ['model', 'adapter']}, 'owner_id': string(),
                 'scopes': array(SCOPE, 256, 1), 'tools': array({'enum': list(SCHEMAS)}, 7, 1), 'token_sha256': HASH})
SERVICE = obj({'schema_version': {'type': 'integer', 'const': 1}, 'host': {'enum': ['127.0.0.1']},
               'port': integer(0, 65535), 'data_directory': string(4096), 'episode_days': integer(1, 3650),
               'allowed_origins': array(string(4096)), 'principals': array(PRINCIPAL, 256, 1)})
ADAPTER = obj({'schema_version': {'type': 'integer', 'const': 1}, 'endpoint': string(4096),
               'owner_id': string(), 'workspace_id': string(), 'host_instance_id': string(),
               'thread_root': string(4096), 'credential_file': string(4096),
               'request_timeout_ms': integer(1, 4500), 'retrieval': BUDGET})


def service_config(path):
    config = private_json(path)
    validate(config, SERVICE)
    for field in ('id', 'token_sha256'):
        if len({p[field] for p in config['principals']}) != len(config['principals']):
            raise MemoryError('invalid_config')
    for p in config['principals']:
        if p['role'] == 'model' and 'memory.observe' in p['tools']:
            raise MemoryError('invalid_config')
        for scope in p['scopes']:
            authorize(p, scope)
    if not Path(config['data_directory']).is_absolute():
        raise MemoryError('invalid_config')
    for origin in config['allowed_origins']:
        url = urlparse(origin)
        if url.scheme not in ('http', 'https') or url.hostname not in ('localhost', '127.0.0.1') or url.path or url.username or url.query or url.fragment:
            raise MemoryError('invalid_config')
    return config


def adapter_config(path):
    config = private_json(path)
    validate(config, ADAPTER)
    for field in ('thread_root', 'credential_file'):
        if not Path(config[field]).is_absolute():
            raise MemoryError('invalid_config')
    return config
