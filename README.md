# ETHPayServer

A self-hosted Ethereum and EVM-networks payment processor built in Rust.

Accept ETH and ERC20 tokens (USDC, USDT, DAI, WBTC, etc.) across 50+ EVM-compatible chains with a single codebase.

## Overview

ETHPayServer is a free, open-source payment processor that enables merchants to accept cryptocurrency payments on Ethereum and all EVM-compatible chains (Polygon, Arbitrum, Optimism, Base, BSC, and more).

### Key Features

- **Multi-Chain Support** - Works across 12+ EVM mainnets and 8+ testnets
- **Native + Token Payments** - Accept ETH and whitelisted ERC20 tokens
- **Payment Monitoring** - Real-time detection via WebSocket subscriptions
- **Reorg Protection** - Handles blockchain reorganizations safely
- **Horizontal Scaling** - Separate monitor binary with Redis event bridge
- **Direct RPC Support** - Connect to your own nodes or use providers (Alchemy, Infura)
- **Testnet Support** - Full testnet support for development and testing

## Quick Start (Local Development)

### Prerequisites

- Rust (pinned via `rust-toolchain.toml`)
- PostgreSQL 14+
- Redis 7+
- Ethereum RPC access (Alchemy, Infura, or self-hosted node)

### 1. Clone and Setup

```bash
# Clone the repository
git clone git@github.com:gcannelloni/ethpayserver.git
cd ethpayserver
```

### 2. Start Services

```bash
# Copy and configure environment (see "Docker" below for details)
cp docker/.env.example docker/.env

# Start PostgreSQL and Redis with docker-compose
docker compose -f docker/docker-compose.local.yml up -d

# Verify services are running
docker compose -f docker/docker-compose.local.yml ps
```

### 3. Database Setup

```bash
# Install sqlx-cli
cargo install sqlx-cli --no-default-features --features postgres

# Run migrations (database is auto-created by docker-compose)
DATABASE_URL="postgres://postgres:postgres@localhost/ethpayserver" \
sqlx migrate run --source data-service/migrations/postgres
```

### 4. Run the API Server

```bash
# Set environment variables
export DATABASE_URL="postgres://postgres:postgres@localhost/ethpayserver"
export HOST="127.0.0.1"
export PORT="3000"
export RUST_LOG="info"
export ENABLE_SWAGGER="true"

# Run the server
cargo run --release --bin ethpayserver
```

The API server will be available at `http://localhost:3000` with Swagger UI at `/swagger-ui`.

### 5. Run the Payment Monitor

The payment monitor is a separate binary that watches blockchain activity and publishes events to Redis.

```bash
# Build the monitor
cargo build --release --bin evmmonitor --features monitor-bin

# Run on mainnet (example with Ethereum)
EVMMONITOR_REDIS_URL=redis://localhost:6379 \
EVMMONITOR_CHAINS=1 \
EVMMONITOR_CHAIN_1_RPC_HTTP=https://eth-mainnet.g.alchemy.com/v2/YOUR_KEY \
EVMMONITOR_CHAIN_1_RPC_WS=wss://eth-mainnet.g.alchemy.com/v2/YOUR_KEY \
RUST_LOG=info \
./target/release/evmmonitor
```

#### Running on Testnet (Sepolia)

```bash
EVMMONITOR_REDIS_URL=redis://localhost:6379 \
EVMMONITOR_CHAINS=11155111 \
EVMMONITOR_CHAIN_11155111_RPC_HTTP=https://sepolia.infura.io/v3/YOUR_KEY \
EVMMONITOR_CHAIN_11155111_RPC_WS=wss://sepolia.infura.io/ws/v3/YOUR_KEY \
RUST_LOG=info \
./target/release/evmmonitor
```

#### Multi-Chain Configuration

