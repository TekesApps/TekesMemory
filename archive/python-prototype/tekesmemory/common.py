"""Closed wire validation, safe serialization and local identity checks."""
import hashlib
import json
import os
from pathlib import Path
import re
import stat

MAX_BODY = 2 * 1024 * 1024
PROTOCOL = "2025-11-25"


class MemoryError(Exception):
    def __init__(self, code, message=None):
        self.code = code
        super().__init__(message or code)


def canonical(value):
    # Application digests use this encoding, not arbitrary incoming JSON bytes.
    # Wire values exclude floats; UTF-16 ordering matches RFC 8785 for keys.
    def ordered(v):
        if isinstance(v, dict):
            return {k: ordered(v[k]) for k in sorted(v, key=lambda k: k.encode('utf-16-be'))}
        if isinstance(v, list):
            return [ordered(x) for x in v]
        return v
    return json.dumps(ordered(value), ensure_ascii=False, separators=(',', ':'), allow_nan=False)


def digest(value):
    return hashlib.sha256(canonical(value).encode()).hexdigest()


def decode(raw):
    def pairs(items):
        result = {}
        for k, v in items:
            if k in result:
                raise ValueError('duplicate key')
            result[k] = v
        return result
    try:
        value = json.loads(raw, object_pairs_hook=pairs,
                           parse_constant=lambda _: (_ for _ in ()).throw(ValueError()))
        canonical(value).encode('utf-8')
        return value
    except (ValueError, UnicodeError, TypeError, RecursionError):
        raise MemoryError('invalid_argument') from None


def private_json(path):
    path = Path(path)
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise MemoryError('unsafe_config_permissions')
        with os.fdopen(fd, 'rb', closefd=False) as stream:
            data = stream.read(MAX_BODY + 1)
        if len(data) > MAX_BODY:
            raise MemoryError('budget_exceeded')
        return decode(data)
    finally:
        os.close(fd)


def scrub(value):
    """Conservative best-effort transform; never a universal secret detector."""
    if isinstance(value, dict):
        return {k: ('[REDACTED]' if re.search(
            r'(?i)(password|secret|credential|authorization|api[_-]?key|access[_-]?token|reasoning|thinking)', k)
                    else scrub(v)) for k, v in value.items()}
    if isinstance(value, list):
        return [scrub(v) for v in value]
    if isinstance(value, str):
        value = re.sub(r'(?is)-----BEGIN [^-]*PRIVATE KEY-----.*?-----END [^-]*PRIVATE KEY-----', '[REDACTED]', value)
        value = re.sub(r'(?i)\bBearer\s+[A-Za-z0-9._~+/-]+=*', 'Bearer [REDACTED]', value)
        value = re.sub(r'\b(?:sk-[A-Za-z0-9_-]{12,}|gh[pousr]_[A-Za-z0-9]{16,})\b', '[REDACTED]', value)
        value = re.sub(r'(?i)((?:password|api[_-]?key|secret|access[_-]?token)\s*[:=]\s*)[^\s,;]+', r'\1[REDACTED]', value)
    return value


def validate(value, schema):
    typ = schema.get('type')
    valid = {'object': isinstance(value, dict), 'array': isinstance(value, list),
             'string': isinstance(value, str), 'integer': type(value) is int,
             'boolean': type(value) is bool, 'null': value is None}
    if typ and not valid.get(typ, False):
        raise MemoryError('invalid_argument')
    if 'enum' in schema and value not in schema['enum']:
        raise MemoryError('invalid_argument')
    if 'const' in schema and value != schema['const']:
        raise MemoryError('unsupported_schema_version' if schema['const'] == 1 else 'invalid_argument')
    if typ == 'object':
        props = schema.get('properties', {})
        if set(schema.get('required', [])) - value.keys():
            raise MemoryError('invalid_argument')
        if not schema.get('additionalProperties', True) and value.keys() - props.keys():
            raise MemoryError('invalid_argument')
        for k, v in value.items():
            if k in props:
                validate(v, props[k])
    if typ == 'array':
        if not schema.get('minItems', 0) <= len(value) <= schema.get('maxItems', 10000):
            raise MemoryError('invalid_argument')
        for item in value:
            validate(item, schema.get('items', {}))
    if typ == 'string':
        if not schema.get('minLength', 0) <= len(value) <= schema.get('maxLength', MAX_BODY):
            raise MemoryError('invalid_argument')
        if 'pattern' in schema and not re.fullmatch(schema['pattern'], value):
            raise MemoryError('invalid_argument')
    if typ == 'integer' and not schema.get('minimum', -9007199254740991) <= value <= schema.get('maximum', 9007199254740991):
        raise MemoryError('invalid_argument')


def authorize(principal, scope):
    kind = scope['kind']
    expected = {'kind', 'owner_id'} | ({'workspace_id'} if kind != 'user' else set()) | ({'thread_id'} if kind == 'thread' else set())
    if set(scope) != expected or scope['owner_id'] != principal['owner_id']:
        raise MemoryError('scope_denied')
    for grant in principal['scopes']:
        if grant == scope or (grant.get('kind') == 'workspace' and kind == 'thread'
                              and grant.get('owner_id') == scope['owner_id']
                              and grant.get('workspace_id') == scope['workspace_id']):
            return
    raise MemoryError('scope_denied')
