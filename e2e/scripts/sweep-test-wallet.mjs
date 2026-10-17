#!/usr/bin/env node
/**
 * Reclaim funds parked by the synthetic-payment test.
 *
 * Each nightly run sends three payments of a random 0.00005-0.00015 ETH from
 * the spender (m/44'/60'/9'/0/0) to addresses the server derived from the
 * merchant xpub at m/44'/60'/0'/0/{i}. Both come from the same seed, so that
 * principal is never actually spent — only gas is. This walks the derived
 * addresses and sends anything worth moving back to the spender.
 *
 * The indices march outwards, three per night, and never restart. They used to:
 * the counter lived on the payment method, so a fresh store began at 0 and the
 * parked funds piled up on the first few addresses. The counter moved
 * onto an account-level wallet keyed by the xpub, which is the whole point -
 * one key, one counter, no reuse - so a fresh store now continues wherever that
 * key left off. The scan has to cover the entire history rather than a window
 * near zero, and a range that stops short does not fail: it silently reports
 * nothing to sweep while the funds sit past the end of it.
 *
 * Hence --scan 1000 by default, which is over three years of nightlies at three
 * payments a night. Raise it rather than trim it; each index is one RPC call.
 *
 * Deliberately NOT part of the test: a failed sweep must not fail a run whose
 * payment already succeeded, and best-effort cleanup inside an assertion is how
 * things quietly stop working. Run it occasionally instead — once a year is
 * plenty, since gas is the only real burn.
 *
 *   E2E_TEST_MNEMONIC="..." E2E_SEPOLIA_RPC_URL="https://..." \
 *     node scripts/sweep-test-wallet.mjs [--scan 1000] [--account N] [--execute]
 *
 * Dry run by default: it prints what it would move and sends nothing. Pass
 * --execute to actually broadcast.
 *
 * ## Emptying the wallet entirely
 *
 *   ... node scripts/sweep-test-wallet.mjs --to 0xabc... [--execute]
 *
 * `--to` changes where the consolidation lands and adds the spender itself to
 * the sweep, which the default run deliberately leaves alone - it is the
 * wallet the nightly pays *from*, so draining it would stop the suite.
 *
 * ## Tokens, and why they go first
 *
 * `--token 0x...` (repeatable) sweeps an ERC-20 balance as well. It runs
 * *before* the ether pass, and the order is not a preference: moving a token
 * costs gas, gas is paid in ether by the address holding the token, and the
 * ether pass leaves each address with nothing. Sweep ether first and any
 * token on that address is stranded on a key you are about to throw away.
 *
 * This is not hypothetical. The wallet this script was extended to recover
 * had 60 USDC sitting on an address holding 2.5 ETH; an ether-only sweep
 * would have taken the ether and left the tokens unreachable forever.
 *
 * That is the point of the flag: it exists for rotation. The phrase behind
 * this wallet lives only as a GitHub Actions secret and was never written
 * down, so a run with `--to` is how the balance leaves a wallet nobody can
 * open, and it necessarily runs inside CI where that secret resolves. See
 * `.github/workflows/e2e-wallet-recover.yml`.
 */
import {
  createPublicClient,
  createWalletClient,
  erc20Abi,
  formatEther,
  formatUnits,
  getAddress,
  http,
} from 'viem';
import { mnemonicToAccount } from 'viem/accounts';
import { sepolia } from 'viem/chains';

const SPENDER_ACCOUNT_INDEX = 9; // must match synthetic-payment.spec.ts
const TRANSFER_GAS = 21_000n;
/** Skip anything that would cost more to move than it is worth. */
const GAS_HEADROOM = 2n; // require value >= 2x the fee before bothering

function requireEnv(name) {
  const v = process.env[name];
  if (!v) {
    console.error(`${name} is required. See e2e/README.md.`);
    process.exit(1);
  }
  return v;
}

const args = process.argv.slice(2);
const execute = args.includes('--execute');
const scanIdx = args.indexOf('--scan');
const scanCount = scanIdx >= 0 ? Number(args[scanIdx + 1]) : 1000;
// Which merchant account's receive addresses to walk. The synthetic test pays
// into a fresh merchant account every run, at account index = the run's start in
// Unix seconds (the same instant as its `e2e-synthetic-<ISO>` store name), so a
// run's funds sit at m/44'/60'/<that>'/0/{i}. Account 0 holds everything paid
// before that change. Run once per account to recover.
const accountIdx = args.indexOf('--account');
const merchantAccount = accountIdx >= 0 ? Number(args[accountIdx + 1]) : 0;
if (!Number.isInteger(merchantAccount) || merchantAccount < 0) {
  console.error('--account must be a non-negative integer.');
  process.exit(1);
}
const toIdx = args.indexOf('--to');
const destinationArg = toIdx >= 0 ? args[toIdx + 1] : undefined;

