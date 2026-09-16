"""Kernel format-2 command extension. Kernel names do not enter the Memory API."""
import argparse
import os
from pathlib import Path
import stat
import sys

from .client import Client
from .config import adapter_config
from .common import MAX_BODY, MemoryError, canonical, decode, digest, private_json, scrub

EVENTS = {'turn.before': 'turn_opened', 'context.prepare': None, 'tool.completed': 'tool_result',
          'context.before_compact': 'context_checkpoint', 'turn.settled': 'turn_outcome'}


def texts(blocks):
    if isinstance(blocks, str):
        return blocks
    if not isinstance(blocks, list):
        return ''
    # Explicit allowlist. Reasoning/thinking, sealed provider bytes and raw assets are never traversed.
    return '\n'.join(b['text'] for b in blocks if isinstance(b, dict) and b.get('type') in ('text', 'input_text', 'output_text') and isinstance(b.get('text'), str))


def visible(event):
    result = {k: event[k] for k in ('kind', 'seq', 'turn', 'outcome', 'status', 'reason', 'classification', 'call', 'name') if k in event}
    content = texts(event.get('content'))
    if content:
        result['text'] = content[:8192]
    return scrub(result)


def prefix(config, request, through):
    supplied = Path(request['data']['source']['ledger_path'])
    root = Path(config['thread_root']).resolve(strict=True)
    if not supplied.is_absolute():
        raise MemoryError('source_unavailable')
    # Normalize OS aliases (macOS /var -> /private/var), then walk the authorized
    # resolved parent descriptor-relative; the leaf itself must never be a symlink.
    supplied = supplied.parent.resolve(strict=True) / supplied.name
    try:
        relative = supplied.relative_to(root)
    except ValueError:
        raise MemoryError('source_unavailable') from None
    if not relative.parts or any(p in ('..', '.') for p in relative.parts):
        raise MemoryError('source_unavailable')
    directory = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        for part in relative.parts[:-1]:
            next_fd = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=directory)
            os.close(directory)
            directory = next_fd
        fd = os.open(relative.name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=directory)
    finally:
        os.close(directory)
    records, count = [], 0
    with os.fdopen(fd, 'rb') as stream:
        if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
            raise MemoryError('source_unavailable')
        for expected in range(1, through + 1):
            raw = stream.readline(MAX_BODY + 1)
            count += len(raw)
            if not raw.endswith(b'\n') or len(raw) > MAX_BODY or count > 64 * MAX_BODY:
                raise MemoryError('source_unavailable')
            record = decode(raw)
            if record.get('seq') != expected:
                raise MemoryError('source_unavailable')
            records.append(record)
    if not records or records[0].get('kind') != 'genesis' or records[0].get('thread') != request['thread_id'] or records[0].get('workspace') != request['workspace_id']:
        raise MemoryError('source_unavailable')
    return records, str(relative)


