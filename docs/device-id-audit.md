# Consumers of `device_id`

Why this exists: at the device cap, a login in `payserver-commons` now attaches
to the least-recently-used active device instead of being refused
(`auth/src/service/device.rs`, `device_for_new_login`). Two machines can
therefore hold sessions with the same `device_id`. Commons reaches this
repository only when the pinned `rev` moves, so before it moves we need to know
whether anything treats a device id as identifying one machine, or as a reason
to trust a request.

Checked against: the pin in `Cargo.toml` (`db03bd4`, which does **not** yet
contain the least-recently-used behaviour; verified by reading that checkout's
`device.rs`) and the commons working tree that does. Every site below was read,
not inferred from its name. `payserver-client` and `payserver-billing` contain
no reference to `device_id` / `DeviceId`.

## Verdict

**No consumer carries trust on a device id.** There is no remembered-device
check, no 2FA or re-authentication skip, no refresh token, and no session
binding keyed on it. A session is authenticated by `session_id` alone
(`validate_session`: expiry, idle timeout, user exists). The device id is never
compared to anything the caller proves. Nothing here blocks the rev move.

Two behavioural consequences of sharing (not trust, but worth knowing) are in
"Effects of sharing" below.

## Commons: reads and writes

| Site | Access | Assumption |
| -- | -- | -- |
| `auth/src/models.rs` `DeviceId`, `Device.id` | type / PK | Opaque UUID naming a *record*. |
| `models.rs` `Session.device_id`, `Session::new`, `with_expiration` | write at session creation | Records which device row the session was issued under. Informational. |
| `service/passkey.rs` register (≈L152-166), `service/wallet.rs` register (≈L438-452), `service/recovery.rs` (≈L287-298) | write | Fresh device minted at registration / recovery; id returned in `LoginResponse`. Always unique here. |
| `service/passkey.rs` login (≈L300-330), `service/wallet.rs` login (≈L285-314) | read `request.device_id`, write session | Client-supplied id is a **hint** only, consulted *after* the credential is verified. |
| `service/device.rs` `reusable_device` | read | Accepts the hint only if the row exists, belongs to the same user and `is_active`; otherwise ignored. Grants nothing: a valid hint just picks which record to stamp `last_used_at` on. |
| `service/device.rs` `device_for_new_login` | read/write | Below cap: new row. At cap: reuses LRU active row, overwrites its name/type/key copy/`last_used_at`. This is the change that makes ids shared. |
| `service/device.rs` `get_devices` | read | Lists active devices of the *session's* user. |
| `service/device.rs` `revoke_device` | read | Refuses when `session.device_id == device_id`; checks the target belongs to the caller's user; then deletes that device's sessions and deactivates it. Authorised by the caller's session, not by the device id. |
| `repository.rs` `get_device`, `update_device`, `deactivate_device`, `delete_device`, `delete_sessions_for_device`, in-memory impl | storage | Plain keyed access. |
| `api/management.rs` `list_devices`, `revoke_device` (`/auth/devices`, `/auth/devices/{id}`) | HTTP | Path id is a target to revoke, authorised by session. |
| `ui-kit/src/auth/session.rs` `DEVICE_ID_KEY` (`ps_device_id`, localStorage), `get_device_id`, `login_page.rs` | client read/write | Browser stores the id from the last `LoginResponse` and resends it as the hint. Never sent as a credential. |
| `ui-kit/src/auth/types.rs` `LoginRequest.device_id`, `StoredSession.device_id` | DTO | Round-trips the id. |

## ethpayserver: reads and writes

| Site | Access | Assumption |
| -- | -- | -- |
| `data-service/migrations/postgres/20241214000001_create_auth_tables.sql` `sessions.device_id` (FK to `devices`, `ON DELETE CASCADE`), `idx_sessions_device_id` | schema | Many sessions may reference one device; nothing requires one-to-one. No unique constraint on it. |
| `data-service/src/postgres/auth/session.rs` create / select (L20-43, L136-154), `delete_sessions_for_device` (L96) | read/write | Persists and loads the column; the delete is by device id. |
| `data-service/src/postgres/auth/device.rs` | read/write | CRUD by `id` / `user_id`. `update_device` rewrites name, type, key copy, `last_used_at`, `is_active` - exactly what the LRU path calls. |
| `server/src/api/**` | none | No handler reads `session.device_id`. Sessions are consumed only for the user id and role. (`server/src/api/plugins.rs`, `refunds/tests.rs`, `tests/ws_store_scope.rs` construct sessions with arbitrary ids as test fixtures.) |
| `server/tests/standing_warning_header.rs` | test fixture | Inserts a session row directly. |
| `data-service/src/postgres/auth/{tests,integration_tests}.rs` | tests | Assert a session round-trips its device id. They would not notice sharing. |

## Effects of sharing (not trust, but visible)

1. **Revoking a shared device signs out every machine on it.**
   `revoke_device` deletes all sessions for the id. Correct for the record, but
   the device list shows one entry for what are now two machines, so a user
   revoking "Chrome on MacBook" also ends the other machine's session.
2. **The device list's name, type and `last_used_at` describe only the most
   recent login** (the reclaimed record takes the new name). Any audit or
   display that reads them as a stable machine identity will be wrong at the cap.

Neither is reachable from a request in this repository today.

## Not covered

Anything outside the three public repositories and `payserver-billing` (deploy
config, dashboards, log queries that group by `device_id`) was not searched.
Commons logs `device_id` at `info` on reuse; nothing here parses those lines.
