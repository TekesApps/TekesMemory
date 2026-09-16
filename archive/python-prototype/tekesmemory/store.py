"""Transactional local memory. No host filesystem access and no model calls."""
import base64
import hashlib
import hmac
import json
from pathlib import Path
import re
import secrets
import sqlite3
import threading
import time
import uuid

from .common import MemoryError, authorize, canonical, digest, scrub, validate
from .schema import SCHEMAS


def uid():
    return uuid.uuid4().hex


def words(text):
    tokens = re.findall(r'[a-z0-9_]+', text.lower())
    for run in re.findall(r'[\u3400-\u9fff]+', text):
        tokens.extend(run if len(run) == 1 else [run[i:i + 2] for i in range(len(run) - 1)])
    return list(dict.fromkeys(tokens))[:256]


class Store:
    def __init__(self, path, clock=time.time, episode_days=90):
        if sqlite3.sqlite_version_info < (3, 42, 0):
            raise MemoryError('sqlite_3_42_required')
        self.lock = threading.RLock()
        self.clock = clock
        self.episode_days = episode_days
        self.db = sqlite3.connect(path, check_same_thread=False, isolation_level=None, timeout=2)
        self.db.row_factory = sqlite3.Row
        self.db.execute('PRAGMA foreign_keys=ON')
        self.db.execute('PRAGMA journal_mode=WAL')
        self.db.execute('PRAGMA synchronous=FULL')
        self.db.execute('PRAGMA secure_delete=ON')
        version = self.db.execute('PRAGMA user_version').fetchone()[0]
        if version not in (0, 1):
            self.db.close()
            raise MemoryError('unsupported_database_version')
        self.db.executescript('''
        BEGIN IMMEDIATE;
        CREATE TABLE IF NOT EXISTS records(id TEXT PRIMARY KEY,scope TEXT NOT NULL,kind TEXT NOT NULL,
          revision INTEGER NOT NULL,status TEXT NOT NULL,expires INTEGER,doc TEXT NOT NULL,fact_key TEXT);
        CREATE INDEX IF NOT EXISTS records_scope ON records(scope,status,kind);
        CREATE INDEX IF NOT EXISTS records_fact ON records(scope,fact_key);
        CREATE TABLE IF NOT EXISTS versions(id TEXT,revision INTEGER,doc TEXT NOT NULL,PRIMARY KEY(id,revision));
        CREATE VIRTUAL TABLE IF NOT EXISTS search_index USING fts5(id UNINDEXED, words);
        CREATE TABLE IF NOT EXISTS operations(id TEXT PRIMARY KEY,principal TEXT,scope TEXT,key TEXT,
          digest TEXT,result TEXT,UNIQUE(principal,scope,key));
        CREATE TABLE IF NOT EXISTS observations(id TEXT PRIMARY KEY,scope TEXT,source_key TEXT,digest TEXT,
          payload TEXT,state TEXT,record_id TEXT,UNIQUE(scope,source_key,digest));
        CREATE TABLE IF NOT EXISTS jobs(id TEXT PRIMARY KEY,state TEXT NOT NULL,attempts INTEGER NOT NULL,
          next_at INTEGER NOT NULL,error TEXT);
        CREATE TABLE IF NOT EXISTS dependencies(record_id TEXT,source_key TEXT,PRIMARY KEY(record_id,source_key));
        CREATE TABLE IF NOT EXISTS tombstones(scope TEXT,source_key TEXT,PRIMARY KEY(scope,source_key));
        CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
        PRAGMA user_version=1;
        COMMIT;
        ''')
        self.db.execute("INSERT INTO search_index(search_index,rank) VALUES('secure-delete',1)")
        self.db.execute('INSERT OR IGNORE INTO meta VALUES(?,?)', ('cursor_secret', secrets.token_hex(32)))
        self.db.execute('INSERT OR IGNORE INTO meta VALUES(?,?)', ('generation', '0'))
        self.cursor_key = bytes.fromhex(self.db.execute("SELECT value FROM meta WHERE key='cursor_secret'").fetchone()[0])

    def close(self):
        with self.lock:
            self.db.close()

    def _bump(self):
        self.db.execute("UPDATE meta SET value=CAST(value AS INTEGER)+1 WHERE key='generation'")

    def call(self, principal, name, args):
        if name not in principal['tools'] or (name == 'memory.observe' and principal['role'] != 'adapter'):
            raise MemoryError('scope_denied')
        if name not in SCHEMAS:
            raise MemoryError('invalid_argument')
        validate(args, SCHEMAS[name])
        if 'scope' in args:
            authorize(principal, args['scope'])
        with self.lock:
            if name == 'memory.status':
                row = self.db.execute('SELECT * FROM operations WHERE id=? AND principal=?',
                                      (args['operation_id'], principal['id'])).fetchone()
                if not row:
                    raise MemoryError('not_found')
                authorize(principal, json.loads(row['scope']))
                result = json.loads(row['result'])
                job = self.db.execute('SELECT state,error FROM jobs WHERE id=?', (result.get('observation_id'),)).fetchone()
                return {'schema_version': 1, 'request_id': uid(), 'operation_id': row['id'],
                        'processing_state': job['state'] if job else 'completed',
                        'error': job['error'] if job else None}
            if name in ('memory.search', 'memory.get'):
                result = self._search(principal, args) if name.endswith('search') else self._get(args)
                return {'schema_version': 1, 'request_id': uid(), **result}
            scope = canonical(args['scope'])
            request_digest = digest({'name': name, 'args': args})
            self.db.execute('BEGIN IMMEDIATE')
            try:
                old = self.db.execute('SELECT * FROM operations WHERE principal=? AND scope=? AND key=?',
                                      (principal['id'], scope, args['idempotency_key'])).fetchone()
                if old:
                    if old['digest'] != request_digest:
                        raise MemoryError('idempotency_conflict')
                    result = json.loads(old['result'])
                else:
                    safe = scrub(args)
                    method = getattr(self, '_' + name.split('.')[1])
                    result = {'schema_version': 1, 'request_id': uid(), 'operation_id': uid(), **method(safe)}
                    self.db.execute('INSERT INTO operations VALUES(?,?,?,?,?,?)',
                                    (result['operation_id'], principal['id'], scope, args['idempotency_key'],
                                     request_digest, canonical(result)))
                    self._bump()
                self.db.execute('COMMIT')
                return result
            except BaseException:
                self.db.execute('ROLLBACK')
                raise

    def _row(self, scope, record_id):
        row = self.db.execute('SELECT * FROM records WHERE id=? AND scope=?', (record_id, canonical(scope))).fetchone()
        if not row or row['status'] in ('deleted', 'expired') or (row['expires'] is not None and row['expires'] <= self.clock()):
            raise MemoryError('not_found')
        return row

    def _current_status(self, record):
        if record['kind'] != 'fact':
            return record['status']
        now, fact = self.clock(), record['fact']
        if not fact.get('valid_from', 0) <= now < fact.get('valid_to', 9007199254740991):
            return 'expired'
        rows = self.db.execute("SELECT doc FROM records WHERE scope=? AND fact_key=? AND status IN ('active','contested') AND (expires IS NULL OR expires>?)",
                               (canonical(record['scope']), self._fact_key(record), int(now)))
        for row in rows:
            other = json.loads(row['doc'])['fact']
            if other['value'] != fact['value'] and other.get('valid_from', 0) <= now < other.get('valid_to', 9007199254740991):
                return 'contested'
        return 'active'

    def _get(self, args):
        row = self._row(args['scope'], args['id'])
        if 'revision' in args:
            version = self.db.execute('SELECT doc FROM versions WHERE id=? AND revision=?',
                                      (row['id'], args['revision'])).fetchone()
            if not version:
                raise MemoryError('not_found')
            item = json.loads(version['doc'])
            item['historical'] = args['revision'] != row['revision']
        else:
            item = json.loads(row['doc'])
        if not item.get('historical'):
            item['status'] = self._current_status(item)
            if item['status'] == 'expired':
                raise MemoryError('not_found')
        return {'item': item}

    @staticmethod
    def _fact_key(record):
        fact = record.get('fact')
        return digest({k: fact.get(k) for k in ('subject', 'predicate', 'environment')}) if fact else None

    def _put(self, record):
        scope = canonical(record['scope'])
        self.db.execute('INSERT OR REPLACE INTO records VALUES(?,?,?,?,?,?,?,?)',
                        (record['id'], scope, record['kind'], record['revision'], record['status'],
                         record['expires_at'], canonical(record), self._fact_key(record)))
        self.db.execute('INSERT OR REPLACE INTO versions VALUES(?,?,?)', (record['id'], record['revision'], canonical(record)))
        self.db.execute('DELETE FROM search_index WHERE id=?', (record['id'],))
        if record['status'] in ('active', 'contested'):
            self.db.execute('INSERT INTO search_index VALUES(?,?)',
                            (record['id'], ' '.join(words(record['content'] + ' ' + canonical(record.get('fact', {}))))))
        # Retain source dependencies from historical revisions as well.
        for source in record['sources']:
            if source.get('source_key'):
                self.db.execute('INSERT OR IGNORE INTO dependencies VALUES(?,?)', (record['id'], source['source_key']))

    def _blocked(self, scope, sources):
        for source in sources:
            if source.get('source_key') and self.db.execute('SELECT 1 FROM tombstones WHERE scope=? AND source_key=?',
                                                           (canonical(scope), source['source_key'])).fetchone():
                raise MemoryError('source_unavailable')

    def _save(self, args):
        self._blocked(args['scope'], args['sources'])
        kind = args['kind']
        if (kind == 'fact') != ('fact' in args) or (kind == 'procedure') != ('procedure' in args):
            raise MemoryError('invalid_argument')
        now = int(self.clock())
        if 'verified_at' in args and args['verified_at'] > now:
            raise MemoryError('invalid_argument')
        if 'fact' in args and args['fact'].get('valid_to', 9007199254740991) <= args['fact'].get('valid_from', 0):
            raise MemoryError('invalid_argument')
        record = {'id': uid(), 'scope': args['scope'], 'kind': kind, 'revision': 1, 'status': 'active',
                  'content': args['content'], 'sources': args['sources'], 'observed_at': now,
                  'verified_at': args.get('verified_at'), 'pinned': args.get('pinned', False),
                  'expires_at': args.get('expires_at', now + self.episode_days * 86400 if kind == 'episode' else None)}
        if record['pinned']:
            record['expires_at'] = None
        for field in ('fact', 'procedure'):
            if field in args:
                record[field] = args[field]
        self._put(record)
        if kind == 'fact':
            self._reconcile(args['scope'], [self._fact_key(record)])
        row = self.db.execute('SELECT status FROM records WHERE id=?', (record['id'],)).fetchone()
        return {'id': record['id'], 'revision': 1, 'status': row['status']}

    def _correct(self, args):
        old = json.loads(self._row(args['scope'], args['id'])['doc'])
        if old['revision'] != args['expected_revision']:
            raise MemoryError('revision_conflict')
        if (old['kind'] == 'fact') != ('fact' in args) or (old['kind'] == 'procedure') != ('procedure' in args):
            raise MemoryError('invalid_argument')
        self._blocked(args['scope'], args['sources'])
        if 'fact' in args and args['fact'].get('valid_to', 9007199254740991) <= args['fact'].get('valid_from', 0):
            raise MemoryError('invalid_argument')
        old_key = self._fact_key(old)
        old.update(revision=old['revision'] + 1, content=args['replacement'], sources=args['sources'],
                   observed_at=int(self.clock()), verified_at=None, status='active')
        for field in ('fact', 'procedure'):
            if field in args:
                old[field] = args[field]
        self._put(old)
        if old_key:
            self._reconcile(args['scope'], [old_key, self._fact_key(old)])
        return {'id': old['id'], 'revision': old['revision'],
                'status': self.db.execute('SELECT status FROM records WHERE id=?', (old['id'],)).fetchone()[0]}

    def _reconcile(self, scope, keys=None):
        sql = "SELECT doc FROM records WHERE scope=? AND kind='fact' AND status IN ('active','contested') AND (expires IS NULL OR expires>?)"
        params = [canonical(scope), int(self.clock())]
        if keys:
            sql += ' AND fact_key IN (' + ','.join('?' for _ in keys) + ')'
            params.extend(keys)
        groups = {}
        for row in self.db.execute(sql, params):
            record = json.loads(row['doc'])
            groups.setdefault(self._fact_key(record), []).append(record)
        for records in groups.values():
            for a in records:
                fa = a['fact']
                conflict = any(a['id'] != b['id'] and fa['value'] != b['fact']['value'] and
                    max(fa.get('valid_from', 0), b['fact'].get('valid_from', 0)) <
                    min(fa.get('valid_to', 9007199254740991), b['fact'].get('valid_to', 9007199254740991))
                    for b in records)
                status = 'contested' if conflict else 'active'
                if a['status'] != status:
                    a['status'] = status
                    self._put(a)

    def _erase(self, scope, ids, block_sources=True):
        queue = list(ids)
        seen = set()
        while queue:
            record_id = queue.pop()
            if record_id in seen:
                continue
            seen.add(record_id)
            row = self.db.execute('SELECT * FROM records WHERE id=? AND scope=?', (record_id, scope)).fetchone()
            if not row:
                continue
            sources = self.db.execute('SELECT source_key FROM dependencies WHERE record_id=?', (record_id,)).fetchall()
            for source in sources:
                key = source['source_key']
                if block_sources:
                    self.db.execute('INSERT OR IGNORE INTO tombstones VALUES(?,?)', (scope, key))
                queue.extend(r[0] for r in self.db.execute('SELECT d.record_id FROM dependencies d JOIN records r ON r.id=d.record_id WHERE d.source_key=? AND r.scope=?', (key, scope)))
                self.db.execute("UPDATE observations SET payload='{}',state='deleted' WHERE scope=? AND source_key=?", (scope, key))
                self.db.execute("UPDATE jobs SET state='completed' WHERE id IN (SELECT id FROM observations WHERE scope=? AND source_key=?)", (scope, key))
            self.db.execute('DELETE FROM versions WHERE id=?', (record_id,))
            self.db.execute('DELETE FROM search_index WHERE id=?', (record_id,))
            self.db.execute('DELETE FROM dependencies WHERE record_id=?', (record_id,))
            self.db.execute("UPDATE records SET status='deleted',doc='{}' WHERE id=?", (record_id,))
        return len(seen)

    def _forget(self, args):
        if len(args['ids']) != len(args['expected_revisions']) or len(set(args['ids'])) != len(args['ids']):
            raise MemoryError('invalid_argument')
        for record_id, rev in zip(args['ids'], args['expected_revisions']):
            # Explicit deletion also permits expired records.
            row = self.db.execute("SELECT revision FROM records WHERE id=? AND scope=? AND status!='deleted'", (record_id, canonical(args['scope']))).fetchone()
            if not row:
                raise MemoryError('not_found')
            if row['revision'] != rev:
                raise MemoryError('revision_conflict')
        count = self._erase(canonical(args['scope']), args['ids'])
        self._reconcile(args['scope'])
        return {'hidden': True, 'records_deleted': count, 'physical_cleanup': 'checkpoint_pending',
                'external_copies': 'Host logs, hook receipts, provider request assets and backups require separate cleanup.'}

    def _observe(self, args):
        event, observation = args['source_event'], args['observation']
        if args['scope']['kind'] == 'user' or event['workspace_id'] != args['scope']['workspace_id']:
            raise MemoryError('scope_denied')
        if args['scope']['kind'] == 'thread' and event['thread_id'] != args['scope']['thread_id']:
            raise MemoryError('scope_denied')
        payload = observation['payload']
        required = {'turn_opened': [], 'tool_result': ['call_id', 'tool_name', 'outcome'],
                    'context_checkpoint': ['through_seq', 'covers', 'manual'], 'turn_outcome': ['outcome'],
                    'source_invalidated': ['source_keys']}[observation['type']]
        if any(k not in payload for k in required):
            raise MemoryError('invalid_argument')
        identity = {k: event[k] for k in ('host', 'host_instance_id', 'workspace_id', 'thread_id', 'line_id', 'source_seq')}
        identity['type'] = observation['type']
        key, scope = digest(identity), canonical(args['scope'])
        old = self.db.execute('SELECT * FROM observations WHERE scope=? AND source_key=? AND digest=?', (scope, key, event['source_digest'])).fetchone()
        if old:
            if old['state'] not in ('deleted', 'expired') and old['payload'] != canonical(observation):
                raise MemoryError('idempotency_conflict')
            return {'accepted': True, 'observation_id': old['id'], 'processing_state': old['state'], 'source_key': key}
        if self.db.execute('SELECT 1 FROM tombstones WHERE scope=? AND source_key=?', (scope, key)).fetchone():
            raise MemoryError('source_unavailable')
        # Invalidate any previous version and all its derived content before replacement.
        previous = self.db.execute('SELECT record_id FROM observations WHERE scope=? AND source_key=?', (scope, key)).fetchall()
        self._erase(scope, [p[0] for p in previous if p[0]], block_sources=False)
        self.db.execute("UPDATE observations SET payload='{}',state='deleted' WHERE scope=? AND source_key=?", (scope, key))
        self.db.execute("UPDATE jobs SET state='completed' WHERE id IN (SELECT id FROM observations WHERE scope=? AND source_key=?)", (scope, key))
        if observation['type'] == 'source_invalidated':
            for target in payload['source_keys']:
                self.db.execute('INSERT OR IGNORE INTO tombstones VALUES(?,?)', (scope, target))
                ids = [r[0] for r in self.db.execute('SELECT d.record_id FROM dependencies d JOIN records r ON r.id=d.record_id WHERE r.scope=? AND d.source_key=?', (scope, target))]
                self._erase(scope, ids)
                self.db.execute("UPDATE observations SET payload='{}',state='deleted' WHERE scope=? AND source_key=?", (scope, target))
                self.db.execute("UPDATE jobs SET state='completed' WHERE id IN (SELECT id FROM observations WHERE scope=? AND source_key=?)", (scope, target))
        obs_id = uid()
        state = 'completed' if observation['type'] in ('turn_opened', 'source_invalidated') else 'accepted'
        self.db.execute('INSERT INTO observations VALUES(?,?,?,?,?,?,NULL)',
                        (obs_id, scope, key, event['source_digest'], canonical(observation), state))
        self.db.execute('INSERT INTO jobs VALUES(?,?,0,?,NULL)', (obs_id, state, int(self.clock())))
        return {'accepted': True, 'observation_id': obs_id, 'processing_state': state, 'source_key': key}

    def maintain(self):
        """One bounded work item, atomically. Crashes roll back the whole item."""
        with self.lock:
            self.db.execute('BEGIN IMMEDIATE')
            job = None
            try:
                now = int(self.clock())
                expired = self.db.execute("SELECT id,doc FROM records WHERE status IN ('active','contested') AND expires<=?", (now,)).fetchall()
                expired_scopes = set()
                for row in expired:
                    record = json.loads(row['doc'])
                    expired_scopes.add(canonical(record['scope']))
                    # Expiration removes service-owned body/history/index text. Keep
                    # source identities so explicit deletion can still prevent reimport.
                    self.db.execute('DELETE FROM versions WHERE id=?', (row['id'],))
                    self.db.execute('DELETE FROM search_index WHERE id=?', (row['id'],))
                    self.db.execute("UPDATE records SET status='expired',doc='{}' WHERE id=?", (row['id'],))
                    self.db.execute("UPDATE observations SET payload='{}',state='expired' WHERE record_id=?", (row['id'],))
                for expired_scope in expired_scopes:
                    self._reconcile(json.loads(expired_scope))
                job = self.db.execute("SELECT j.*,o.scope,o.payload,o.source_key FROM jobs j JOIN observations o ON o.id=j.id WHERE j.state='accepted' AND j.next_at<=? ORDER BY j.next_at,j.id LIMIT 1", (now,)).fetchone()
                if job:
                    observation = json.loads(job['payload'])
                    payload = observation['payload']
                    sources = [dict(s, source_key=job['source_key']) for s in payload['sources']]
                    summary = canonical({'observation': observation['type'], **{k: v for k, v in payload.items() if k != 'sources'}})
                    saved = self._save({'scope': json.loads(job['scope']), 'kind': 'episode',
                                        'content': summary[:32768], 'sources': sources})
                    self.db.execute("UPDATE observations SET state='completed',record_id=? WHERE id=?", (saved['id'], job['id']))
                    self.db.execute("UPDATE jobs SET state='completed',attempts=attempts+1,error=NULL WHERE id=?", (job['id'],))
                if job or expired:
                    self._bump()
                self.db.execute('COMMIT')
            except Exception:
                self.db.execute('ROLLBACK')
                if job:
                    attempts = job['attempts'] + 1
                    self.db.execute("UPDATE jobs SET state=?,attempts=?,next_at=?,error='processing_failed' WHERE id=?",
                                    ('failed' if attempts >= 5 else 'accepted', attempts, int(self.clock()) + 2 ** attempts, job['id']))
                else:
                    raise
            self.db.execute('PRAGMA wal_checkpoint(TRUNCATE)')
            return bool(job)

    def _search(self, principal, args):
        scopes = [args['scope']]
        if args['scope']['kind'] == 'thread':
            scopes.append({k: v for k, v in args['scope'].items() if k != 'thread_id'} | {'kind': 'workspace'})
        if args['scope']['kind'] != 'user':
            scopes.append({'kind': 'user', 'owner_id': args['scope']['owner_id']})
        permitted = []
        for scope in scopes:
            try:
                authorize(principal, scope)
                permitted.append(canonical(scope))
            except MemoryError:
                pass
        query_id = digest({k: v for k, v in args.items() if k != 'cursor'} | {'principal': principal['id']})
        generation = self.db.execute("SELECT value FROM meta WHERE key='generation'").fetchone()[0]
        offset = 0
        if 'cursor' in args:
            try:
                encoded, signature = args['cursor'].split('.')
                raw = base64.urlsafe_b64decode(encoded)
                if not hmac.compare_digest(signature, hmac.new(self.cursor_key, raw, hashlib.sha256).hexdigest()):
                    raise ValueError()
                cursor = json.loads(raw)
                if cursor['query'] != query_id or cursor['generation'] != generation:
                    raise ValueError()
                offset = cursor['offset']
            except (ValueError, KeyError, TypeError):
                raise MemoryError('invalid_argument') from None
        tokens = words(args['query'])
        clauses = ['scope IN (' + ','.join('?' for _ in permitted) + ')',
                   'kind IN (' + ','.join('?' for _ in args['kinds']) + ')',
                   "status IN ('active','contested')", '(expires IS NULL OR expires>?)']
        params = permitted + args['kinds'] + [int(self.clock())]
        if tokens:
            clauses.append('search_index MATCH ?')
            params.append(' OR '.join('"' + t + '"' for t in tokens))
            sql = 'SELECT records.doc FROM search_index JOIN records ON records.id=search_index.id WHERE '
            order = 'search_index.rank,records.id'
        else:
            sql = 'SELECT doc FROM records WHERE '
            order = 'id'
        rows = self.db.execute(sql + ' AND '.join(clauses) + ' ORDER BY ' + order + ' LIMIT 1001 OFFSET ?', params + [offset]).fetchall()
        items, conflicts = [], []
        used = 0
        consumed = 0
        # Conservative estimator: one UTF-8 byte per estimated token; never claims actual tokenizer accounting.
        cap = min(args['budget']['max_utf8_bytes'], args['budget']['estimated_tokens'])
        for row in rows:
            record = json.loads(row['doc'])
            fact = record.get('fact', {})
            if not fact.get('valid_from', 0) <= self.clock() < fact.get('valid_to', 9007199254740991):
                consumed += 1
                continue
            record['status'] = self._current_status(record)
            size = len(canonical(record).encode())
            if len(items) + len(conflicts) >= args['budget']['max_items']:
                break
            if size > cap:
                consumed += 1
                continue
            if used + size > cap:
                break
            (conflicts if record['status'] == 'contested' else items).append(record)
            used += size
            consumed += 1
        more = consumed < len(rows) or len(rows) == 1001
        next_cursor = None
        if more and consumed:
            raw = canonical({'query': query_id, 'generation': generation, 'offset': offset + consumed}).encode()
            next_cursor = base64.urlsafe_b64encode(raw).decode() + '.' + hmac.new(self.cursor_key, raw, hashlib.sha256).hexdigest()
        return {'items': items, 'conflicts': conflicts, 'next_cursor': next_cursor, 'truncated': more,
                'budget': {'utf8_bytes': used, 'estimator': 'utf8-byte-upper-bound', 'max_bytes': cap}}