```bash
# Monitor multiple chains simultaneously
EVMMONITOR_REDIS_URL=redis://localhost:6379 \
EVMMONITOR_CHAINS=1,137,42161 \
EVMMONITOR_CHAIN_1_RPC_HTTP=https://eth-mainnet.g.alchemy.com/v2/KEY \
EVMMONITOR_CHAIN_1_RPC_WS=wss://eth-mainnet.g.alchemy.com/v2/KEY \
EVMMONITOR_CHAIN_137_RPC_HTTP=https://polygon-mainnet.g.alchemy.com/v2/KEY \
EVMMONITOR_CHAIN_137_RPC_WS=wss://polygon-mainnet.g.alchemy.com/v2/KEY \
EVMMONITOR_CHAIN_42161_RPC_HTTP=https://arb-mainnet.g.alchemy.com/v2/KEY \
EVMMONITOR_CHAIN_42161_RPC_WS=wss://arb-mainnet.g.alchemy.com/v2/KEY \
./target/release/evmmonitor
```

Or use a config file (`evmmonitor.toml`):

```toml
[bridge]
redis_url = "redis://localhost:6379"

[[chains]]
chain_id = 1
rpc_http = "https://eth-mainnet.g.alchemy.com/v2/KEY"
rpc_ws = "wss://eth-mainnet.g.alchemy.com/v2/KEY"

[[chains]]
chain_id = 137
rpc_http = "https://polygon-mainnet.g.alchemy.com/v2/KEY"
rpc_ws = "wss://polygon-mainnet.g.alchemy.com/v2/KEY"
```

```bash
./target/release/evmmonitor --config evmmonitor.toml
```

## Supported Networks

### Mainnets

| Network | Chain ID | Native Token |
|---------|----------|--------------|
| Ethereum | 1 | ETH |
| Polygon | 137 | POL |
| Arbitrum | 42161 | ETH |
| Optimism | 10 | ETH |
| Base | 8453 | ETH |
| Avalanche | 43114 | AVAX |
| BNB Smart Chain | 56 | BNB |
| zkSync Era | 324 | ETH |
| Linea | 59144 | ETH |
| Scroll | 534352 | ETH |
| Fantom | 250 | FTM |
| Gnosis | 100 | xDAI |

### Testnets

| Network | Chain ID | Native Token |
|---------|----------|--------------|
| Sepolia | 11155111 | ETH |
| Holesky | 17000 | ETH |
| Hoodi | 560048 | ETH |
| Polygon Amoy | 80002 | POL |
| Arbitrum Sepolia | 421614 | ETH |
| Optimism Sepolia | 11155420 | ETH |
| Base Sepolia | 84532 | ETH |
| Avalanche Fuji | 43113 | AVAX |
| BSC Testnet | 97 | tBNB |

### Supported Tokens

Native tokens on every chain above, plus per-chain ERC20 tokens seeded into the
`tokens` table by migration (`data-service/migrations/postgres/`) — stablecoins
and, where unambiguous, WETH/WBTC, each address checked against the issuer's
or chain operator's own published list rather than a block explorer search.
Query `GET /evm/tokens` for the current list on a running instance, or read
the seed migrations for the addresses and their sourcing. A store can further
restrict which of those tokens it accepts with an allowlist or blocklist
(`PUT /stores/{id}/token-policy`).

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                     ETHPayServer                             │
├─────────────────────────────────────────────────────────────┤
│  API Server (ethpayserver binary)                            │
│  ├── /health     - Health checks                             │
│  ├── /stores     - Store management                          │
│  ├── /invoices   - Invoice CRUD                              │
│  ├── /auth       - Authentication (passkey, wallet, BIP39)   │
│  ├── /evm        - EVM operations (tokens, networks)         │
│  └── /swagger-ui - API documentation                         │
├─────────────────────────────────────────────────────────────┤
│  Background Services                                         │
│  ├── EventConsumer   - Processes events from Redis           │
│  ├── ExpirationSvc   - Expires old invoices                  │
│  ├── CleanupService  - Unwatches completed addresses         │
│  ├── WatchRetryService - Retries failed watch commands       │
│  └── WebhookService  - Delivers webhook notifications        │
├─────────────────────────────────────────────────────────────┤
│  Data Layer (PostgreSQL)                                     │
│  └── PgDataService - Repository implementations              │
└─────────────────────────────────────────────────────────────┘
                              │
                              │ Redis pub/sub
                              ▼
