"""Loopback-only, authenticated Streamable HTTP MCP server (JSON responses)."""
import fcntl
import hashlib
import hmac
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import secrets
import threading
import time

from . import __version__
from .common import MAX_BODY, PROTOCOL, MemoryError, canonical, decode, private_json
from .schema import SCHEMAS, tools_for
from .store import Store
from .config import service_config


class Server(ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 32

    def __init__(self, config_path):
        self.config_path = Path(config_path).resolve()
        self.config = service_config(self.config_path)
        if self.config.get('schema_version') != 1 or self.config.get('host') != '127.0.0.1':
            raise MemoryError('invalid_config')
        directory = Path(self.config['data_directory'])
        if not directory.is_absolute() or directory.is_symlink():
            raise MemoryError('invalid_config')
        directory.mkdir(mode=0o700, parents=True, exist_ok=True)
        info = directory.stat()
        if info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise MemoryError('unsafe_config_permissions')
        self.lock_fd = os.open(directory / 'service.lock', os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
        try:
            fcntl.flock(self.lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            os.close(self.lock_fd)
            raise MemoryError('service_already_running') from None
        for candidate in directory.glob('memory.sqlite3*'):
            if candidate.is_symlink() or not candidate.is_file():
                os.close(self.lock_fd)
                raise MemoryError('unsafe_database_path')
        self.store = Store(directory / 'memory.sqlite3', episode_days=self.config.get('episode_days', 90))
        self.sessions = {}
        self.session_lock = threading.Lock()
        self.slots = threading.BoundedSemaphore(32)
        self.stopping = threading.Event()
        super().__init__(('127.0.0.1', self.config['port']), Handler)
        self.maintenance = threading.Thread(target=self._maintain, daemon=True)
        self.maintenance.start()

    def process_request(self, request, address):
        if not self.slots.acquire(blocking=False):
            request.close()
            return
        super().process_request(request, address)

    def process_request_thread(self, request, address):
        try:
            super().process_request_thread(request, address)
        finally:
            self.slots.release()

    def _maintain(self):
        while not self.stopping.is_set():
            try:
                busy = self.store.maintain()
            except Exception:
                busy = False
                print('memory maintenance failed; retrying', file=__import__('sys').stderr)
            self.stopping.wait(0.01 if busy else 0.25)

    def server_close(self):
        self.stopping.set()
        self.maintenance.join(timeout=5)
        super().server_close()
        self.store.close()
        fcntl.flock(self.lock_fd, fcntl.LOCK_UN)
        os.close(self.lock_fd)


class Handler(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def setup(self):
        super().setup()
        self.connection.settimeout(3)

    def log_message(self, *_):
        pass  # Never log request data, headers, bearer tokens or source text.

    def respond(self, status, value=None, session=None):
        data = canonical(value).encode() if value is not None else b''
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data)))
        self.send_header('Cache-Control', 'no-store')
        self.send_header('Connection', 'close')
        if session:
            self.send_header('MCP-Session-Id', session)
        self.end_headers()
        self.close_connection = True
        if data:
            self.wfile.write(data)

    def auth(self):
        if self.path != '/mcp':
            self.respond(404)
            return None
        host = self.headers.get('Host', '')
        port = self.server.server_port
        if host not in (f'127.0.0.1:{port}', f'localhost:{port}'):
            self.respond(403)
            return None
        # Read current credentials/grants on every request: revocation takes effect immediately.
        config = service_config(self.server.config_path)
        origin = self.headers.get('Origin')
        if origin is not None and origin not in config.get('allowed_origins', []):
            self.respond(403)
            return None
        header = self.headers.get('Authorization', '')
        if not header.startswith('Bearer '):
            self.respond(401)
            return None
        token_hash = hashlib.sha256(header[7:].encode()).hexdigest()
        for principal in config['principals']:
            if hmac.compare_digest(token_hash, principal['token_sha256']):
                return principal
        self.respond(401)
        return None

    def do_GET(self):
        try:
            if self.auth():
                self.respond(405)
        except (MemoryError, OSError):
            self.respond(503)

    def do_DELETE(self):
        try:
            principal = self.auth()
            if not principal:
                return
            session_id = self.headers.get('MCP-Session-Id')
            with self.server.session_lock:
                session = self.server.sessions.get(session_id)
                if not session or session['principal'] != principal['id']:
                    self.respond(404)
                    return
                del self.server.sessions[session_id]
            self.respond(200)
        except (MemoryError, OSError):
            self.respond(503)

    def do_POST(self):
        request_id = None
        try:
            principal = self.auth()
            if not principal:
                return
            if self.headers.get('Content-Type', '').split(';')[0].strip() != 'application/json':
                self.respond(415)
                return
            accept = self.headers.get('Accept', '')
            if 'application/json' not in accept or 'text/event-stream' not in accept:
                self.respond(406)
                return
            if self.headers.get('Transfer-Encoding') or not self.headers.get('Content-Length', '').isdigit():
                self.respond(400)
                return
            length = int(self.headers['Content-Length'])
            if not 0 < length <= MAX_BODY:
                self.respond(413)
                return
            raw = self.rfile.read(length)
            if len(raw) != length:
                self.respond(400)
                return
            try:
                request = decode(raw)
            except MemoryError:
                self.respond(400, {'jsonrpc': '2.0', 'id': None, 'error': {'code': -32700, 'message': 'Parse error'}})
                return
            if not isinstance(request, dict) or request.get('jsonrpc') != '2.0' or not isinstance(request.get('method'), str):
                self.respond(400, {'jsonrpc': '2.0', 'id': None, 'error': {'code': -32600, 'message': 'Invalid request'}})
                return
            request_id = request.get('id')
            if request_id is not None and type(request_id) not in (int, str):
                self.respond(400)
                return
            params = request.get('params', {})
            if not isinstance(params, dict):
                self.respond(400)
                return
            method = request['method']
            if method == 'initialize':
                if request_id is None or not isinstance(params.get('protocolVersion'), str) or not isinstance(params.get('capabilities'), dict) or not isinstance(params.get('clientInfo'), dict):
                    self.respond(400)
                    return
                session_id = secrets.token_urlsafe(24)
                with self.server.session_lock:
                    now = time.monotonic()
                    self.server.sessions = {k: v for k, v in self.server.sessions.items() if v['expires'] > now}
                    if len(self.server.sessions) >= 1024:
                        self.respond(503)
                        return
                    self.server.sessions[session_id] = {'principal': principal['id'], 'initialized': False, 'expires': now + 3600}
                self.respond(200, {'jsonrpc': '2.0', 'id': request_id, 'result': {'protocolVersion': PROTOCOL,
                    'capabilities': {'tools': {'listChanged': False}}, 'serverInfo': {'name': 'TekesMemory', 'version': __version__}}}, session_id)
                return
            session_id = self.headers.get('MCP-Session-Id')
            with self.server.session_lock:
                session = self.server.sessions.get(session_id)
                if not session or session['principal'] != principal['id'] or session['expires'] <= time.monotonic():
                    self.respond(404 if session_id else 400)
                    return
                if self.headers.get('MCP-Protocol-Version', PROTOCOL) != PROTOCOL:
                    self.respond(400)
                    return
                if method == 'notifications/initialized' and request_id is None:
                    session['initialized'] = True
                    self.respond(202)
                    return
                initialized = session['initialized']
            if not initialized:
                self.respond(400)
                return
            if request_id is None:
                # No long-running MCP operations; a committed transaction is not rolled back by cancellation.
                self.respond(202)
                return
            if method == 'ping':
                result = {}
            elif method == 'tools/list':
                result = {'tools': tools_for(principal)}
            elif method == 'tools/call':
                name, args = params.get('name'), params.get('arguments', {})
                if name not in SCHEMAS:
                    self.respond(200, {'jsonrpc': '2.0', 'id': request_id, 'error': {'code': -32602, 'message': 'Unknown tool'}})
                    return
                try:
                    data = self.server.store.call(principal, name, args)
                    result = {'structuredContent': data, 'content': [{'type': 'text', 'text': canonical(data)}], 'isError': False}
                except MemoryError as error:
                    data = {'schema_version': 1, 'error': {'code': error.code}}
                    result = {'structuredContent': data, 'content': [{'type': 'text', 'text': error.code}], 'isError': True}
            else:
                self.respond(200, {'jsonrpc': '2.0', 'id': request_id, 'error': {'code': -32601, 'message': 'Method not found'}})
                return
            self.respond(200, {'jsonrpc': '2.0', 'id': request_id, 'result': result})
        except (BrokenPipeError, ConnectionResetError, TimeoutError):
            self.close_connection = True
        except Exception:
            # No arbitrary exception text may enter an HTTP response or logs.
            self.respond(500, {'jsonrpc': '2.0', 'id': request_id, 'error': {'code': -32603, 'message': 'Internal error'}})