// Repeatable: `--token A --token B`.
const tokenArgs = args.flatMap((a, i) => (a === '--token' ? [args[i + 1]] : []));
const tokens = tokenArgs.map((t) => {
  try {
    return getAddress(t);
  } catch {
    console.error(`--token ${t} is not an address.`);
    process.exit(1);
  }
});

// Checked before anything is derived, let alone broadcast. A mistyped
// destination is not a failed run, it is ether sent to an address nobody
// holds a key for.
//
// `getAddress` alone is not that check. It *normalizes*: hand it
// `0x...DA15Bbc` when you meant `0x...DA15Bbb` and it returns a perfectly
// well-formed `0x...DA15bBc` without complaint, because it only rejects the
// wrong length or a non-hex character. The EIP-55 checksum is in the casing,
// so the test is whether the input already equals its own checksummed form -
// which is how every wallet displays an address, and which a single wrong
// character breaks.
let destination;
if (destinationArg !== undefined) {
  let normalized;
  try {
    normalized = getAddress(destinationArg);
  } catch {
    console.error(`--to ${destinationArg} is not an address.`);
    process.exit(1);
  }
  if (destinationArg !== normalized) {
    console.error(
      `--to ${destinationArg} does not match its EIP-55 checksum ` +
        `(${normalized}). Copy the address exactly as your wallet shows it; ` +
        'a mismatch here usually means a mistyped character.',
    );
    process.exit(1);
  }
  destination = normalized;
}

const mnemonic = requireEnv('E2E_TEST_MNEMONIC');
const rpcUrl = requireEnv('E2E_SEPOLIA_RPC_URL');

const transport = http(rpcUrl);
const publicClient = createPublicClient({ chain: sepolia, transport });

const spender = mnemonicToAccount(mnemonic, { accountIndex: SPENDER_ACCOUNT_INDEX });
const gasPrice = await publicClient.getGasPrice();
const fee = TRANSFER_GAS * gasPrice;

const sink = destination ?? spender.address;
console.log(
  destination
    ? `emptying the wallet to ${destination}`
    : `sweeping to spender ${spender.address}`,
);
console.log(`gas ${Number(gasPrice) / 1e9} gwei — a transfer costs ${formatEther(fee)} ETH`);
console.log(execute ? 'MODE: execute\n' : 'MODE: dry run (pass --execute to broadcast)\n');

// The addresses this script knows about: every derived receive address, plus
// the spender when the wallet is being emptied.
function accountAt(index) {
  return mnemonicToAccount(mnemonic, { accountIndex: merchantAccount, changeIndex: 0, addressIndex: index });
}