┌─────────────────────────────────────────────────────────────┐
│  evmmonitor (separate binary - can run multiple instances)   │
│  ├── WebSocket subscriptions for real-time blocks            │
│  ├── ERC20 Transfer event detection                          │
│  ├── Native ETH transfer detection                           │
│  ├── Confirmation tracking                                   │
│  └── Reorg detection                                         │
└─────────────────────────────────────────────────────────────┘
                              │
                              │ RPC (HTTP + WebSocket)
                              ▼
┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐
│  Ethereum RPC   │  │  Polygon RPC    │  │  Arbitrum RPC   │
└─────────────────┘  └─────────────────┘  └─────────────────┘
```

## Project Structure

```
ethpayserver/
├── server/            # Main API server (ethpayserver binary)
├── evm/               # EVM blockchain interaction (evmmonitor binary)
├── data-service/      # PostgreSQL + Redis data access layer
├── mcp-server/        # MCP tools for AI agents (ethpay-mcp binary)
├── loadtest/          # Goose-based load test scenarios
├── e2e/               # Playwright suite, against the server + the client image
└── memos/             # Product spec and standing decisions (no session log)
```

The web frontend is **not** here. It lives in
[payserver-client](https://github.com/gcannelloni/payserver-client), because it
is meant to serve every payserver — EVM, Tron, Solana, Monero — rather than this
one. The image tag this server is **tested against** is pinned in
`ops/client-image.pin`. That is not the tag serving production: the
frontend deploys on its own cadence from payserver-client via
the private deploy repository, so the two can drift.

### Crates

| Crate | Binary | Description | README |
|-------|--------|-------------|--------|
| `server` | `ethpayserver` | Main API server with REST endpoints, Swagger UI, background services | [server/README.md](./server/README.md) |
| `evm` | `evmmonitor` | Chain abstraction, payment monitoring, HD wallet derivation | [evm/README.md](./evm/README.md) |
| `data-service` | - | PostgreSQL repositories, Redis persistence | [data-service/README.md](./data-service/README.md) |
| `mcp-server` | `ethpay-mcp` | Exposes invoice/payment operations as MCP tools for AI agents | - |
| `loadtest` | `loadtest`, `loadtest-ws` | Goose-based load test scenarios against a running server | - |

### External Dependencies (payserver-commons)

Shared libraries from [payserver-commons](https://github.com/gcannelloni/payserver-commons):

| Crate | Description |
|-------|-------------|
| `types` | Common types: `ChainId` (CAIP-2), `InvoiceData`, `PaymentData`, repository traits |
| `api-types` | The REST request/response shapes, shared with the frontend so the contract has one definition |
| `auth` | User authentication: passkeys, Ethereum wallets, BIP39 recovery |
| `crypto` | Cryptographic primitives: Argon2id, AES-256, X25519, Ed25519 |
| `rates` | Fiat/crypto exchange rate providers |
| `scrub` | Secret/PII redaction for error reports — see `evm::telemetry` |

`ui-kit` is also published there but is not used here: it is Leptos components,
consumed by [payserver-client](https://github.com/gcannelloni/payserver-client).

#### Which version you build against

**One revision, pinned in the root `Cargo.toml` under `[workspace.dependencies]`,
and recorded in `Cargo.lock`.** Every crate here inherits it with
`{ workspace = true }`; nothing declares its own git URL.

That pin is the whole contract: a build is reproducible from this repo alone, and
checking out an old commit builds the commons it was written against.

It did not always work that way. The workspace used to `[patch]` the crates to a
sibling directory, so the lock file recorded **no commons revision at all** —
which version you got was decided by CI shell logic (`git clone -b "$BRANCH" ||
git clone`) and by whatever `main` happened to be at that minute. Rebuilding last
week's commit silently picked up this week's commons, and a breaking change to a
shared crate broke `testnet` the moment it merged (2026-09-06). It also
meant a commons fix only reached testnet when something unrelated pushed here.

**Working on both repos at once**

Do not edit the pin for this. Link a local checkout:

```bash
scripts/commons.sh link            # sibling ../payserver-commons
scripts/commons.sh link /some/path # or anywhere
scripts/commons.sh status          # what is pinned, and whether a link is active
scripts/commons.sh unlink          # back to the pinned revision
```

`link` writes an **uncommitted** `.cargo/config.toml` — Cargo honours `[patch]`
in config files, so the committed manifest stays pinned while your working copy
builds against your checkout. It is gitignored; committing it would point
everyone's build at one person's disk.

**Landing a commons change**

1. Merge it in `payserver-commons` and note the SHA.
2. Here: `scripts/commons.sh pin <sha>` — it reads the crates currently
   pinned in `Cargo.toml` and prints the matching `cargo update -p ...`
   command to run, so the list can't drift out of sync with the manifest.
3. Commit `Cargo.toml` and `Cargo.lock` in the PR that needs it.

The bump is the same PR as the code that depends on it, so a breaking change to a
shared crate is reviewed and merged as one unit rather than racing across two
repositories. This is also how a commons fix reaches an environment: bump the
pin, which is a push here, so a deploy actually happens.

**Working on several things at once**

```bash
scripts/lane.sh <name>     # worktree of this repo + its own commons, linked
scripts/lane.sh --list
scripts/lane.sh --remove <name>
```

A lane is only needed when you want to **edit** commons in parallel: every
worktree otherwise shares the one `../payserver-commons` on one branch. A lane
that just builds the pinned revision needs nothing special — plain
`git worktree add` works now that nothing depends on directory layout.

## API Endpoints

A representative sample, not the full surface — the tables below predate
several route groups and stores has grown sub-resources (members, wallet,
settings, webhook, payment-methods, token-policy, payouts) not listed
individually. `/swagger-ui` on a running instance is the current, generated
source of truth; `server/src/api/mod.rs` is the router if you want it without
running anything.

Groups not broken out below: `/wallets` (top-level wallet CRUD, xpub export),
`/payments` (list/export detected payments), `/rates` (fiat/crypto quote),
`/dashboard` (stats, analytics), `/users` (account, API keys, sessions),
`/admin` (users, settings, safe mode, plugin management), `/checkout`
(public invoice view + WebSocket, no auth), `/plugins` (installed-plugin
pages; a `routes` segment is reserved but not mounted — see
[Plugins](#plugins)).

### Health

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/health` | Full health check with database status |
| GET | `/health/live` | Liveness probe |
| GET | `/health/ready` | Readiness probe (checks database) |

