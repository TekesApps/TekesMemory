# Linux synthetic provider readiness acceptance

Refs #13; coordinated with [Backend #205](https://github.com/ShuKangZhiYi/SK-Backend/issues/205) and [Chat #51](https://github.com/ShuKangZhiYi/SK-Health-Chat/issues/51). Local acceptance only; no merge or deployment.

The Linux Chat regression exposed two `JSONDecodeError` setup failures: `test_real_resource_policy_blocks_owner_write_and_bulk_privacy_delete` and `test_read_queue_insert_failure_refuses_content`. The provider opened `ready.json` before serializing its contents, while the consumer treated file existence as readiness. A reader could therefore observe an empty file.

The fixture now writes `ready.json.tmp`, closes it, and publishes `ready.json` with a same-directory `os.replace`. Product code, protocol, Cargo.lock, consumer assertions and timeouts are unchanged. This is test-fixture readiness, not a new production persistence mechanism.

## Verification

- Deterministic old/new check: an external diagnostic wrapper pauses `json.dump` before writing. With the old fixture, `ready.json` exists and parsing raises `JSONDecodeError`; with the patched fixture, only the temporary file exists. After releasing the gate, the final JSON contains the valid port and the listening socket responds. One run of each variant; both owned processes cleaned up.
- The two original failing Chat tests pass unchanged: 2 tests, 0 skips, 3.327 seconds in Rocky Linux 9 amd64 / glibc 2.34 / CPython 3.12.13 under local Colima/Rosetta. This is not native production performance evidence.
- Source SHA256 before: `7f4569cd1a2599185fb5273091bf9208422dffba5613004cd5a1032ef57104de`; after: `8299a4c62e9bee64f18a404fc1c6271437d38b68470cd4d75a4964b92f62f883`.

Private evidence: `/Users/frankqin/dev/SKZY/review-artifacts/chat-api-linux-20261118/linux/provider-ready-red-green.json`, its adjacent diagnostic script/log, and the Chat focused retry log. The aggregate Chat full-suite results are tracked separately in Backend #205; the two-case receipt alone does not claim the whole suite passes.

The previously built Rust release binary remains byte-for-byte unchanged: SHA256 `8e1c06c752021ec2b682b955d335c03abf69a0cc74d7f1ed577a8f6f93a00caf`, built from `c8c7acbdfeb3807cfcc9620638055ebad15139ba` with Rust 1.95.0 and `cargo build --locked --release --bin tekes-memory -j2`. This patch only changes the Python test support and this record; it does not require a new product version or a new binary.
