"""stdio compatibility relay. It never opens SQLite or runs a second service."""
import sys
import urllib.request

from .client import NoRedirect
from .common import MAX_BODY, PROTOCOL, MemoryError, canonical, decode, private_json
from urllib.parse import urlparse


def run(endpoint, credential_file):
    parsed = urlparse(endpoint)
    if parsed.scheme != 'http' or parsed.hostname not in ('127.0.0.1', 'localhost') or parsed.path != '/mcp' or parsed.username or parsed.query or parsed.fragment:
        raise MemoryError('invalid_endpoint')
    token = private_json(credential_file)['token']
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    session = None
    try:
        while True:
            raw = sys.stdin.buffer.readline(MAX_BODY + 1)
            if not raw:
                break
            if len(raw) > MAX_BODY or not raw.endswith(b'\n'):
                raise MemoryError('invalid_argument')
            request = decode(raw)
            headers = {'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json',
                       'Accept': 'application/json, text/event-stream', 'MCP-Protocol-Version': PROTOCOL}
            if session:
                headers['MCP-Session-Id'] = session
            with opener.open(urllib.request.Request(endpoint, data=canonical(request).encode(), headers=headers), timeout=5) as response:
                session = response.headers.get('MCP-Session-Id', session)
                body = response.read(MAX_BODY + 1)
                if len(body) > MAX_BODY:
                    raise MemoryError('budget_exceeded')
                if body:
                    sys.stdout.write(canonical(decode(body)) + '\n')
                    sys.stdout.flush()
    finally:
        if session:
            try:
                with opener.open(urllib.request.Request(endpoint, method='DELETE', headers={
                    'Authorization': 'Bearer ' + token, 'MCP-Session-Id': session,
                    'MCP-Protocol-Version': PROTOCOL}), timeout=0.2):
                    pass
            except Exception:
                pass
