/**
 * A throwaway merchant account for one synthetic-payment run.
 *
 * Billing is per account over a rolling 30 days of settled volume, and the
 * synthetic payment settles real money every night. Run against one long-lived
 * account it crosses the limit and is refused invoice creation for good - the
 * limit working as designed, against the one merchant that cannot be
 * exempted. A new account starts with nothing settled, so the run takes the
 * real enforcement path and nothing is bypassed.
 *
 * Registration is the wallet flow over the public API: the server only stores
 * the key material it is handed, and this account never signs in again, so the
 * blobs are random bytes of the right shape rather than a real encryption.
 */
import { randomBytes } from 'node:crypto';

import { generatePrivateKey, privateKeyToAccount } from 'viem/accounts';

import { api } from './api';

export interface SyntheticAccount {
  /** Session bearer token for the new account. */
  token: string;
  address: string;
}

const b64 = (bytes: number) => randomBytes(bytes).toString('base64');

/**
 * Register a wallet-only merchant account named after the run.
 *
 * `label` is the run's `e2e-synthetic-<ISO timestamp>` stamp, so the account is
 * as findable as the store it owns.
 */
export async function registerSyntheticAccount(label: string): Promise<SyntheticAccount> {
  const key = privateKeyToAccount(generatePrivateKey());

  const start = await api<{ challenge_message: string; user_id: string; address: string }>(
    '/auth/wallet/new-user/start',
    { method: 'POST', body: { address: key.address, wallet_name: label } },
  );
  const signature = await key.signMessage({ message: start.challenge_message });

  const login = await api<{ session_id: string }>('/auth/wallet/new-user/complete', {
    method: 'POST',
    body: {
      user_id: start.user_id,
      address: start.address,
      signature,
      wallet_name: label,
      kdf_params: {
        algorithm: 'argon2id',
        memory_kb: 65_536,
        iterations: 3,
        parallelism: 4,
        salt: b64(16),
      },
      encrypted_symmetric_key: { ciphertext: b64(48), iv: b64(16), mac: b64(32) },
      recovery_verification_hash: b64(32),
      device_name: label,
      device_type: 'api_client',
    },
  });
  return { token: login.session_id, address: start.address };
}
