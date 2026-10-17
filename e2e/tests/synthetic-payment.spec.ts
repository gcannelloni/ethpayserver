/**
 * Synthetic payment against the live testnet deployment.
 *
 * The one test in the suite that exercises the money path for real: it creates
 * invoices over the API, broadcasts actual Sepolia transactions to the
 * addresses the server derived, waits for `paid` on the public checkout
 * WebSocket, and asserts the store webhook fired with a valid signature.
 * Everything else in the repo stops short of an on-chain payment.
 *
 * Three invoices, not one, and all on the same store and payment method: the
 * addresses the server hands out come from one xpub and one counter, and when
 * that counter was wrong two invoices were quoted the same address. One
 * invoice per run can never see that. The assertions below are
 * therefore as much about *which* invoice each payment paid as about payment
 * working at all.
 *
 * Run:
 *   E2E_REMOTE=true E2E_SYNTHETIC_PAYMENT=true npx playwright test tests/synthetic-payment.spec.ts
 *
 * Off by default — it spends real (testnet) ETH and needs secrets, so the
 * in-pipeline `e2e` job must not pick it up. When it *is* switched on, missing
 * configuration is a hard failure rather than a skip: a silently-skipped money
 * path is exactly the gap this ticket exists to close. The pure arithmetic it
 * leans on lives in `fixtures/synthetic-payment.ts` and is unit-tested in
 * `synthetic-payment-helpers.spec.ts`, which does run everywhere.
 *
 * A new merchant account per run, not just a new store. Billing limits settled
 * volume per account over 30 days and this test settles real money nightly, so
 * one shared account is refused invoice creation sooner or later - correctly.
 * Nothing is exempted: the fresh account goes through the same enforcement as
 * any merchant, it simply has nothing settled yet.
 *
 * Funds are recoverable. The store's xpub is `m/44'/60'/<run>'` of
 * `E2E_TEST_MNEMONIC` (see `merchantPathFor`), so every address the server
 * derives (`0/{index}`) is spendable from the same mnemonic; the spender lives at a separate account
 * index and is topped up from a faucet.
 */
import { appendFileSync } from 'node:fs';

import { test, expect } from '@playwright/test';
import { createPublicClient, createWalletClient, formatEther, http } from 'viem';
import { HDKey, mnemonicToAccount } from 'viem/accounts';
import { sepolia } from 'viem/chains';
import { mnemonicToSeedSync } from '@scure/bip39';

import { api, V4_UUID, wsUrl } from '../fixtures/api';
import { registerSyntheticAccount } from '../fixtures/synthetic-account';
import {
  expectRunSurvivedArchive,
  randomInvoiceAmountWei,
  remainingBudgetMs,
  worstCaseRunCostWei,
} from '../fixtures/synthetic-payment';
import { TunnelUnavailableError, WebhookSink, verifySignature } from '../fixtures/webhook-sink';

const ENABLED = process.env.E2E_SYNTHETIC_PAYMENT === 'true';

// CAIP-2, not the bare EIP-155 integer. The API rejects the integer
// outright — `422 chain_id: invalid type: integer, expected a string` — and this
// spec kept sending one for a day because it only runs on the nightly against
// live testnet, never in PR CI.
const CHAIN_ID = 'eip155:11155111';
/**
 * Account-level path the server expects an xpub at (`evm/src/wallet.rs`).
 *
 * The account index is the run's start in Unix seconds, not 0. Each run is a
 * new merchant account, and a watched address is unique server-wide, so two
 * runs sharing an xpub would be handed the same `0/{index}` addresses from a
 * counter that restarts at 0. One second of resolution cannot repeat between
 * runs, stays below the 2^31 hardened limit until 2038, and is recoverable from
 * the store name's timestamp: `scripts/sweep-test-wallet.mjs --account <that>`.
 */
const merchantPathFor = (runStartedAt: number) => `m/44'/60'/${Math.floor(runStartedAt / 1000)}'`;
/** Kept clear of account 0 so the spender never collides with a receive address. */
const SPENDER_ACCOUNT_INDEX = 9;
/**
 * Invoices per run, on one store and one payment method.
 *
 * Two would already show a collision; three shows it as a *pattern* — a
 * counter that repeats rather than a single unlucky derivation — and gives the
 * attribution checks a payment on either side of each invoice. It is also the
 * ceiling: every extra invoice is another real Sepolia transfer plus gas, every
 * night, and the payments run one at a time, so each one costs wall clock
 * against the job cap as well as ETH.
 */