// ---------------------------------------------------------------- tokens ---
//
// Before the ether pass, always. See the header: the ether pass leaves each
// address unable to pay for anything, and an ERC-20 transfer is paid for by
// the address holding the token.
if (tokens.length > 0) {
  if (!destination) {
    console.error('--token needs --to: there is no reason to move tokens between addresses of the same wallet.');
    process.exit(1);
  }

  for (const token of tokens) {
    const [symbol, decimals] = await Promise.all([
      publicClient.readContract({ address: token, abi: erc20Abi, functionName: 'symbol' }),
      publicClient.readContract({ address: token, abi: erc20Abi, functionName: 'decimals' }),
    ]);
    console.log(`\n${symbol} (${token})`);

    let tokenMoved = 0;
    let tokenTotal = 0n;

    for (let i = 0; i <= scanCount; i++) {
      // `scanCount` receive addresses, then the spender as the last pass.
      const account = i === scanCount ? spender : accountAt(i);
      const label = i === scanCount ? 'spender' : i.toString().padStart(3);

      const balance = await publicClient.readContract({
        address: token,
        abi: erc20Abi,
        functionName: 'balanceOf',
        args: [account.address],
      });
      if (balance === 0n) continue;

      const wallet = createWalletClient({ account, chain: sepolia, transport });
      const request = {
        address: token,
        abi: erc20Abi,
        functionName: 'transfer',
        args: [destination, balance],
        account,
      };

      // Estimated rather than assumed: a token with a fee hook or a first
      // write to a fresh storage slot costs more than a plain transfer, and
      // guessing low means a reverted transaction that still burns the gas.
      let gas;
      try {
        gas = await publicClient.estimateContractGas(request);
      } catch (e) {
        console.log(`  skip  ${label} ${account.address} ${formatUnits(balance, decimals)} (cannot estimate: ${e.shortMessage ?? e.message})`);
        continue;
      }

      const ether = await publicClient.getBalance({ address: account.address });
      const cost = gas * gasPrice;
      if (ether < cost) {
        // Worth failing loudly rather than skipping: this is exactly the
        // stranded-token case, and it is silent if it only prints.
        console.error(
          `::error::${account.address} holds ${formatUnits(balance, decimals)} ${symbol} ` +
            `and ${formatEther(ether)} ETH, which is under the ${formatEther(cost)} ETH ` +
            'this transfer costs. Fund it and re-run before sweeping ether.',
        );
        process.exitCode = 1;
        continue;
      }

      console.log(`  move  ${label} ${account.address} ${formatUnits(balance, decimals)} ${symbol}`);
      tokenMoved++;
      tokenTotal += balance;

      if (execute) {
        const hash = await wallet.writeContract({ ...request, gas, gasPrice });
        await publicClient.waitForTransactionReceipt({ hash, timeout: 180_000 });
        console.log(`        sent https://sepolia.etherscan.io/tx/${hash}`);
      }
    }

    console.log(
      `  ${tokenMoved} address(es), ${formatUnits(tokenTotal, decimals)} ${symbol} ` +
        `${execute ? 'sent to' : 'would go to'} ${destination}`,
    );
  }
  console.log('');
}

// ----------------------------------------------------------------- ether ---

let swept = 0n;
let moved = 0;

for (let i = 0; i < scanCount; i++) {
  // Matches the server's receive-address derivation: m/44'/60'/{account}'/0/{i}
  const account = mnemonicToAccount(mnemonic, {
    accountIndex: merchantAccount,
    changeIndex: 0,
    addressIndex: i,
  });
  const balance = await publicClient.getBalance({ address: account.address });
  if (balance === 0n) continue;

  if (balance < fee * GAS_HEADROOM) {
    console.log(`  skip  ${i.toString().padStart(3)} ${account.address} ${formatEther(balance)} (dust)`);
    continue;
  }

  const value = balance - fee;
  console.log(`  move  ${i.toString().padStart(3)} ${account.address} ${formatEther(value)} ETH`);
  swept += value;
  moved++;

  if (execute) {
    const wallet = createWalletClient({ account, chain: sepolia, transport });
    const hash = await wallet.sendTransaction({
      to: sink,
      value,
      gas: TRANSFER_GAS,
      gasPrice,
    });
    await publicClient.waitForTransactionReceipt({ hash, timeout: 180_000 });
    console.log(`        sent https://sepolia.etherscan.io/tx/${hash}`);
  }
}

// The spender last, and only when emptying. Its balance is what funds the
// nightly, so the ordinary sweep must leave it; and it has to go after the
// others because it is where they are consolidating to when `--to` is absent.
if (destination) {
  const balance = await publicClient.getBalance({ address: spender.address });
  if (balance < fee * GAS_HEADROOM) {
    console.log(`  skip  spender ${spender.address} ${formatEther(balance)} (dust)`);
  } else {
    const value = balance - fee;
    console.log(`  move  spender ${spender.address} ${formatEther(value)} ETH`);
    swept += value;
    moved++;
    if (execute) {
      const wallet = createWalletClient({ account: spender, chain: sepolia, transport });
      const hash = await wallet.sendTransaction({
        to: destination,
        value,
        gas: TRANSFER_GAS,
        gasPrice,
      });
      await publicClient.waitForTransactionReceipt({ hash, timeout: 180_000 });
      console.log(`        sent https://sepolia.etherscan.io/tx/${hash}`);
    }
  }
}

console.log(
  `\n${moved} address(es), ${formatEther(swept)} SepoliaETH ` +
    `${execute ? 'sent to' : 'would go to'} ${sink}`,
);
if (!execute && moved > 0) console.log('Re-run with --execute to broadcast.');
if (!execute && moved === 0) {
  console.log(
    'Nothing found. If that is a surprise, raise --scan: a range that stops ' +
      'short reports exactly this rather than failing.',
  );
}