### Authentication

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/auth/passkey/new-user/start` | Start passkey registration |
| POST | `/auth/passkey/new-user/complete` | Complete passkey registration |
| POST | `/auth/passkey/login/start` | Start passkey login |
| POST | `/auth/passkey/login/complete` | Complete passkey login |
| POST | `/auth/wallet/new-user/start` | Start wallet registration |
| POST | `/auth/wallet/new-user/complete` | Complete wallet registration |
| POST | `/auth/wallet/login/start` | Start wallet login |
| POST | `/auth/wallet/login/complete` | Complete wallet login |
| POST | `/auth/recovery/start` | Start BIP39 recovery |
| POST | `/auth/recovery/complete` | Complete recovery |
| GET | `/auth/devices` | List user's devices |
| DELETE | `/auth/devices/{id}` | Revoke a device |
| POST | `/auth/logout` | Logout current session |

### Stores

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/stores` | List stores for authenticated user |
| POST | `/stores` | Create a new store |
| GET | `/stores/{id}` | Get store details |
| PUT | `/stores/{id}` | Update store |
| DELETE | `/stores/{id}` | Archive store |

### Invoices

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/invoices` | List invoices with filters |
| POST | `/invoices` | Create a new invoice |
| GET | `/invoices/{id}` | Get invoice details |
| POST | `/invoices/{id}/cancel` | Cancel a pending invoice |
| POST | `/invoices/{id}/refund` | Always refuses (501) — see [Refunds](#refunds) |
| GET | `/invoices/{id}/refunds` | List refund records for an invoice |

### EVM

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/evm/networks` | List supported networks |
| GET | `/evm/networks/{id}` | Get network details |
| GET | `/evm/tokens` | List tokens (admin) |
| POST | `/evm/tokens` | Create token (admin) |
| PUT | `/evm/tokens/{id}` | Update token (admin) |
| DELETE | `/evm/tokens/{id}` | Delete token (admin) |