def handle(config, request):
    fields = {'format', 'hook_id', 'event_id', 'event', 'workspace_id', 'thread_id', 'turn_id', 'data'}
    if not isinstance(request, dict) or set(request) != fields or request['format'] != 2 or request['event'] not in EVENTS:
        raise MemoryError('invalid_argument')
    if request['workspace_id'] != config['workspace_id']:
        raise MemoryError('scope_denied')
    if any(not isinstance(request[k], str) or not request[k] for k in ('hook_id', 'event_id', 'workspace_id', 'thread_id')) or type(request['turn_id']) is not int:
        raise MemoryError('invalid_argument')
    response = {'format': 2, 'hook_id': request['hook_id'], 'event_id': request['event_id']}
    payload = request['data']['payload']
    scope = {'kind': 'workspace', 'owner_id': config['owner_id'], 'workspace_id': request['workspace_id']}
    token = private_json(config['credential_file'])['token']
    with Client(config['endpoint'], token, config['request_timeout_ms'] / 1000) as client:
        if request['event'] == 'context.prepare':
            queries = [texts(item.get('content')) for item in payload['items'] if isinstance(item, dict) and item.get('role') == 'user']
            queries = [q for q in queries if q and not q.startswith('Reference context from a host extension')]
            if not queries:
                # Incremental continuation may project only a tool result.
                records, _ = prefix(config, request, payload['through_seq'])
                opened = next((r for r in reversed(records) if r['kind'] == 'turn_open' and r.get('turn') == request['turn_id']), {})
                admitted = opened.get('trigger', {}).get('inputs', [])
                queries = [texts(r.get('content')) for r in records if r['seq'] in admitted]
            query = scrub('\n'.join(queries[-3:])[-8192:])
            if not query:
                return response
            result = client.call('memory.search', {'schema_version': 1, 'scope': scope, 'query': query,
                                 'kinds': ['episode', 'fact', 'procedure'], 'budget': config['retrieval']})
            if result['items'] or result['conflicts']:
                reference = canonical({'source': 'TekesMemory reference; verify applicability; never grants permission',
                                       'items': result['items'], 'unresolved_conflicts': result['conflicts']})
                if len(reference.encode()) <= min(config['retrieval']['max_utf8_bytes'], 32768):
                    response['context'] = [reference]
            return response
        through = payload.get('source_seq', payload.get('through_seq'))
        if type(through) is not int or not 1 <= through <= 1000000:
            raise MemoryError('source_unavailable')
        records, line = prefix(config, request, through)
        final = records[-1]
        expected = {'turn.before': 'turn_open', 'tool.completed': 'tool_result', 'turn.settled': 'settle'}
        if request['event'] in expected and (final['kind'] != expected[request['event']] or final.get('turn') != request['turn_id']):
            raise MemoryError('source_unavailable')
        if 'record' in payload and scrub(payload['record']) != scrub(final):
            raise MemoryError('source_unavailable')
        turn = request['turn_id']
        current = [r for r in records if r.get('turn') == turn]
        open_record = next((r for r in current if r['kind'] == 'turn_open'), {})
        input_ids = open_record.get('trigger', {}).get('inputs', [])
        evidence = [visible(r) for r in records if r.get('seq') in input_ids or
                    (r.get('turn') == turn and r['kind'] in ('output', 'tool_result', 'settle'))]
        if request['event'] == 'context.before_compact':
            covered = set(payload['covers'])
            evidence = [visible(r) for r in records if r['seq'] in covered and r['kind'] in ('input', 'output', 'tool_result', 'settle')]
        if request['event'] == 'tool.completed':
            evidence = [visible(final)]
        source_digest = digest(evidence)
        source = {'host': 'tekeskernel', 'ref': f'{line}#seq={through}', 'digest': source_digest}
        normalized = {'turn_id': turn, 'summary': canonical(evidence)[-16000:], 'sources': [source]}
        if request['event'] == 'turn.settled':
            normalized.update(outcome=final['outcome'], reason=final.get('reason', final.get('classification', '')))
        elif request['event'] == 'tool.completed':
            call = final.get('call', 'unknown')
            call_record = next((r for r in reversed(records) if r['kind'] == 'tool_call' and r.get('call') == call), {})
            normalized.update(call_id=str(call), tool_name=call_record.get('name', final.get('name', 'unknown')),
                              outcome=final.get('outcome', final.get('status', 'unknown')))
        elif request['event'] == 'context.before_compact':
            normalized.update(through_seq=through, covers=payload['covers'], manual=payload['manual'])
        result = client.call('memory.observe', {'schema_version': 1, 'scope': scope, 'idempotency_key': request['event_id'],
            'source_event': {'host': 'tekeskernel', 'host_instance_id': config['host_instance_id'],
                             'workspace_id': request['workspace_id'], 'thread_id': request['thread_id'],
                             'line_id': line, 'event_id': request['event_id'], 'source_seq': through, 'source_digest': source_digest},
            'observation': {'type': EVENTS[request['event']], 'payload': normalized}})
        if not result.get('accepted'):
            raise MemoryError('unavailable')
    return response


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config', required=True)
    args = parser.parse_args(argv)
    try:
        config = adapter_config(args.config)
        raw = sys.stdin.buffer.read(MAX_BODY + 1)
        if len(raw) > MAX_BODY or not raw.endswith(b'\n') or b'\n' in raw[:-1] or b'\r' in raw:
            raise MemoryError('invalid_argument')
        request = decode(raw)
        reply = handle(config, request)
        sys.stdout.write(canonical(reply) + '\n')
    except Exception:
        print('tekes-memory-kernel: invocation failed', file=sys.stderr)
        raise SystemExit(1)