const PAYMENT_COUNT = 3;
const PAID_TIMEOUT_MS = 5 * 60_000;
const WEBHOOK_TIMEOUT_MS = 2 * 60_000;
// Sepolia inclusion is the one step whose latency we do not control. It is
// budgeted separately so the paid-detection window is not eaten by it.
const RECEIPT_TIMEOUT_MS = 3 * 60_000;
/**
 * Wall clock the three payments share.
 *
 * The timeouts above are per payment and stay that way: any single payment can
 * still spend all ten minutes. What three payments cannot do is spend ten
 * minutes *each* — that is 30 minutes, the whole `timeout-minutes` of the
 * synthetic-payment job in `.github/workflows/e2e-scheduled.yml`, and a job
 * killed at its cap uploads no report, names no step and leaves a store behind
 * because `afterEach` never runs. Every wait below is therefore clamped to what
 * is left of this budget, so a run that is going to overrun fails inside
 * Playwright with the invoice it was waiting on.
 *
 * 18 minutes against an expected ~5 (three payments at roughly a block for
 * inclusion plus three confirmations each) leaves the slow-Sepolia case plenty
 * of room, and 18 + 5 setup is 23 of the job's 30 — enough margin for `npm ci`,
 * the cloudflared install and the report upload around it.
 */
const PAYMENTS_BUDGET_MS = 18 * 60_000;
/**
 * Everything before the first broadcast: the sink's tunnel, store, invoices.
 * A budget only — these are different failures, and only the tunnel's own
 * provisioning is reported as infrastructure being unavailable.
 */
const SETUP_BUDGET_MS = 5 * 60_000;
/**
 * The least clock a payment may be started with.
 *
 * Clamping a wait to the remaining budget degrades gracefully right up to the
 * point where it does not: with seconds left, `remainingBudgetMs` returns its
 * 1s floor, the transaction is broadcast anyway, and the 1s wait then reports
 * that the invoice never went paid. That reads as a chain monitor that is not
 * connected — which the scheduled workflow opens an issue about — for a run
 * that simply ran out of clock, having spent real ETH on a payment it could
 * never have verified. Below this, fail before broadcasting and say so.
 *
 * Two minutes is inclusion in a Sepolia block or two plus a detection round
 * trip: not comfortable, but honestly attemptable.
 */
const MIN_PAYMENT_BUDGET_MS = 2 * 60_000;
/**
 * Warn once the spender holds less than this many runs' worth.
 *
 * The hard guard below only trips when the wallet is already short for the
 * *current* run — a cliff, not a warning, whose first notice is a red nightly.
 * Counted in runs rather than in ETH deliberately: at a quiet-gas worst case of
 * ~0.00195 per run (three payments of up to 0.00015 plus a 0.0005 gas floor
 * each) this is ~0.039 SepoliaETH, and it stays three weeks of nightly notice
 * whatever the amounts, the gas price and the payment count become.
 */
const LOW_BALANCE_RUNS = 20;

function requireEnv(name: string, why: string): string {
  const value = process.env[name];
  if (!value) {
    throw new Error(`${name} is required when E2E_SYNTHETIC_PAYMENT=true — ${why}`);
  }
  return value;
}

interface PaymentOption {
  chain_id: string;
  asset_symbol: string;
  token_address: string | null;
  payment_address: string;
  amount: string;
}

interface Invoice {
  id: string;
  payment_options: PaymentOption[];
}

interface PaymentMethod {
  id: string;
  /**
   * Next index the *resolved wallet* will issue, not the method's own.
   *
   * The counter moved off `store_payment_methods` onto `wallets`, and
   * this field became a read through the resolution chain (pin, store
   * override, account primary). It is null when that chain runs out, which is
   * a method that cannot be paid at all.
   */
  derivation_index: number | null;
}

/**
 * `GET /stores/{id}/wallet?payment_method_id=...` — the wallet that specific
 * payment method actually derives from.
 *
 * Queried with `payment_method_id` rather than bare: the method below is
 * created with its own xpub, which pins it to a wallet independent of
 * whatever the store's own override/primary resolve to. The bare form of
 * this endpoint reports only the latter, so it can name a different wallet
 * than the one address allocation is actually using - which is exactly the
 * gap this run exists to catch.
 */
interface StoreWallet {
  id: string;
  derivation_index: number;
  is_override: boolean;
}

interface Checkout {
  status: string;
  amount_received: string;
  is_paid: boolean;
  payments: { tx_hash: string }[];
}

/** One invoice of the run, with the address and amount it is owed. */
interface Target {
  invoice: Invoice;
  option: PaymentOption;
  amountWei: bigint;
}

