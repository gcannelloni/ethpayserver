# E2E Tests

Playwright end-to-end tests for ethpayserver.

Playwright was chosen over Cypress for its built-in WebAuthn
virtual-authenticator support via the Chrome DevTools Protocol, which is
required to drive the passkey-based auth flow.

## Local mode (default)

Runs against a local backend and trunk dev server. Requires PostgreSQL with an
`ethpayserver_e2e` database, and a checkout of
[payserver-client](https://github.com/randomcash/payserver-client) — the
frontend is its own repository now. A sibling directory is assumed; set
`PAYSERVER_CLIENT_DIR` if yours is elsewhere.

```bash
cd e2e
npm install
npx playwright test
```

The config spawns `cargo run --release --bin ethpayserver` and `trunk serve`
automatically (skipped if already running via `reuseExistingServer`). CI does
not use either: it runs the server binary it just built and the published
payserver-client image pinned in `ops/client-image.pin`, so the suite exercises
the real nginx routing rather than the dev server's proxy.

### Raise the rate limits, or the suite will fail for the wrong reason

Start the server with every limit lifted, exactly as the CI `e2e` job does:

```bash
RATE_LIMIT_AUTH=10000 RATE_LIMIT_WRITE=10000 \
RATE_LIMIT_READ=10000 RATE_LIMIT_WS=10000 \
  cargo run --release --bin ethpayserver
```

The stock defaults are `auth_rpm: 30`, `write_rpm: 120`, `read_rpm: 300`,
`ws_rpm: 60` (`server/src/api/rate_limit.rs`) — raised more than once since
this suite was first written, most recently to "numbers a payment processor
can live with." A full suite run still makes far more requests than that in a
minute, so against a stock server a scattering of tests fails with no obvious
pattern — and the rate limiter returns 429 **without logging anything**, so
the server logs look perfectly healthy while it happens. Failures land in
whichever tests happened to be running when the window filled, which makes them
read like flakiness or like a regression in whatever changed most recently.
Run at CI's `RATE_LIMIT_*=10000` and this stops being a variable at all.

## What is not here

Layout regression tests moved to
[payserver-client](https://github.com/randomcash/payserver-client) with the
frontend. They inject `styles.css` into a blank page and assert computed style —
no server, no auth — so they belong with the stylesheet they test, and a CSS
specificity regression now fails the repository that owns the CSS instead of
this one.

## Remote mode

Runs the same suite against a deployed environment (e.g. testnet) without
spawning local servers or touching the database.

```bash
E2E_REMOTE=true npx playwright test
```

This sets sane defaults:

| Variable            | Default when `E2E_REMOTE` is set     | Purpose                                    |
|---------------------|--------------------------------------|--------------------------------------------|
| `E2E_BASE_URL`      | `https://testnet.random.cash`        | Frontend URL for Playwright `baseURL`      |
| `E2E_API_URL`       | `https://testnet.random.cash`        | API base URL                               |
| `E2E_SKIP_DB_RESET` | `true` (implicit in remote mode)     | Skips `TRUNCATE` in `fixtures/db.ts`       |
| `E2E_SKIP_AUTH`     | _(unset)_                            | Set `true` to skip the auth spec           |

All defaults can be overridden explicitly:

```bash
E2E_REMOTE=true \
E2E_BASE_URL=https://staging.random.cash \
E2E_SKIP_AUTH=false \
  npx playwright test
```

### Environment variables reference

| Variable            | Default (local)                          | Description                                        |
|---------------------|------------------------------------------|----------------------------------------------------|
| `E2E_REMOTE`        | _(unset)_                                | Enable remote mode (skip webServer, adjust defaults)|
| `E2E_BASE_URL`      | `http://localhost:8080`                  | Frontend base URL                                  |
| `E2E_API_URL`       | `http://localhost:3000`                  | API base URL                                       |
| `E2E_DATABASE_URL`  | `postgres://postgres:postgres@localhost:5432/ethpayserver_e2e` | Database connection string      |
| `E2E_SKIP_DB_RESET` | _(unset)_                                | Skip database truncate-and-seed in `beforeAll`     |
| `E2E_SKIP_AUTH`     | _(unset)_                                | Set `true` to skip the auth spec                   |

### Target guard

In local mode (`E2E_REMOTE` not `true`) the harness refuses to start unless
`E2E_API_URL` is loopback on ports 3000-3099 and `E2E_BASE_URL` is loopback on
8000-8099. The live service's port, listed in `E2E_RESERVED_PORTS` (comma separated),
is refused on any host with no override; set it in your shell profile.
Any other target needs `E2E_ALLOW_NON_LOCAL_TARGET=true`. Remote mode is unaffected.

### Running against testnet from a local machine

```bash
cd e2e
npm install
E2E_REMOTE=true E2E_BASE_URL=https://testnet.random.cash npx playwright test
```

Auth tests are skipped in remote mode, and the reason has been wrong twice.

It is **not** the RP ID. The old note claimed the virtual authenticator's RP ID
(`localhost`) could not match a remote domain, but `WebAuthn.addVirtualAuthenticator`
has no RP ID parameter — it comes from the page's origin when
`navigator.credentials.create()` runs. The deployed server logs
`rp_id=testnet.random.cash rp_origin=https://testnet.random.cash`, and a single
registration against live testnet completes end to end.

It is **not** `resetDatabase()`. `fixtures/db.ts` returns early when `E2E_REMOTE`
is `true`, so it is already a no-op remotely.

The real blocker is **rate limiting**: the auth tier defaults to
`RATE_LIMIT_AUTH=30` requests per minute per IP (whatever the deployed
environment actually sets it to may differ), and this spec performs five
registrations plus a login. Depending on what else is hitting the same IP,
this can still return `HTTP 429: Too many requests` for some of the five
tests. `scout.spec.ts` registers once, which is why it is far less likely to
trip this than this spec is.

`E2E_SKIP_AUTH=false` force-runs them; expect occasional 429s until either the
spec paces itself under the limit or test traffic gets a dedicated one.

**Still local-only for a different reason:** `invoices`, `stores`,
`payment-methods`, `ui-interactions` and `webhooks` all call `resetDatabase()`.
The guard that protects a shared database is `E2E_REMOTE=true` in
`fixtures/db.ts` — *not* the `E2E_DATABASE_URL` localhost default. So with
`E2E_REMOTE` unset and `E2E_DATABASE_URL` pointed at a shared database, those
specs will truncate it.

Note separately that `scout.spec.ts` was seen failing to establish a session
after passkey registration against testnet (#56). That is a real, open gap and
unrelated to the RP ID story above.

## Test wallet maintenance (`scripts/`)

These are operator tools — nothing in CI runs them.

```bash
# Mint a throwaway Sepolia wallet: prints the phrase, the spender address to
# fund, and the merchant xpub. Store the phrase as the E2E_TEST_MNEMONIC secret.
node scripts/new-test-wallet.mjs

# Reclaim funds parked in derived receive addresses. Dry run by
# default; pass --execute to broadcast.
E2E_TEST_MNEMONIC="..." E2E_SEPOLIA_RPC_URL="https://..." \
  node scripts/sweep-test-wallet.mjs --scan 1000
```

`--scan` has to cover the *whole* history, not a window near zero. The
derivation counter used to live on the payment method, so a fresh store
each night restarted at 0 and the parked funds piled up on the first few
addresses; the counter now lives on an account-level wallet keyed by the xpub,
so the indices march outwards three per night and never restart. A scan that
stops short reports nothing to sweep rather than failing, so the default is 1000
(over three years of nightlies). Raise it rather than trim it.

Each nightly run makes three payments of a random 0.00005–0.00015 ETH
(~0.0003/run on average) from the spender to addresses derived from the *same*
seed, so the principal is parked rather than spent — only gas (~0.00006/run at
0.94 gwei, three transfers) is actually consumed. At 0.05 funded that is ~140
runs without sweeping, ~800 with.

The spec emits a `::warning::` once fewer than 20 runs' worth remain, and sizes
a run at its **worst case** — three maximum draws plus a gas reserve each, the
larger of a 0.0005 floor and three times the live gas price, so ~0.00195 while
Sepolia is quiet — because the amounts of future runs have not been drawn yet
and the gas price they will pay is not today's. That is a
deliberately pessimistic ~0.039 ETH line (it assumes the parked principal is
gone), so a wallet funded at 0.05 and never swept starts warning after a few
weeks. Sweep it, or fund ~0.1.

## Leftover synthetic-payment stores (`scripts/sweep-e2e-stores.mjs`)

The synthetic-payment spec creates a store per run and now removes it again in
an `afterEach`. This script archives the ones that accumulated before
that landed, and anything a run abandoned by dying outright.

```bash
E2E_API_URL=https://testnet.random.cash E2E_REMOTE=true E2E_API_TOKEN=ak_... \
  node scripts/sweep-e2e-stores.mjs          # lists only
E2E_API_URL=... E2E_REMOTE=true E2E_API_TOKEN=ak_... \
  node scripts/sweep-e2e-stores.mjs --execute
```

It **archives** every live store whose name starts with `e2e-`, through the
self-service `DELETE /stores/{id}`. Archived stores drop out of `GET /stores`
(`?archived=true` lists them), refuse new invoices, and keep their payment
history; `POST /stores/{id}/unarchive` reverses it. Because it is reversible
there is no timestamp-shape gate, and the token only needs to own the stores
(it need not be `server_admin`). `GET /stores` returns only the token's own
stores, so it cannot reach another account. A real merchant store whose name
starts with `e2e-` would be archived too, so `--execute` refuses any host other
than `testnet.random.cash` or localhost unless you add `--force`.

It no longer hard-deletes. `DELETE /admin/stores/{id}` (`hard_delete_store`)
still exists as the escape hatch and still refuses any name that is not the
exact synthetic stamp.

## Leftover registrations (`scripts/sweep-e2e-accounts.mjs`)

`tests/scout.spec.ts` registers a fresh passkey account per run; most never
get past registration, so what is left is an account with no email, no wallet
and usually no store. Its own `afterAll` now removes the account it just
created, the same way `synthetic-payment.spec.ts`'s `afterEach` removes the
store it created — set `E2E_API_TOKEN` to a `server_admin` token in whatever
job runs `scout.spec.ts` and cleanup happens there, next to creation, with no
separate schedule to keep in sync. Without that variable set (e.g. running
scout locally, or CI's own non-remote `e2e` job, which resets its database
every run and never sets it) it leaves the account in place rather than
failing the run.

**No scheduled job actually exercises this against testnet today.** The
scheduled `browser` job runs `perf.spec.ts` only — `scout.spec.ts` is
deliberately left out of it, per the comment in
`.github/workflows/e2e-scheduled.yml`, because remote passkey registration
does not yet establish a session (#56, open and unrelated to this cleanup
hook). Re-add `scout.spec.ts` to that job, with `E2E_API_TOKEN` set on it,
once #56 is fixed - until then this hook only ever runs locally or against a
manual `E2E_REMOTE=true` invocation, which is real coverage of the cleanup
logic but not of "testnet stops accumulating scout accounts on its own."

`scripts/sweep-e2e-accounts.mjs` is the backfill for residue that predates
that hook - every account this repo's E2E runs created before this shipped -
and the backstop for a run whose cleanup step itself failed. It removes any
account with no email and no wallet that also has no store, or whose only
stores are ones the sweep above would also clear.

```bash
E2E_API_URL=https://testnet.random.cash E2E_REMOTE=true E2E_API_TOKEN=ak_... \
  node scripts/sweep-e2e-accounts.mjs          # lists only
E2E_API_URL=... E2E_REMOTE=true E2E_API_TOKEN=ak_... \
  node scripts/sweep-e2e-accounts.mjs --execute
```

The token must belong to a `server_admin` — it calls `GET /admin/users`,
`GET /admin/users/{id}/stores` and `DELETE /admin/users/{id}`
(`server/src/api/admin/mod.rs`). That last one goes through the same cascade
as self-service `DELETE /users/me`: it refuses on its own if the account ever
took a payment, payout or refund, and it refuses outright on a `server_admin`
target, so this script cannot reach the account `synthetic-payment.spec.ts`
pays into even if the query above ever matched it by accident. It does not,
today — that account also owns `testnet-subscriptions`, a store this sweep
does not recognize — but the script hardcodes an exclusion for it anyway,
because a query that happens to be safe today is not a guarantee.

## Synthetic payment (`tests/synthetic-payment.spec.ts`)

The only test that exercises the money path for real: it creates invoices over
the API, broadcasts actual Sepolia transactions to the addresses the server
derived, waits for `paid` on the public checkout WebSocket, and asserts the store
webhook fired with a valid HMAC signature.

Three invoices per run, on one store and one payment method, paid one at a time.
That is the regression test for address reuse: the addresses come from a single xpub
and a single counter, and when the counter was wrong two invoices were quoted the
same address — which one invoice per run can never see. The run asserts the three
addresses are distinct, that the counter on the wallet the store derives from
advanced by exactly three, that the payment method agrees with that
wallet rather than keeping a number of its own, and that each payment paid its
own invoice and no other.

The three payments share an 18-minute wall clock, checked against the job's
30-minute `timeout-minutes` in `.github/workflows/e2e-scheduled.yml`: each
payment keeps its full per-payment timeouts, but a run that would overrun the job
fails inside Playwright — naming the invoice it was waiting on — rather than
being killed at the cap with no report and a leaked store.

The pure parts (the amount draw, the shared budget) live in
`fixtures/synthetic-payment.ts` and are unit-tested by
`tests/synthetic-payment-helpers.spec.ts`, which runs in every suite: the spec
itself only runs on a runner holding secrets, so arithmetic left inside it is
unverified until a nightly spends real ETH to find out.

It is **off unless `E2E_SYNTHETIC_PAYMENT=true`**, because it spends testnet ETH
and needs secrets — the in-pipeline `e2e` job must not pick it up. When it *is*
on, missing configuration fails the test rather than skipping it: a silently
skipped money path is the gap this test exists to close.

```bash
E2E_REMOTE=true E2E_SYNTHETIC_PAYMENT=true \
E2E_TEST_MNEMONIC="..." E2E_SEPOLIA_RPC_URL=https://... \
  npx playwright test tests/synthetic-payment.spec.ts
```

| Variable                   | Required | Description                                                        |
|----------------------------|----------|--------------------------------------------------------------------|
| `E2E_SYNTHETIC_PAYMENT`    | yes      | `true` to run the spec at all                                      |
| `E2E_TEST_MNEMONIC`        | yes      | BIP39 phrase — merchant xpub **and** the spending wallet           |
| `E2E_SEPOLIA_RPC_URL`      | yes      | Sepolia RPC endpoint used to broadcast                             |
| `E2E_WEBHOOK_PUBLIC_URL`   | no       | Skip the cloudflared quick tunnel and use this base URL instead    |
| `E2E_WEBHOOK_PORT`         | no       | Bind the sink to a fixed port (pairs with the above)               |
| `E2E_CLOUDFLARED_BIN`      | no       | Path to `cloudflared` (default: on `PATH`)                         |
| `E2E_API_PREFIX`           | no       | API path prefix (default `/api` remote, empty locally)             |

### One-time setup

1. Generate a BIP39 mnemonic **for testnet only** and store it as the repo secret
   `E2E_TEST_MNEMONIC`. Never commit it, and never reuse a mainnet phrase.
2. Fund the spending wallet at `m/44'/60'/9'/0/0` from a Sepolia faucet. The test
   prints the address and its balance on every run, and fails with that address
   when the balance hits zero.
3. Nothing to create on testnet: each run registers its own merchant account
   (wallet flow, no CAPTCHA) so the per-account volume limit never accumulates.
   If the server turns CAPTCHA on, registration fails with that message.
4. Store a Sepolia RPC endpoint as `E2E_SEPOLIA_RPC_URL`.
5. Set `SENTRY_CRON_E2E_URL` to a Sentry Cron Monitor check-in URL. This is
   required, not optional, and the workflow enforces it — the check-in step
   fails the run while the secret is unset, loudly, every time, rather than
   skipping quietly. Skipping quietly would mean neither a passing run, nor a
   failing one, nor a run that never happens at all ever reached a person,
   which is the whole thing the monitor exists to prevent.

Nothing else needs provisioning: the test creates its own account, store, payment method
and webhook config on each run.

### Why the funds are not burned

Each run's store xpub is `m/44'/60'/<run>'` of the same mnemonic, where `<run>`
is the run's start in Unix seconds, and the server derives payment addresses at
`0/{index}` beneath it (`evm/src/wallet.rs`). Every address it pays into is
therefore spendable from `E2E_TEST_MNEMONIC` — the faucet ETH can be swept back
to the spender with `node scripts/sweep-test-wallet.mjs --account <run>` rather
than being stranded. Runs before the fresh-account change used account 0. The spender sits at account index 9 so it can never collide
with a receive address.

### Deliberate non-cleanup

Each run creates a fresh merchant account and store (`e2e-synthetic-<timestamp>`).
The account stays: the server refuses to delete one that took a payment.
The store is archived by the run. The derivation index advances per payment method, so reusing one store
would couple each run to the last; and on a failure the invoice and its payment
rows are the evidence. Prune them by hand if testnet gets noisy.

## Nightly visual review (`tests/visual-review.spec.ts`, `scripts/visual-review.mjs`)

The routes scout.spec.ts already reaches — unauthenticated, then authenticated
after one passkey registration — screenshotted at a mobile (375x812) and a
desktop (1280x720) viewport. `scripts/visual-review.mjs` then sends each
route's pair of screenshots to Claude against a fixed rubric (clipped content,
an unlabelled control, a raw decimal where a formatted amount belongs, and so
on) and writes `test-results/visual/report.md`. The looking is done by the
Claude Code CLI (`claude -p`), which reads the PNGs off disk itself.

This is advisory, not a gate — a model judging layout will produce false
positives, and a check that can go red on one gets disabled. It never fails
the build.

**The nightly job lives in `central-infrastructure`**
(`.github/workflows/visual-review-testnet.yml`), not here: it points at a
deployed service and needs a model credential, which is that repo's business
and not a public repo's. It checks this repo out at `testnet`, runs the two
commands below, and files (or comments on) a `visual-review`-labelled issue
*in this repo* only when there is something to report — the findings are
public bug reports (see below), the credential is not.

This repo is public, so the issue carries the per-finding "what is wrong"
text in full, not just counts and routes — the rubric only ever asks about
layout/UX defects, never a security or fund-movement bug, and every one of
those is already visible to anyone loading the page, so describing it here
is no different from any other public bug report. Only the screenshots stay
out of it: a `visual-review-manifest` artifact (which routes/viewports
captured, no defect text) is uploaded for debugging the capture pipeline,
and report.md / findings.json stay in the job's own ephemeral workspace,
since a picture of the live UI adds nothing the text doesn't already say.

A route that could not be captured (a broken passkey registration, a
navigation timeout) or reviewed (a CLI failure, output that is not
the asked-for JSON) is not silently dropped — it lands in `findings.json`'s
`errors` array and in `report.md` as "could not be reviewed," so a broken run
reads as incomplete rather than as a clean pass with nothing to say.

**The exit contract:** under `E2E_VISUAL_REVIEW=true` the review script exits
0 if and only if at least one route was judged. A per-route model error,
refusal or unparseable answer stays advisory and exits 0 — a model judging
layout produces false positives, and a check that goes red on one gets
disabled. But a run that judged *no* route is a broken pipeline, not a verdict
on the page: an absent credential, a revoked one (every route's call fails), a
capture that produced nothing. Those exit 1, and nothing is published for them
— `renderPublicSummary` returns null, so there is no issue body to post and
not even the errors reach the public issue. The reason is in the job log,
which is private. An advisory green reading "no route was reviewed" would be
the nineteen-night failure rebuilt one layer up.

`findings.json` also carries `reviewed`: how many routes were actually
judged, and `total`: how many the capture step set out to produce (read from
`expected-routes.json`, null if absent). Every rendering says "N of M routes
reviewed" and a run with `reviewed < total` is marked INCOMPLETE, so one route
of eight cannot read as a full clean night. "No findings" is a claim about routes that were looked at, so the
report may not print it without the count it is out of. That is not a
hypothetical — between 2026-09-27 and 2026-10-15 nineteen consecutive nightly
runs filed "No findings among the routes that were reviewed" having reviewed
none, because the credential they needed existed in no repository and a
missing one exited 0.

Coverage is checked against the client router, not the sidebar:
`fixtures/client-routes.txt` is a copy of the list payserver-client tests
against its router, and `tests/route-coverage.spec.ts` fails when a route in it
has neither a capture nor a stated reason in `ROUTE_DISPOSITION`. In-page states
(the four store tabs) are listed by hand in `TAB_STATES` and are deliberately
not exhaustive. A nightly run that misses any promised capture at either
viewport fails its `Coverage` test; the model's judgement stays advisory.

**Off by default**, same reasoning as the synthetic payment: `npx playwright
test` with no filter is what `ci.yml`'s `e2e` job runs, and a screenshot pass
nobody reviews has no business slowing down every push. Run it explicitly:

```bash
E2E_REMOTE=true E2E_VISUAL_REVIEW=true npx playwright test tests/visual-review.spec.ts
CLAUDE_CODE_OAUTH_TOKEN=... node scripts/visual-review.mjs
```

`CLAUDE_CODE_OAUTH_TOKEN` is the credential — the same one the agent jobs
already use, which is the point: this script spent its first nineteen nightly
runs asking for an `ANTHROPIC_API_KEY` that existed in no repository and that
nobody had to create for the step to go green. Without it the review script
logs and exits cleanly locally, but under `E2E_VISUAL_REVIEW=true` (as the
scheduled job sets it) it exits 1 — a missing credential there is a broken
pipeline, not an empty run.

`VISUAL_REVIEW_MODEL`, `VISUAL_REVIEW_TIMEOUT_MS` (per route, default 300000)
and `VISUAL_REVIEW_CLAUDE_BIN` (a stand-in CLI, which is how
`tests/visual-review-main.spec.ts` runs the real script without a credential)
are the knobs.
