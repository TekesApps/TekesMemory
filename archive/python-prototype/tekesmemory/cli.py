"""Local setup and operational entry points. Setup never enables host hooks implicitly."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import secrets
import signal
import sys
import threading

from .common import MemoryError, canonical, private_json
from .schema import SCHEMAS


def write_private(path, data):
    import tempfile
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd, temporary = tempfile.mkstemp(prefix='.publish-', dir=path.parent)
    try:
        with os.fdopen(fd, 'wb') as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        # Atomic publication without replacing an existing credential/configuration.
        os.link(temporary, path, follow_symlinks=False)
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        os.unlink(temporary)


def invocation():
    launcher = Path(__file__).resolve().parent.parent / "run.py"
    return [sys.executable, str(launcher)] if launcher.is_file() else [sys.executable, "-m", "tekesmemory"]


def setup(directory, workspace, thread_root, port=43187):
    directory = Path(directory).absolute()
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    if directory.is_symlink() or directory.stat().st_mode & 0o077:
        raise MemoryError('unsafe_config_permissions')
    if any(directory.iterdir()):
        raise MemoryError('setup_directory_not_empty')
    root = Path(thread_root).resolve(strict=True)
    owner = 'local-' + str(os.getuid())
    scope = {'kind': 'workspace', 'owner_id': owner, 'workspace_id': workspace}
    principals = []
    for role in ('model', 'adapter'):
        token = secrets.token_urlsafe(32)
        write_private(directory / f'{role}-credential.json', (canonical({'token': token}) + '\n').encode())
        tools = [name for name in SCHEMAS if (name != 'memory.observe' if role == 'model' else name in ('memory.search', 'memory.get', 'memory.observe', 'memory.status'))]
        principals.append({'id': role, 'role': role, 'owner_id': owner, 'scopes': [scope], 'tools': tools,
                           'token_sha256': hashlib.sha256(token.encode()).hexdigest()})
    config = {'schema_version': 1, 'host': '127.0.0.1', 'port': port, 'data_directory': str(directory / 'data'),
              'episode_days': 90, 'allowed_origins': [], 'principals': principals}
    write_private(directory / 'service.json', (canonical(config) + '\n').encode())
    adapter = {'schema_version': 1, 'endpoint': f'http://127.0.0.1:{port}/mcp', 'owner_id': owner,
               'workspace_id': workspace, 'host_instance_id': secrets.token_hex(16), 'thread_root': str(root),
               'credential_file': str(directory / 'adapter-credential.json'), 'request_timeout_ms': 1000,
               'retrieval': {'max_items': 5, 'max_utf8_bytes': 12000, 'estimated_tokens': 1500}}
    adapter_path = directory / ('adapter-' + hashlib.sha256(canonical(adapter).encode()).hexdigest()[:16] + '.json')
    write_private(adapter_path, (canonical(adapter) + '\n').encode())
    for event in ('turn.before', 'context.prepare', 'tool.completed', 'context.before_compact', 'turn.settled'):
        name = 'tekes-memory-' + event.replace('.', '-') + '.json'
        binding = {'format': 2, 'id': name, 'event': event, 'enabled': True,
                   'argv': invocation() + ['kernel', '--config', str(adapter_path)],
                   'env': {}, 'timeout_ms': 1500, 'stdout_bytes': 65536, 'stderr_bytes': 4096}
        write_private(directory / 'hooks' / name, (canonical(binding) + '\n').encode())
    return {'service_config': str(directory / 'service.json'), 'adapter_config': str(adapter_path),
            'hook_templates': str(directory / 'hooks'), 'endpoint': adapter['endpoint']}


def main(argv=None):
    parser = argparse.ArgumentParser(description='TekesMemory independent local MCP service')
    sub = parser.add_subparsers(dest='command', required=True)
    init = sub.add_parser('init')
    init.add_argument('--directory', required=True)
    init.add_argument('--workspace', required=True)
    init.add_argument('--thread-root', required=True)
    init.add_argument('--port', type=int, default=43187)
    serve = sub.add_parser('serve')
    serve.add_argument('--config', required=True)
    serve.add_argument('--ready-file')
    kernel = sub.add_parser('kernel')
    kernel.add_argument('--config', required=True)
    schemas = sub.add_parser('schemas')
    schemas.add_argument('--directory', required=True)
    launchd = sub.add_parser('launchd-plist')
    launchd.add_argument('--config', required=True)
    launchd.add_argument('--output', required=True)
    proxy = sub.add_parser('stdio')
    proxy.add_argument('--endpoint', required=True)
    proxy.add_argument('--credential-file', required=True)
    call = sub.add_parser('call')
    call.add_argument('--endpoint', required=True)
    call.add_argument('--credential-file', required=True)
    call.add_argument('--tool', required=True, choices=list(SCHEMAS))
    call.add_argument('--arguments-file', required=True)
    args = parser.parse_args(argv)
    os.umask(0o077)
    try:
        if args.command == 'init':
            if not 1 <= args.port <= 65535:
                raise MemoryError('invalid_argument')
            print(json.dumps(setup(args.directory, args.workspace, args.thread_root, args.port), indent=2))
        elif args.command == 'call':
            from .client import Client
            from .common import decode, MAX_BODY
            with open(args.arguments_file, 'rb') as source:
                raw = source.read(MAX_BODY + 1)
            if len(raw) > MAX_BODY:
                raise MemoryError('budget_exceeded')
            with Client(args.endpoint, private_json(args.credential_file)['token'], 5) as client:
                print(canonical(client.call(args.tool, decode(raw))))
        elif args.command == 'kernel':
            from .adapter import main as adapter_main
            adapter_main(['--config', args.config])
        elif args.command == 'schemas':
            directory = Path(args.directory)
            directory.mkdir(parents=True, exist_ok=True)
            from .config import SERVICE, ADAPTER
            for name, schema in (SCHEMAS | {'service-config': SERVICE, 'adapter-config': ADAPTER}).items():
                (directory / (name + '.schema.json')).write_text(json.dumps({'$schema': 'https://json-schema.org/draft/2020-12/schema', **schema}, indent=2) + '\n')
        elif args.command == 'launchd-plist':
            private_json(args.config)
            value = {'Label': 'local.tekesmemory', 'ProgramArguments': invocation() + ['serve', '--config', str(Path(args.config).resolve())],
                     'RunAtLoad': True, 'KeepAlive': True, 'ThrottleInterval': 10,
                     'StandardErrorPath': str(Path(args.config).resolve().parent / 'service.stderr.log')}
            write_private(args.output, plistlib.dumps(value))
        elif args.command == 'stdio':
            from .proxy import run
            run(args.endpoint, args.credential_file)
        elif args.command == 'serve':
            from .server import Server
            server = Server(args.config)
            def stop(*_):
                threading.Thread(target=server.shutdown, daemon=True).start()
            signal.signal(signal.SIGTERM, stop)
            signal.signal(signal.SIGINT, stop)
            if args.ready_file:
                write_private(args.ready_file, (canonical({'endpoint': f'http://127.0.0.1:{server.server_port}/mcp'}) + '\n').encode())
            try:
                server.serve_forever(poll_interval=0.1)
            finally:
                server.server_close()
    except (MemoryError, OSError, ValueError, KeyError):
        print('tekes-memory: command failed; check configuration and permissions', file=sys.stderr)
        raise SystemExit(1)