/**
 * Subscribe to the public checkout socket and resolve once the invoice reports
 * `paid`.
 *
 * Reconnects on drop, because a five-minute wait outlives plenty of sockets.
 * The socket only carries live events — it replays nothing on connect — so a
 * transition landing inside a reconnect gap would otherwise be missed forever;
 * each gap is therefore closed by re-reading `GET /checkout/{id}`.
 */
async function waitForPaid(
  invoiceId: string,
  timeoutMs: number,
): Promise<{ via: string; seen: string[] }> {
  const seen: string[] = [];
  const deadline = Date.now() + timeoutMs;
  const url = `${wsUrl('/checkout/ws')}?invoice_id=${encodeURIComponent(invoiceId)}`;

  for (;;) {
    const paid = await new Promise<boolean>((resolve) => {
      let timer: ReturnType<typeof setTimeout>;
      const socket = new WebSocket(url);
      const settle = (value: boolean) => {
        clearTimeout(timer);
        socket.close();
        resolve(value);
      };
      timer = setTimeout(() => settle(false), Math.max(1_000, deadline - Date.now()));

      // Errors and drops both just end this attempt; only the deadline below
      // turns a run of failed attempts into a test failure.
      socket.onerror = () => settle(false);
      socket.onclose = () => settle(false);
      socket.onmessage = (event: { data: unknown }) => {
        const update = JSON.parse(String(event.data)) as { type: string; status?: string };
        seen.push(update.status ? `${update.type}:${update.status}` : update.type);
        if (update.type === 'invoice_status' && update.status === 'paid') settle(true);
      };
    });
    if (paid) return { via: 'checkout WebSocket', seen };

    const checkout = await api<Checkout>(`/checkout/${invoiceId}`).catch(() => null);
    if (checkout?.is_paid) return { via: 'checkout API after a WebSocket drop', seen };

    if (Date.now() >= deadline) {
      throw new Error(
        `Invoice ${invoiceId} did not reach 'paid' within ${timeoutMs}ms. ` +
          `WebSocket saw: [${seen.join(', ') || 'nothing'}]. ` +
          `Checkout API reports: ${checkout ? `${checkout.status} (received ${checkout.amount_received})` : 'unreachable'}. ` +
          `If it is stuck at 'pending' the chain monitors are probably not connected — ` +
          `check evmmonitor:health in Redis.`,
      );
    }
    await new Promise((r) => setTimeout(r, 2_000));
  }
}

/**
 * What the cleanup hook needs, published the moment it exists.
 *
 * The hook cannot read the test's locals: the run this cleanup matters most
 * for is the one that threw, and by then that scope is gone. Module scope is
 * the only place the test body and the hook both see.
 */
let createdStoreId: string | null = null;
let apiToken: string | null = null;
/** Each invoice this run saw paid, with the transaction that paid it. */
let paidRecords: { invoiceId: string; hash: string }[] = [];
/**
 * The sink, exposed to `afterEach`.
 *
 * The `finally` below stops it on any throw, but a Playwright *timeout* is not
 * a throw the test body sees: the worker is torn down mid-await and the
 * `finally` never runs, leaving a listening server and a cloudflared tunnel
 * behind. Hooks still run, so the hook is the only place a timeout is covered.
 */
let activeSink: WebhookSink | null = null;