## Refunds

**Refunds are the merchant's job, not this server's, in this release.**
ETHPayServer is non-custodial today: a merchant hands over an xpub, this
server derives payment addresses from it, and it never holds the matching
private key (see `evm::wallet::validate_xpub`, which rejects an xprv pasted
in by mistake). A server with no spending key cannot sign or broadcast a
transaction, refund included — that is expected to change when custody
support lands.

`POST /invoices/{id}/refund` reflects that: it always returns `501 Not
Implemented` and creates nothing. It used to write a `Pending` refund row that
nothing downstream ever moved past that status, which was worse than refusing
— it told a merchant a refund was in flight when none was, and could never be.
To refund a payer, send the funds back from the wallet that holds the
spending key: yours.

`GET /invoices/{id}/refunds` still lists any refund records associated with
an invoice for historical/audit purposes; going forward none will be created
through this API. Testnet was checked directly on 2026-09-14: zero rows of
any status existed there, so no migration or backfill was needed for existing
data — there was nothing to clean up. Mainnet needs no equivalent check: it
deploys only from a release tag matching `vMAJOR.MINOR.PATCH` exactly (see
`.github/workflows/ci.yml`'s `notify-deploy` job), this repository's only tag
is the prerelease `v0.1.0-alpha`, and it has consequently never received a
deploy dispatch — there is no mainnet deployment of the old refund code for
any row to exist in.

`evm::transaction` contains the signing/broadcasting infrastructure a refund
would need. It is reserved for a possible future hot-wallet mode, gated
behind the (default-off) `hot-wallet` feature on the `evm` crate — enabling it
would be a deliberate product decision, not something this endpoint should
grow into by accident.

## Redis Communication

The API server and evmmonitor communicate via Redis pub/sub:

### Channels

| Channel | Direction | Description |
|---------|-----------|-------------|
| `evmmonitor:commands` | API -> Monitor | Commands to watch/unwatch addresses |
| `evmmonitor:events` | Monitor -> API | Payment events, confirmations, reorgs |

### Commands (API Server -> Monitor)

```json
{"type": "watch_address", "chain_id": 1, "address": "0x...", "invoice_id": "uuid"}
{"type": "unwatch_address", "chain_id": 1, "address": "0x..."}
{"type": "get_status"}
```

### Events (Monitor -> API Server)

- `PaymentDetected` - Payment received (unconfirmed)
- `PaymentConfirmed` - Payment reached required confirmations
- `ReorgDetected` - Chain reorganization detected
- `AddressWatched` - Address monitoring started
- `AddressUnwatched` - Address removed from watch list

### Redis Keys

| Pattern | Description |
|---------|-------------|
| `evmwatch:addr:{chain_id}:{address}` | Watched address -> invoice_id mapping |

## Database Schema

### Migrations

Located in `data-service/migrations/postgres/`:

```bash
# Run migrations
sqlx migrate run --source data-service/migrations/postgres

# Revert last migration
sqlx migrate revert --source data-service/migrations/postgres
```

### Tables

Not exhaustive — WebAuthn ceremony-state tables (`passkey_*_challenges`,
`discoverable_authentication_challenges`, `wallet_challenges`,
`wallet_reauth_challenges`) and other bookkeeping tables are omitted below;
`data-service/migrations/postgres/` is the source of truth.

**Auth Tables:**
- `users` - User accounts
- `sessions` - Active sessions
- `devices` - Registered devices/passkeys
- `wallet_credentials` - Ethereum wallets linked for login
- `passkey_credentials` - Registered passkey public keys
- `api_keys` - Long-lived keys for programmatic access (e.g. `mcp-server`)
- `email_change_requests` - Pending email-change confirmations

**Wallet Tables:**
- `wallets` - Account receiving wallets: one xpub and its single
  derivation counter
- `wallet_rotations` - History of xpub rotations (see `docs/xpub-rotation.md`)

**Store Tables:**
- `stores` - Merchant stores
- `store_roles` - Role definitions (Owner, Manager, Employee, Guest)
- `user_stores` - User-store membership
- `store_wallets` - Per-store wallet override (absent = account primary)
- `store_webhooks` - Webhook configuration
- `store_settings`, `server_settings` - Per-store and instance-wide settings
- `store_payment_methods` - Enabled payment methods per store
- `store_token_policies`, `store_token_policy_entries` - Per-store token
  allowlist/blocklist (see [Supported Tokens](#supported-tokens))

**Payment Tables:**
- `invoices` - Payment invoices
- `payments` - Detected payments
- `payment_options` - Chain/token options presented for an invoice
- `payment_events` - Audit log
- `watched_addresses` - PostgreSQL persistence for watched addresses
- `tokens` - Configured ERC20 tokens
- `payouts` - Merchant payout records
- `refunds` - Historical refund records (see [Refunds](#refunds) — no longer written by this API)
- `webhook_deliveries` - Webhook delivery attempts and payloads

**Plugin Tables:**
- `installed_plugins` - Plugins installed per store (see [Plugins](#plugins))
- `plugin_events` - Audit log of plugin lifecycle events

**Chain Tables:**
- `chain_configs` - Per-chain configuration overrides

## Telemetry

Both binaries can forward panics, errors, structured logs and Prometheus
metrics to a Sentry-protocol backend, in addition to the `/metrics` endpoint
above:

- `SENTRY_DSN` — collector endpoint. Leaving it unset disables reporting, but
  only boots that way when `SENTRY_ENVIRONMENT` is exactly `testnet` or
  `dev`; any other value, including an unset `SENTRY_ENVIRONMENT`, refuses to
  start without a DSN. `docker/.env.example` ships `SENTRY_ENVIRONMENT=dev`
  uncommented for exactly this reason — remove it and a DSN-less `cargo run`
  will not start. Confirmed: copying `docker/.env.example` to `docker/.env`
  verbatim still resolves cleanly under `docker compose config`.
- `SENTRY_LOG_LEVEL` — minimum level forwarded as a structured log event
  (default `WARN`); independent of `RUST_LOG`, which only controls what is
  printed locally.
- `SENTRY_RELEASE` — set at build time from the commit SHA; used to associate
  errors with the build that produced them.

Every event and log line passes through `evm::telemetry` first, which redacts
wallet/private keys, mnemonics, JWTs, bearer tokens, emails, on-chain
addresses/hashes and RPC provider URLs (the API key embedded in an
Alchemy/Infura/QuickNode path) before anything leaves the process. The same
rules exist a second time, hand-written with no dependencies, in
`payserver-commons`' `scrub` crate for the browser client —
`evm::telemetry::tests::parity_with_shared_scrubber` runs both over the same
corpus and asserts byte-identical output.

The Leptos/WASM client reports to the same backend independently, via the
`telemetry-dsn` / `telemetry-environment` meta tags in payserver-client's
`index.html` — it is not configured from this repo.

## Plugins

A plugin host (`payserver-plugin-host`, in payserver-commons) loads
merchant-installed plugins as wasmtime modules. Of the two things a manifest
can declare, only one is live: `pages` — a plugin's declared page tree — are
served today at `GET /plugins/{id}/pages/{path}`, mounted directly in
`server/src/api/mod.rs` and requiring the same `AuthenticatedUser` extractor
every core handler uses.

A plugin's own `routes` are reserved a separate segment
(`/plugins/{id}/routes/...`, kept apart from `pages` so neither can shadow the
other) and `server/src/api/plugins.rs::router()` exists to mount them behind
the host's own auth — but nothing in the live server calls it, and nothing in
this build can ask a loaded plugin for its own router in the first place, so
`declared_routes` is always empty today. It is a seam for a future slice, not
a reachable endpoint.

Installed plugins are tracked in the `installed_plugins` table. Install,
enable, disable and uninstall are admin-only and live under `/admin/plugins`,
handled by `server/src/api/admin/plugins.rs`.

## Development Status

### What's Implemented

#### Infrastructure
- [x] Workspace structure with modular crates
- [x] PostgreSQL database with migrations
- [x] Repository pattern (Reader/Writer traits)
- [x] Redis event bridge for horizontal scaling
- [x] Unified API server with Swagger UI
- [x] Shared libraries in `payserver-commons`

#### Authentication
- [x] Passkey/WebAuthn authentication
- [x] Ethereum wallet authentication (EIP-191)
- [x] BIP39 mnemonic recovery
- [x] Session and device management
- [x] Multi-tenant stores with role-based permissions

#### EVM Support
- [x] 12 mainnet networks configured
- [x] 8 testnet networks configured (Sepolia, Holesky, etc.)
- [x] HD wallet derivation (BIP-32/44)
- [x] RPC provider abstraction (Alloy)
- [x] Token management API

#### Payment Monitoring
- [x] Standalone evmmonitor binary
- [x] WebSocket block subscriptions (real-time)
- [x] HTTP polling fallback
- [x] Native ETH and ERC20 transfer detection
- [x] Confirmation tracking
- [x] Reorg detection
- [x] Bidirectional Redis commands

#### Payment Flow
- [x] Invoice creation with address derivation
- [x] Event consumer for payment events
- [x] Invoice status updates (pending -> processing -> paid)
- [x] Invoice expiration service
- [x] Address cleanup service
- [x] Watch retry service
- [x] Webhook notifications with HMAC-SHA256

#### Exchange Rates
- [x] Kraken + CoinGecko rate providers with caching and fallback
- [x] Fiat-to-crypto conversion integrated into invoice creation
- [x] Rate staleness validation

#### Real-time Updates
- [x] Payment status WebSocket endpoint (/ws authenticated, /checkout/ws public)
- [x] tokio broadcast for event fan-out

#### Observability
- [x] Prometheus metrics endpoint (/metrics)
- [x] HTTP request counters and latency histograms
- [x] Payment, webhook, invoice, and DB pool metrics
- [x] Sentry-protocol error reporting and structured logs, with mandatory
      PII/secret scrubbing (see [Telemetry](#telemetry))

#### Plugins
- [x] Wasmtime plugin host loads installed plugins and mounts their declared
      pages under `/plugins/{id}/pages/{path}`
- [ ] Plugin-declared routes are reserved a URL segment but not mounted — the
      router that would serve them is never called (see [Plugins](#plugins))

#### Load Testing
- [x] Goose-based load test scenarios (invoice create, list, webhook burst)
- [x] WebSocket connection stress test
- [x] Baseline targets documented

### What's Remaining

- [ ] Load test: live run (pending `LOADTEST_API_KEY` / `LOADTEST_STORE_ID` secrets) and
      baselines measured from it for automated regression comparison. The CI job
      (`.github/workflows/loadtest.yml`) and comparison script
      (`scripts/loadtest-check-regression.py`) already exist, but `loadtest/baselines.json`
      still holds sizing targets, not measured results.
- [ ] Security audit (pre-mainnet)
- [ ] Public /rates API endpoint for frontends

## Testing

```bash
# Run all tests
cargo test

# Run with test utilities
cargo test --features test-utils

# Run specific crate tests
cargo test -p evm
cargo test -p server
cargo test -p data-service
```

## Docker

Dockerfiles are located in `/docker`:

| File | Description |
|------|-------------|
| `ethpayserver.Dockerfile` | API server (ethpayserver) |
| `evmmonitor.Dockerfile` | Chain monitor (evmmonitor) |
| `docker-compose.local.yml` | Local development (default credentials, exposed ports) |
| `docker-compose.prod.yml` | Production (isolated networks, required secrets) |
| `.env.example` | Environment variable template |

### Build Individual Images

Build from ethpayserver directory:

```bash
cd ./ethpayserver

# Build API server
docker build -f docker/ethpayserver.Dockerfile -t ethpayserver .

# Build chain monitor
docker build -f docker/evmmonitor.Dockerfile -t evmmonitor .
```

### Run Local Development Stack

```bash
cd ./ethpayserver

# Copy and configure environment
cp docker/.env.example docker/.env
# Edit .env with your RPC URLs

# Start all services (postgres, redis, server, evmmonitor)
docker compose -f docker/docker-compose.local.yml up --build

# Or run in background
docker compose -f docker/docker-compose.local.yml up -d --build
```

### Run Production Stack

```bash
cd ./ethpayserver

# Copy and configure environment
cp docker/.env.example docker/.env
# Edit .env with production credentials and RPC URLs

# Start all services
docker compose -f docker/docker-compose.prod.yml up --build -d

# View logs
docker compose -f docker/docker-compose.prod.yml logs -f
```

Production differences from local:
- Requires `POSTGRES_PASSWORD` and `EVMMONITOR_CHAINS` (will fail without)
- Swagger UI disabled by default
- Internal services (postgres, redis) not exposed to host
- Isolated networks (internal for services, external for API only)
- Redis persistence enabled (`appendonly yes`)

### Run Individual Containers

```bash
# API server
docker run -p 3000:3000 \
  -e DATABASE_URL=postgres://user:pass@host/ethpayserver \
  -e REDIS_URL=redis://host:6379 \
  ethpayserver

# Chain monitor
docker run \
  -e EVMMONITOR_REDIS_URL=redis://host:6379 \
  -e EVMMONITOR_CHAINS=1,137 \
  -e EVMMONITOR_CHAIN_1_RPC_HTTP=https://eth-mainnet.g.alchemy.com/v2/KEY \
  -e EVMMONITOR_CHAIN_1_RPC_WS=wss://eth-mainnet.g.alchemy.com/v2/KEY \
  evmmonitor
```

## Security

ETHPayServer implements several security measures:

- Non-custodial by design: derives payment addresses from a merchant's xpub
  and holds no spending key in the shipped build (see [Refunds](#refunds) for
  the reserved, off-by-default exception)
- Address validation (checksum verification)
- Whitelisted token contracts only
- Confirmation requirements per chain
- Reorg detection and handling
- Webhook HMAC-SHA256 signatures
- Audit logging for all payment events

## Performance Targets

- Invoice creation: <100ms
- Payment detection: <5 seconds after confirmation
- Concurrent monitored addresses: 10,000+
- Uptime: 99.9%

## Contributing

Contributions are welcome! This project is in active development.

## License

MIT License

---

**Built with Rust** | **Self-hosted** | **Open Source**
