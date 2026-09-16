"""Small MCP client for this service; bounded deadline and explicit session cleanup."""
import time
import urllib.request
import urllib.error
from urllib.parse import urlparse

from .common import MAX_BODY, PROTOCOL, MemoryError, canonical, decode


class Client:
    def __init__(self, url, token, timeout=1):
        parsed = urlparse(url)
        if parsed.scheme != 'http' or parsed.hostname not in ('127.0.0.1', 'localhost') or parsed.path != '/mcp' or parsed.username or parsed.query or parsed.fragment:
            raise MemoryError('invalid_endpoint')
        self.url, self.token = url, token
        self.deadline = time.monotonic() + timeout
        self.session = None
        self.counter = 0
        # Ambient proxy variables must not route local credentials to another host.
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
        result = self.rpc('initialize', {'protocolVersion': PROTOCOL, 'capabilities': {},
                                        'clientInfo': {'name': 'tekes-memory-client', 'version': '0.1.0'}})
        if result['protocolVersion'] != PROTOCOL:
            raise MemoryError('unsupported_protocol')
        self.rpc('notifications/initialized', {}, notification=True)

    def rpc(self, method, params, notification=False):
        self.counter += 1
        message = {'jsonrpc': '2.0', 'method': method, 'params': params}
        if not notification:
            message['id'] = self.counter
        headers = {'Authorization': 'Bearer ' + self.token, 'Content-Type': 'application/json',
                   'Accept': 'application/json, text/event-stream', 'MCP-Protocol-Version': PROTOCOL}
        if self.session:
            headers['MCP-Session-Id'] = self.session
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise MemoryError('unavailable')
        request = urllib.request.Request(self.url, data=canonical(message).encode(), headers=headers)
        with self.opener.open(request, timeout=remaining) as response:
            if response.headers.get('MCP-Session-Id'):
                self.session = response.headers['MCP-Session-Id']
            raw = response.read(MAX_BODY + 1)
            if len(raw) > MAX_BODY:
                raise MemoryError('budget_exceeded')
            if notification:
                if response.status != 202:
                    raise MemoryError('invalid_response')
                return None
            result = decode(raw)
        if result.get('id') != self.counter or 'result' not in result:
            raise MemoryError('invalid_response')
        return result['result']

    def call(self, name, args):
        result = self.rpc('tools/call', {'name': name, 'arguments': args})
        if result.get('isError'):
            raise MemoryError(result.get('structuredContent', {}).get('error', {}).get('code', 'unavailable'))
        return result['structuredContent']

    def close(self):
        if not self.session:
            return
        request = urllib.request.Request(self.url, method='DELETE', headers={
            'Authorization': 'Bearer ' + self.token, 'MCP-Session-Id': self.session,
            'MCP-Protocol-Version': PROTOCOL})
        try:
            with self.opener.open(request, timeout=max(0.01, min(0.1, self.deadline - time.monotonic()))) as response:
                response.read()
        except urllib.error.HTTPError as error:
            error.close()
        except Exception:
            pass
        self.session = None

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        raise MemoryError('invalid_endpoint')