test.describe('Synthetic payment (live testnet)', () => {
  // A retry would broadcast a second set of transactions and leave the first
  // run's invoices half-paid, so this suite never retries even when the rest of
  // CI does.
  test.describe.configure({ retries: 0 });
  test.skip(
    !ENABLED,
    'Set E2E_SYNTHETIC_PAYMENT=true to run the on-chain payment test (spends testnet ETH)',
  );

  /**
   * Remove the store this run created.
   *
   * Without this the daily schedule left one store behind per day, forever.
   * The case that has to work is the *failing* one — waiting on an on-chain
   * payment is what fails here — so this is a hook rather than anything in the
   * test body, which a throw skips straight past. Three payments only widen
   * that window: the run now has three chances to die with a store on the
   * server, and one store still holds all three invoices.
   *
   * `afterEach` rather than `afterAll` because it is the hook that is told
   * whether the test passed, and the suite holds exactly one test, so it still
   * runs exactly once.
   *
   * A cleanup failure only fails the run when the test itself passed. On an
   * already-failed run it is announced but not rethrown: a leaked store must
   * never become the reported cause and bury the payment failure underneath
   * it. Announced either way — swallowing it quietly would restore the
   * original bug in a form nobody can see, which is the whole point of this
   * ticket.
   *
   * `DELETE /stores/{id}`, which archives, not `DELETE /admin/stores/{id}`
   * (`hard_delete_store`). The hard delete cascades through invoices and
   * payments, so the proof that the money path works was erased minutes after
   * it was produced and testnet showed an empty payment list on a green day
   * and a dead monitor alike. An archived store keeps its payments, is hidden
   * from `GET /stores` by default, and is told apart from merchant traffic by
   * its `e2e-synthetic-<ISO timestamp>` name. Retention is
   * `scripts/sweep-e2e-stores.mjs`, run deliberately.
   *
   * After archiving, a passing run checks the payments are still there. That
   * is what fails if this ever goes back to a hard delete.
   */
  test.afterEach(async ({}, testInfo) => {
    // First, because it holds a port and a tunnel process. Only reached when a
    // Playwright timeout skipped the test body's own `finally`.
    if (activeSink) {
      const sink = activeSink;
      activeSink = null;
      try {
        await sink.stop();
        console.log('stopped the webhook sink from afterEach — the test body was cut short');
      } catch (err) {
        console.log(`::warning title=Webhook sink not stopped::${err}`);
      }
    }

    const storeId = createdStoreId;
    createdStoreId = null;
    const records = paidRecords;
    paidRecords = [];
    if (!storeId || !apiToken) {
      // A passing run with nothing to archive means setup never recorded the
      // store; saying nothing would look like a healthy run.
      if (testInfo.status === 'passed') {
        throw new Error('no synthetic store or API token was recorded, so nothing was archived or verified');
      }
      return;
    }

    try {
      await api(`/stores/${storeId}`, { method: 'DELETE', token: apiToken });
      console.log(`archived store ${storeId}`);
      // Only a passing run is held to this: a failed one has no promise to
      // keep, and its own failure is the story.
      if (testInfo.status === testInfo.expectedStatus) {
        const token = apiToken;
        await expectRunSurvivedArchive(
          storeId,
          records,
          PAYMENT_COUNT,
          <T>(path: string, authed: boolean) => api<T>(path, authed ? { token } : {}),
        );
      }
      return;
    } catch (err) {
      const msg =
        `Synthetic-payment archive or survival check failed for store ${storeId}: ${err}. ` +
        `If its payments are missing, suspect a hard delete or a cascade regression, not the ` +
        `archive call. If the store is still listed, archive it with ` +
        `\`DELETE /stores/${storeId}\`; \`node scripts/sweep-e2e-stores.mjs --execute\` ` +
        `removes synthetic stores for good.`;
      console.log(`::error title=Synthetic payment archive or survival check failed::${msg}`);
      if (process.env.GITHUB_STEP_SUMMARY) {
        appendFileSync(
          process.env.GITHUB_STEP_SUMMARY,
          `### \u274c Synthetic payment archive or survival check failed\n\n${msg}\n`,
        );
      }
      if (testInfo.status === testInfo.expectedStatus) throw new Error(msg);
      console.log(
        'Not failing the run on this: the test had already failed, and that is the story.',
      );
    }
  });

  test('three invoices → distinct addresses → on-chain tx → paid → webhook', async () => {
    test.setTimeout(SETUP_BUDGET_MS + PAYMENTS_BUDGET_MS);
    // Anchored where `test.setTimeout` is anchored. The payments deadline used
    // to be taken after setup, so the two clocks measured from different
    // origins and their sum could exceed the test timeout: setup overran, the
    // clamp went on promising a full payments budget, and Playwright killed the
    // test mid-wait — no invoice named, and the `finally` that stops the sink
    // never reached. Everything below measures from here.
    const runStartedAt = Date.now();

    const mnemonic = requireEnv('E2E_TEST_MNEMONIC', 'BIP39 phrase for the merchant xpub + spender');
    const rpcUrl = requireEnv('E2E_SEPOLIA_RPC_URL', 'Sepolia RPC endpoint to broadcast from');

    const merchantXpub = HDKey.fromMasterSeed(mnemonicToSeedSync(mnemonic)).derive(
      merchantPathFor(runStartedAt),
    ).publicExtendedKey;
    const spender = mnemonicToAccount(mnemonic, { accountIndex: SPENDER_ACCOUNT_INDEX });

    const transport = http(rpcUrl);
    const publicClient = createPublicClient({ chain: sepolia, transport });
    const walletClient = createWalletClient({ account: spender, chain: sepolia, transport });

    // Fail on an empty wallet with the address to refill, not with a stack
    // trace from deep inside viem when the transaction is rejected.
    // Against the amount actually needed plus a gas margin — `> 0n` passes with
    // 1 wei, which is exactly the near-drained wallet this guard exists for, and
    // the run would then die inside viem with an insufficient-funds trace.
    // Sized for the whole run at its worst case: a wallet that covers the first
    // payment and not the third fails halfway through, having already sent
    // money to an address whose invoice will now expire unpaid.
    const balance = await publicClient.getBalance({ address: spender.address });
    const gasPrice = await publicClient.getGasPrice();
    const needed = worstCaseRunCostWei(PAYMENT_COUNT, gasPrice);
    console.log(
      `gas price ${gasPrice} wei — reserving ${formatEther(needed)} SepoliaETH for the run`,
    );
    expect(
      balance >= needed,
      `Test wallet ${spender.address} holds ${formatEther(balance)} SepoliaETH, ` +
        `needs at least ${formatEther(needed)} for ${PAYMENT_COUNT} payments. ` +
        `Refill it from a faucet.`,
    ).toBe(true);
    console.log(`spender ${spender.address} — ${formatEther(balance)} SepoliaETH`);

    // Advance warning, never a failure: the run is fine, the wallet just needs
    // topping up before it isn't. Surfaces in the Actions summary so it is seen
    // without anyone reading the log.
    const lowWater = needed * BigInt(LOW_BALANCE_RUNS);
    if (balance < lowWater) {
      const runsLeft = Number(balance / needed);
      const msg =
        `Synthetic payment wallet is low: ${spender.address} holds ` +
        `${formatEther(balance)} SepoliaETH, about ${runsLeft} run(s) left. ` +
        `Top it up from a Sepolia faucet, or reclaim parked funds with ` +
        `\`node scripts/sweep-test-wallet.mjs\`.`;
      console.log(`::warning title=Synthetic payment wallet low::${msg}`);
      if (process.env.GITHUB_STEP_SUMMARY) {
        appendFileSync(process.env.GITHUB_STEP_SUMMARY, `### \u26a0\ufe0f Wallet low\n\n${msg}\n`);
      }
    }

    // The one place "could not run" applies: only the tunnel's own
    // provisioning. Store and invoice creation below (a refused invoice is a
    // product failure) must keep failing as ordinary assertions.
    const sink = await WebhookSink.start().catch((err: unknown) => {
      if (err instanceof TunnelUnavailableError) {
        const msg =
          `INFRASTRUCTURE UNAVAILABLE, NOT A PAYMENT FAILURE: the Cloudflare quick tunnel ` +
          `could not be provisioned, so no payment was attempted and nothing was verified. ` +
          `Set E2E_WEBHOOK_PUBLIC_URL to a named tunnel to remove this dependency.\n${err.message}`;
        console.log(`::error title=Synthetic payment NOT VERIFIED (tunnel unavailable)::${msg}`);
        throw new Error(msg);
      }
      throw err;
    });
    activeSink = sink;
    try {
      console.log(`webhook sink listening on :${sink.port}, public at ${sink.publicUrl}`);

      // Fresh store per run: the derivation index advances per payment method,
      // so reusing one would couple today's run to yesterday's state — and this
      // run asserts on how far the index moved, which only means anything from
      // a known starting point. The afterEach hook above archives it again —
      // keep the name on the `e2e-synthetic-` prefix that
      // `scripts/sweep-e2e-stores.mjs` matches, so a run that dies before
      // cleanup is still findable.
      const stamp = new Date(runStartedAt).toISOString().replace(/[:.]/g, '-');

      // A new merchant account per run, so the volume limit that refuses a
      // long-lived account never sees this traffic accumulate - and nothing is
      // exempted from it. The account cannot be deleted afterwards: the server
      // refuses to delete an account that took a payment, by design, so it is
      // left behind under the same `e2e-synthetic-` name as its store.
      const account = await registerSyntheticAccount(`e2e-synthetic-${stamp}`);
      const token = account.token;
      // Published before anything else can throw, for the same reason as the
      // store below.
      apiToken = token;
      console.log(`merchant account for this run: wallet ${account.address}`);
      const store = await api<{ id: string }>('/stores', {
        method: 'POST',
        token,
        body: { name: `e2e-synthetic-${stamp}` },
      });
      // Published before anything else can throw: everything below this line
      // fails often, and each of those failures used to leak the store.
      createdStoreId = store.id;

      // One payment method for all three invoices. That is the point: sharing
      // an xpub is what made two invoices collide on one address, so
      // a run that gave each invoice its own method would assert nothing.
      const method = await api<PaymentMethod>(`/stores/${store.id}/payment-methods`, {
        method: 'POST',
        token,
        body: {
          chain_id: CHAIN_ID,
          token_address: null,
          asset_symbol: 'ETH',
          decimals: 18,
          xpub: merchantXpub,
        },
      });

      // The starting point for the counter assertion, read from the wallet
      // *this method* resolves to - not the store's bare fallback, which
      // stops seeing the method's own pin the moment the account's primary
      // or override moves elsewhere. A fresh store on a fresh xpub starts at
      // 0, but read rather than assumed: the xpub comes from a mnemonic the
      // account may have used on a previous night, and the wallet remembers
      // that across stores. Asserting a delta from whatever it is now is the
      // only form that holds either way.
      const storeWallet = await api<StoreWallet>(
        `/stores/${store.id}/wallet?payment_method_id=${method.id}`,
        { token },
      );
      // `null` when the resolution chain runs out, and `undefined` if the field
      // ever stops being sent — both are "no wallet", and both must fail here
      // rather than at the first invoice with no explanation.
      expect(
        typeof method.derivation_index,
        `the new payment method resolves to no wallet at all ` +
          `(derivation_index: ${method.derivation_index}) — it cannot derive an ` +
          `address, and every invoice below would fail without saying why`,
      ).toBe('number');
      console.log(
        `store ${store.id} derives from wallet ${storeWallet.id} at index ` +
          `${storeWallet.derivation_index} (override: ${storeWallet.is_override})`,
      );

      const webhook = await api<{ webhook_secret: string | null }>(
        `/stores/${store.id}/webhook`,
        { method: 'PUT', token, body: { webhook_url: sink.publicUrl, enabled: true } },
      );
      const secret = webhook.webhook_secret;
      expect(secret, 'webhook secret is only returned on upsert — it must be present here').toBeTruthy();

      // ETH-denominated invoices: currency matches the asset, so no exchange
      // rate is involved and the test does not depend on the rate provider.
      //
      // A random amount per invoice, not the fixed 0.0001 this used to pay.
      // What that buys is narrow and worth stating precisely: anything in the
      // pipeline that hard-codes the old constant now fails, and the decimal
      // round trip (wei -> `formatEther` -> API -> quoted wei) is exercised
      // across the whole band instead of at one point that happens to work.
      //
      // What it does NOT buy is catching attribution by amount. Three
      // *distinct* amounts are exactly what a server matching payments to
      // invoices by amount gets right; three identical ones are what would
      // expose it. Address-based attribution is covered by the distinct-address
      // assertion below and by paying one invoice at a time and checking the
      // others stay unpaid.
      //
      // The draw is exact in wei and renders as a plain decimal string
      // (`fixtures/synthetic-payment.ts`).
      const targets: Target[] = [];
      for (let i = 0; i < PAYMENT_COUNT; i++) {
        const amountWei = randomInvoiceAmountWei();
        const amountEth = formatEther(amountWei);
        const invoice = await api<Invoice>('/invoices', {
          method: 'POST',
          token,
          body: {
            store_id: store.id,
            currency: 'ETH',
            amount: amountEth,
            expiration_seconds: 1_800,
            metadata: { source: 'synthetic-payment', sequence: i + 1 },
          },
        });

        expect(invoice.id, 'invoice id is not a v4 UUID').toMatch(V4_UUID);

        const option = invoice.payment_options.find(
          (o) => o.chain_id === CHAIN_ID && o.token_address === null,
        );
        expect(option, `no native Sepolia payment option on invoice ${invoice.id}`).toBeDefined();
        const target = option as PaymentOption;
        // The amount survived the round trip through a decimal string and back
        // into wei. If it did not, the transaction below would underpay by a
        // rounding error and the invoice would sit at `pending` until the
        // timeout, reported as a detection failure rather than as this.
        expect(
          BigInt(target.amount),
          `invoice ${invoice.id} was created for ${amountEth} ETH but quotes ${target.amount} wei`,
        ).toBe(amountWei);

        console.log(
          `invoice ${i + 1}/${PAYMENT_COUNT} ${invoice.id} — ${amountEth} ETH ` +
            `(${target.amount} wei) to ${target.payment_address}`,
        );
        targets.push({ invoice, option: target, amountWei });
      }

      // The assertion the whole exercise is for.
      //
      // Three invoices, one payment method, one xpub: the server allocates an
      // index per payment option and derives `0/{index}`, so the addresses must
      // differ. When they did not, one address was quoted to two invoices and
      // the payment that arrived paid whichever the monitor matched first while
      // the other expired — with a single-invoice test, invisibly.
      const addresses = targets.map((t) => t.option.payment_address.toLowerCase());
      expect(
        new Set(addresses).size,
        `payment addresses are not distinct — the derivation counter is repeating ` +
          `: ${addresses.join(', ')}`,
      ).toBe(PAYMENT_COUNT);

      // …and the counter moved by exactly three. Distinct addresses alone would
      // also hold if the index jumped about; what the counter owes is one index
      // per payment option, from one counter per xpub. Asserted as a
      // delta, because whether the stored index is "last used" or "next free"
      // is the server's business — three invoices consume three either way.
      //
      // Read from the wallet, which is where the counter lives
      // (`data-service/src/postgres/wallet.rs`, `next_derivation_index`) -
      // scoped to this method again, for the same reason as the read above.
      // The payment method reports the same number through the resolution
      // chain, but reading it there would keep passing if a second counter
      // ever reappeared per method — the exact bug this whole run exists to
      // catch. One store, one xpub, so the wallet's delta is the run's whole
      // draw.
      const wallet = await api<StoreWallet>(
        `/stores/${store.id}/wallet?payment_method_id=${method.id}`,
        { token },
      );
      expect(
        wallet.id,
        `the store resolved to wallet ${wallet.id} before the invoices and ` +
          `${storeWallet.id} after — the counter below would be measured across two keys`,
      ).toBe(storeWallet.id);
      expect(
        wallet.derivation_index - storeWallet.derivation_index,
        `derivation index on wallet ${wallet.id} moved ` +
          `${storeWallet.derivation_index} → ${wallet.derivation_index} for ` +
          `${PAYMENT_COUNT} invoices`,
      ).toBe(PAYMENT_COUNT);

      // The method must agree with the wallet it resolves to. A method
      // reporting its own number again is a second counter on one key.
      const methods = await api<PaymentMethod[]>(`/stores/${store.id}/payment-methods`, { token });
      const current = methods.find((m) => m.id === method.id);
      expect(current, `payment method ${method.id} vanished from store ${store.id}`).toBeDefined();
      expect(
        (current as PaymentMethod).derivation_index,
        `payment method ${method.id} reports index ` +
          `${(current as PaymentMethod).derivation_index} while the wallet it ` +
          `derives from is at ${wallet.derivation_index}`,
      ).toBe(wallet.derivation_index);

      /**
       * Pay them one at a time, never concurrently:
       *
       * - every transaction is signed by the same spender account, so three in
       *   flight share one nonce sequence. viem reads the pending nonce per
       *   call, two calls made together read the same one, and the second
       *   replaces the first instead of paying its own invoice.
       * - concurrency *hides* the bug this run exists to catch. If two invoices
       *   share an address, paying both at once still turns both green; paying
       *   one and then checking the others are still unpaid is what makes a
       *   shared address fail.
       * - the balance guard above read one balance up front. Spends racing each
       *   other against a single snapshot is a state the guard cannot describe.
       *
       * The cost is wall clock, which is what the shared budget is for.
       */
      // Setup is expected to fit its own budget; if it did not, say so here
      // rather than letting the overrun surface as a payment that timed out.
      const setupMs = Date.now() - runStartedAt;
      expect(
        setupMs <= SETUP_BUDGET_MS,
        `setup (sink tunnel, store, ${PAYMENT_COUNT} invoices) took ` +
          `${Math.round(setupMs / 1000)}s against a ${SETUP_BUDGET_MS / 1000}s budget. ` +
          `The payments below would be running on borrowed clock.`,
      ).toBe(true);

      const deadline = runStartedAt + SETUP_BUDGET_MS + PAYMENTS_BUDGET_MS;
      const hashes: string[] = [];

      for (const [index, { invoice, option, amountWei }] of targets.entries()) {
        const label = `payment ${index + 1}/${PAYMENT_COUNT}`;

        // Refuse to spend money we cannot then watch. See MIN_PAYMENT_BUDGET_MS.
        const budgetLeftMs = deadline - Date.now();
        expect(
          budgetLeftMs >= MIN_PAYMENT_BUDGET_MS,
          `${label} (invoice ${invoice.id}) would start with ` +
            `${Math.round(budgetLeftMs / 1000)}s left of the run budget. ` +
            `Broadcasting now would send real SepoliaETH to ` +
            `${option.payment_address} and then report the invoice as never ` +
            `detected — a monitor outage that did not happen. The earlier ` +
            `payments are what ran long; look there.`,
        ).toBe(true);

        // Subscribe before broadcasting: the socket only forwards live events,
        // so a fast confirmation must not land while we are still connecting.
        // The budget covers inclusion as well as detection: this starts before
        // the broadcast (deliberately — the socket only forwards live events),
        // so a slow Sepolia block would otherwise spend most of PAID_TIMEOUT_MS
        // before the monitors have anything to detect, and the failure would be
        // reported as "chain monitors are probably not connected".
        const paidPromise = waitForPaid(
          invoice.id,
          remainingBudgetMs(deadline, RECEIPT_TIMEOUT_MS + PAID_TIMEOUT_MS),
        );
        // Mark it handled now: if an assertion below throws first, an unobserved
        // rejection here would take the worker down instead of reporting.
        paidPromise.catch(() => {});

        const hash = await walletClient.sendTransaction({
          to: option.payment_address as `0x${string}`,
          value: BigInt(option.amount),
        });
        hashes.push(hash.toLowerCase());
        console.log(
          `${label}: ${formatEther(amountWei)} ETH to ${option.payment_address} — ` +
            `https://sepolia.etherscan.io/tx/${hash}`,
        );
        const receipt = await publicClient.waitForTransactionReceipt({
          hash,
          timeout: remainingBudgetMs(deadline, RECEIPT_TIMEOUT_MS),
        });
        expect(receipt.status, `transaction ${hash} reverted`).toBe('success');

        const { via, seen } = await paidPromise;
        console.log(`${label}: paid, detected via ${via} — updates: ${seen.join(', ') || 'none'}`);

        const checkout = await api<Checkout>(`/checkout/${invoice.id}`);
        expect(checkout.is_paid, `invoice ${invoice.id} reports ${checkout.status}`).toBe(true);
        expect(
          checkout.payments.map((p) => p.tx_hash.toLowerCase()),
          `invoice ${invoice.id} is paid, but not by the transaction sent to its own address`,
        ).toContain(hash.toLowerCase());
        paidRecords.push({ invoiceId: invoice.id, hash: hash.toLowerCase() });

        // Attribution from the other side: an invoice sharing this one's
        // address would go `paid` on this transaction, having been sent nothing.
        // Only the ones not yet paid — the earlier invoices are
        // checked again after the loop, when every hash is known.
        for (const other of targets.slice(index + 1)) {
          const state = await api<Checkout>(`/checkout/${other.invoice.id}`);
          expect(
            state.is_paid,
            `invoice ${other.invoice.id} (${other.option.payment_address}) went paid on ` +
              `${label}'s transaction ${hash} — it has been sent nothing. Two invoices are ` +
              `sharing an address.`,
          ).toBe(false);
        }

        const delivered = await sink.waitFor(
          'payment_confirmed',
          remainingBudgetMs(deadline, WEBHOOK_TIMEOUT_MS),
          // Filtered on the invoice: unfiltered, every payment after the first
          // would be handed payment one's delivery and assert against it.
          (hook) => hook.body.invoice_id === invoice.id,
        );
        expect(delivered.body.invoice_id).toBe(invoice.id);
        expect(delivered.body.store_id).toBe(store.id);
        expect(delivered.body.status).toBe('paid');
        expect(delivered.body.chain_id).toBe(CHAIN_ID);
        expect(
          verifySignature(delivered, secret as string),
          'X-Webhook-Signature did not match an HMAC-SHA256 of the delivered body',
        ).toBe(true);
      }

      // Final sweep, now that all three transactions are known: each invoice is
      // paid, and paid by its own transaction only. The in-loop check catches a
      // later invoice paid by an earlier transaction; this catches the reverse —
      // an earlier invoice quietly collecting a later payment as well, which is
      // what an address served twice looks like from the front.
      for (const [index, { invoice, option }] of targets.entries()) {
        const checkout = await api<Checkout>(`/checkout/${invoice.id}`);
        expect(checkout.is_paid, `invoice ${invoice.id} ended at ${checkout.status}`).toBe(true);
        const paidBy = checkout.payments.map((p) => p.tx_hash.toLowerCase());
        const strays = paidBy.filter((h) => h !== hashes[index]);
        expect(
          strays,
          `invoice ${invoice.id} (${option.payment_address}) also collected ` +
            `${strays.join(', ')}, which was sent to another invoice's address`,
        ).toEqual([]);
      }
    } finally {
      await sink.stop();
      activeSink = null;
    }
  });
});
