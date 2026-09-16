import json
from pathlib import Path
import tempfile
import threading
import unittest

from tekesmemory.common import MemoryError, canonical, digest, validate
from tekesmemory.schema import SCHEMAS
from tekesmemory.store import Store

SCOPE = {'kind': 'workspace', 'owner_id': 'u', 'workspace_id': 'ws'}
SOURCE = {'host': 'test', 'ref': 'test#1', 'digest': 'a' * 64}


def principal(role='model', identity=None):
    return {'id': identity or role, 'role': role, 'owner_id': 'u', 'scopes': [SCOPE],
            'tools': list(SCHEMAS) if role == 'adapter' else [k for k in SCHEMAS if k != 'memory.observe']}


def save_args(key='save', content='测试 Python preferences', **extra):
    return {'schema_version': 1, 'scope': SCOPE, 'kind': 'episode', 'content': content,
            'sources': [SOURCE], 'idempotency_key': key, **extra}


def search_args(query='', **extra):
    return {'schema_version': 1, 'scope': SCOPE, 'query': query, 'kinds': ['episode', 'fact', 'procedure'],
            'budget': {'max_items': 10, 'max_utf8_bytes': 32768, 'estimated_tokens': 8192}, **extra}


def observation(key='event', content='visible task completed', source_digest='b' * 64, **extra):
    return {'schema_version': 1, 'scope': SCOPE, 'idempotency_key': key,
            'source_event': {'host': 'test', 'host_instance_id': 'instance', 'workspace_id': 'ws',
                             'thread_id': 't', 'line_id': 'main', 'event_id': key,
                             'source_seq': 9, 'source_digest': source_digest},
            'observation': {'type': 'turn_outcome', 'payload': {'turn_id': 1, 'summary': content,
                              'sources': [SOURCE], 'outcome': 'completed'}}, **extra}


class StoreTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = Path(self.tmp.name) / 'memory.db'
        self.now = 1000
        self.store = Store(self.path, clock=lambda: self.now)
        self.p = principal()

    def tearDown(self):
        self.store.close()
        self.tmp.cleanup()

    def call(self, name, args, p=None):
        return self.store.call(p or self.p, 'memory.' + name, args)

    def assert_code(self, code, name, args, p=None):
        with self.assertRaises(MemoryError) as error:
            self.call(name, args, p)
        self.assertEqual(error.exception.code, code)

    def test_restart_and_exact_retry(self):
        args = save_args()
        saved = self.call('save', args)
        self.store.close()
        self.store = Store(self.path, clock=lambda: self.now)
        self.assertEqual(saved, self.call('save', args))
        item = self.call('get', {'schema_version': 1, 'scope': SCOPE, 'id': saved['id']})['item']
        self.assertEqual(item['content'], args['content'])
        self.assert_code('idempotency_conflict', 'save', save_args(content='changed'))

    def test_scope_and_operation_isolation(self):
        saved = self.call('save', save_args())
        foreign = SCOPE | {'workspace_id': 'other'}
        self.assert_code('scope_denied', 'search', search_args(scope=foreign))
        self.assert_code('scope_denied', 'get', {'schema_version': 1, 'scope': foreign, 'id': saved['id']})
        self.assert_code('not_found', 'status', {'schema_version': 1, 'operation_id': saved['operation_id']}, principal(identity='other'))
        self.assert_code('scope_denied', 'search', search_args(scope=SCOPE | {'thread_id': 't'}))

    def test_model_cannot_forge_observation_even_with_tool_grant(self):
        model = self.p | {'tools': list(SCHEMAS)}
        self.assert_code('scope_denied', 'observe', observation(), model)

    def test_cjk_lexical_retrieval_and_budget(self):
        self.call('save', save_args(content='运行项目测试使用 unittest'))
        result = self.call('search', search_args('项目测试'))
        self.assertEqual(len(result['items']), 1)
        small = search_args('项目测试', budget={'max_items': 1, 'max_utf8_bytes': 256, 'estimated_tokens': 64})
        self.assertEqual(self.call('search', small)['items'], [])

    def test_expiry_without_maintenance_and_pin(self):
        self.call('save', save_args(expires_at=1010))
        pinned = self.call('save', save_args('pin', expires_at=1010, pinned=True))
        self.now = 1011
        self.assertEqual([r['id'] for r in self.call('search', search_args())['items']], [pinned['id']])

    def test_revision_race_and_history(self):
        saved = self.call('save', save_args())
        args = {'schema_version': 1, 'scope': SCOPE, 'id': saved['id'], 'expected_revision': 1,
                'replacement': 'new', 'sources': [SOURCE], 'idempotency_key': 'correction'}
        outcomes = []
        def run(key):
            try:
                outcomes.append(self.call('correct', args | {'idempotency_key': key})['revision'])
            except MemoryError as error:
                outcomes.append(error.code)
        threads = [threading.Thread(target=run, args=(str(i),)) for i in range(2)]
        for t in threads: t.start()
        for t in threads: t.join()
        self.assertCountEqual(outcomes, [2, 'revision_conflict'])
        old = self.call('get', {'schema_version': 1, 'scope': SCOPE, 'id': saved['id'], 'revision': 1})['item']
        self.assertTrue(old['historical'])

    def test_conflicts_not_arbitrary_winner(self):
        for key, value in [('one', 'Python'), ('two', 'Rust')]:
            self.call('save', save_args(key, kind='fact', content=value,
                fact={'subject': 'user', 'predicate': 'prefers', 'value': value}))
        result = self.call('search', search_args())
        self.assertEqual(result['items'], [])
        self.assertEqual(len(result['conflicts']), 2)

    def test_time_and_environment_disambiguate(self):
        self.call('save', save_args('one', kind='fact', fact={'subject': 'api', 'predicate': 'version', 'value': '1', 'valid_to': 1000}))
        self.call('save', save_args('two', kind='fact', fact={'subject': 'api', 'predicate': 'version', 'value': '2', 'valid_from': 1000}))
        result = self.call('search', search_args())
        self.assertEqual(len(result['items']), 1)
        self.assertEqual(result['conflicts'], [])

    def test_delete_purges_all_versions_and_fts(self):
        saved = self.call('save', save_args(content='UNIQUE_PRIVATE_CONTENT'))
        self.call('correct', {'schema_version': 1, 'scope': SCOPE, 'id': saved['id'], 'expected_revision': 1,
                            'replacement': 'new private content', 'sources': [SOURCE], 'idempotency_key': 'update'})
        result = self.call('forget', {'schema_version': 1, 'scope': SCOPE, 'ids': [saved['id']],
                                     'expected_revisions': [2], 'idempotency_key': 'forget'})
        self.assertTrue(result['hidden'])
        self.assert_code('not_found', 'get', {'schema_version': 1, 'scope': SCOPE, 'id': saved['id'], 'revision': 1})
        self.assertEqual(self.call('search', search_args())['items'], [])
        self.store.maintain()
        dump = '\n'.join(self.store.db.iterdump())
        self.assertNotIn('UNIQUE_PRIVATE_CONTENT', dump)
        self.assertNotIn('new private content', dump)

    def test_observation_durable_queue_replay_and_business_dedup(self):
        adapter = principal('adapter')
        first = self.call('observe', observation(), adapter)
        self.store.close()
        self.store = Store(self.path, clock=lambda: self.now)
        again = self.call('observe', observation('changed-binding'), adapter)
        self.assertEqual(first['observation_id'], again['observation_id'])
        self.store.maintain()
        self.assertEqual(len(self.call('search', search_args())['items']), 1)
        status = self.call('status', {'schema_version': 1, 'operation_id': first['operation_id']}, adapter)
        self.assertEqual(status['processing_state'], 'completed')
        self.assertFalse(self.store.maintain())

    def test_source_revision_invalidates_old_text(self):
        adapter = principal('adapter')
        self.call('observe', observation(content='OLD_SOURCE'), adapter)
        self.store.maintain()
        self.call('observe', observation('new', content='NEW_SOURCE', source_digest='c' * 64), adapter)
        self.store.maintain()
        items = self.call('search', search_args())['items']
        self.assertEqual(len(items), 1)
        self.assertIn('NEW_SOURCE', items[0]['content'])
        self.assertNotIn('OLD_SOURCE', '\n'.join(self.store.db.iterdump()))

    def test_deleted_observation_cannot_resurrect(self):
        adapter = principal('adapter')
        self.call('observe', observation(), adapter)
        self.store.maintain()
        record = self.call('search', search_args())['items'][0]
        self.call('forget', {'schema_version': 1, 'scope': SCOPE, 'ids': [record['id']],
                             'expected_revisions': [1], 'idempotency_key': 'delete'})
        self.call('observe', observation('another-binding'), adapter)
        self.store.maintain()
        self.assertEqual(self.call('search', search_args())['items'], [])
        self.assert_code('source_unavailable', 'observe', observation('revision', source_digest='d' * 64), adapter)

    def test_source_invalidation_before_job_runs(self):
        adapter = principal('adapter')
        original = self.call('observe', observation(), adapter)
        invalid = observation('invalidate', source_digest='e' * 64)
        invalid['source_event']['source_seq'] = 10
        invalid['observation'] = {'type': 'source_invalidated', 'payload': {
            'turn_id': 1, 'summary': '', 'sources': [SOURCE], 'source_keys': [original['source_key']]}}
        self.call('observe', invalid, adapter)
        self.store.maintain()
        self.assertEqual(self.call('search', search_args())['items'], [])

    def test_schema_errors_and_secret_transform(self):
        self.assert_code('invalid_argument', 'save', save_args(extra=True))
        self.assert_code('unsupported_schema_version', 'save', save_args(schema_version=2))
        self.call('save', save_args(content='api_key=very-secret Bearer abcdefgh'))
        content = self.call('search', search_args())['items'][0]['content']
        self.assertNotIn('very-secret', content)
        self.assertNotIn('abcdefgh', content)

    def test_transaction_rollback_never_publishes_operation(self):
        original = self.store._save
        def fail(args):
            original(args)
            raise RuntimeError('simulated failure before commit')
        self.store._save = fail
        with self.assertRaises(RuntimeError):
            self.call('save', save_args())
        self.store._save = original
        self.assertEqual(self.call('search', search_args())['items'], [])
        self.assertEqual(self.store.db.execute('SELECT count(*) FROM operations').fetchone()[0], 0)
        self.call('save', save_args())

    def test_cursor_tampering_and_stale_generation(self):
        for i in range(3): self.call('save', save_args(str(i)))
        args = search_args(budget={'max_items': 1, 'max_utf8_bytes': 32768, 'estimated_tokens': 8192})
        first = self.call('search', args)
        next_page = self.call('search', args | {'cursor': first['next_cursor']})
        self.assertNotEqual(first['items'][0]['id'], next_page['items'][0]['id'])
        self.assert_code('invalid_argument', 'search', args | {'cursor': first['next_cursor'] + 'x'})
        self.call('save', save_args('four'))
        self.assert_code('invalid_argument', 'search', args | {'cursor': first['next_cursor']})

    def test_pending_source_revision_cancels_old_job(self):
        adapter = principal('adapter')
        self.call('observe', observation(content='OLD_PENDING'), adapter)
        self.call('observe', observation('new', content='NEW_PENDING', source_digest='c' * 64), adapter)
        while self.store.maintain():
            pass
        records = self.call('search', search_args())['items']
        self.assertEqual(len(records), 1)
        self.assertIn('NEW_PENDING', records[0]['content'])
        self.assertNotIn('OLD_PENDING', '\n'.join(self.store.db.iterdump()))

    def test_reused_source_digest_with_different_payload_is_rejected(self):
        adapter = principal('adapter')
        self.call('observe', observation(), adapter)
        self.assert_code('idempotency_conflict', 'observe', observation('new', content='different'), adapter)

    def test_procedure_is_versioned_data_not_execution(self):
        procedure = {'preconditions': ['clean tree'], 'steps': ['run tests'],
                     'success_criteria': 'tests pass', 'verified_environment': 'fixture', 'failure_modes': []}
        saved = self.call('save', save_args(kind='procedure', procedure=procedure))
        self.call('correct', {'schema_version': 1, 'scope': SCOPE, 'id': saved['id'], 'expected_revision': 1,
                             'replacement': 'updated method', 'sources': [SOURCE], 'procedure': procedure,
                             'idempotency_key': 'procedure-update'})
        self.assertEqual(self.call('get', {'schema_version': 1, 'scope': SCOPE, 'id': saved['id']})['item']['revision'], 2)

    def test_expiration_removes_stored_body_and_history(self):
        saved = self.call('save', save_args(content='EXPIRED_BODY_MARKER', expires_at=1010))
        self.now = 1011
        self.store.maintain()
        self.assertNotIn('EXPIRED_BODY_MARKER', '\n'.join(self.store.db.iterdump()))
        self.call('forget', {'schema_version': 1, 'scope': SCOPE, 'ids': [saved['id']],
                             'expected_revisions': [1], 'idempotency_key': 'expired-delete'})

    def test_old_revision_source_invalidation_removes_history(self):
        adapter = principal('adapter')
        source = self.call('observe', observation(), adapter)
        self.store.maintain()
        record = self.call('search', search_args())['items'][0]
        self.call('correct', {'schema_version': 1, 'scope': SCOPE, 'id': record['id'], 'expected_revision': 1,
                             'replacement': 'new source', 'sources': [SOURCE], 'idempotency_key': 'revise'})
        invalid = observation('invalidate', source_digest='e' * 64)
        invalid['source_event']['source_seq'] = 10
        invalid['observation'] = {'type': 'source_invalidated', 'payload': {'turn_id': 1, 'summary': '', 'sources': [SOURCE], 'source_keys': [source['source_key']]}}
        self.call('observe', invalid, adapter)
        self.assert_code('not_found', 'get', {'schema_version': 1, 'scope': SCOPE, 'id': record['id'], 'revision': 1})

    def test_expired_temporal_conflict_does_not_taint_current_fact(self):
        self.call('save', save_args('old', kind='fact', fact={'subject': 'api', 'predicate': 'version', 'value': '1', 'valid_to': 1100}))
        self.call('save', save_args('new', kind='fact', fact={'subject': 'api', 'predicate': 'version', 'value': '2', 'valid_from': 1000}))
        self.assertEqual(len(self.call('search', search_args())['conflicts']), 2)
        self.now = 1101
        result = self.call('search', search_args())
        self.assertEqual(result['conflicts'], [])
        self.assertEqual(len(result['items']), 1)
        self.assertEqual(result['items'][0]['fact']['value'], '2')

    def test_future_database_version_refused(self):
        self.store.db.execute('PRAGMA user_version=2')
        with self.assertRaises(MemoryError):
            Store(self.path)


if __name__ == '__main__':
    unittest.main()
