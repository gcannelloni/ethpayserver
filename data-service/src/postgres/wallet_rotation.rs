//! Wallet rotation repository implementation.

use sqlx::{PgConnection, Row};
use uuid::Uuid;

use super::PgDataService;
use super::wallet::{
    METHOD_NAMESPACE, lock_account, lock_xpub, reject_if_another_account_holds,
    store_wallet_resolution, upsert_wallet,
};
use crate::{RepositoryResult, sqlx_to_repo_error};

/// A recorded wallet rotation event.
#[derive(Debug, Clone)]
pub struct WalletRotation {
    pub id: Uuid,
    pub store_id: Uuid,
    pub previous_xpub: String,
    pub new_xpub: String,
    pub payment_method_id: Uuid,
    pub previous_derivation_index: i32,
    pub reason: Option<String>,
    pub rotated_at: chrono::DateTime<chrono::Utc>,
}

fn row_to_rotation(row: &sqlx::postgres::PgRow) -> WalletRotation {
    WalletRotation {
        id: row.get("id"),
        store_id: row.get("store_id"),
        previous_xpub: row.get("previous_xpub"),
        new_xpub: row.get("new_xpub"),
        payment_method_id: row.get("payment_method_id"),
        previous_derivation_index: row.get("previous_derivation_index"),
        reason: row.get("reason"),
        rotated_at: row.get("rotated_at"),
    }
}

/// One payment method as the rotation sees it: which wallet it derives from
/// right now, and whether it is pinned there or merely inheriting.
struct MethodToRotate {
    payment_method_id: Uuid,
    xpub: String,
    derivation_index: i32,
    pinned: bool,
}

impl PgDataService {
    /// Record a wallet rotation and repoint a single payment method.
    ///
    /// A thin wrapper over [`Self::rotate_store_xpub`] restricted to one
    /// method, so both paths share one transaction shape and one lock order.
    pub async fn rotate_payment_method_xpub(
        &self,
        store_id: Uuid,
        payment_method_id: Uuid,
        namespace: &str,
        new_xpub: &str,
        reason: Option<&str>,
    ) -> RepositoryResult<WalletRotation> {
        let rotations = self
            .rotate_methods(
                store_id,
                Some(&[payment_method_id]),
                namespace,
                new_xpub,
                reason,
            )
            .await?;

        rotations.into_iter().next().ok_or_else(|| {
            crate::RepositoryError::NotFound(
                "payment method not found, or already on this xpub".into(),
            )
        })
    }

    /// Rotate every payment method in a store *on one chain family* onto
    /// `new_xpub`, in one transaction.
    ///
    /// Scoped by family because a key belongs to one. Rotating a store onto a
    /// new Ethereum xpub must leave its Tron methods where they are: repointing
    /// them would pin a coin-type-60 key to a chain whose wallets derive at
    /// 195, so every address quoted afterwards would be unreachable by the
    /// merchant. The schema refuses that pairing outright, so without the
    /// filter this would not misroute quietly - it would fail the whole
    /// rotation on a foreign key violation, which is a compromised-key
    /// response that cannot complete.
    ///
    /// One transaction, not a loop of them, because rotation is a response to a
    /// compromised key: a partial rotation leaves some of the store still
    /// collecting on the key the merchant just declared burnt, and the failure
    /// gives no indication of how far it got. Either the whole store moves or
    /// none of it does.
    ///
    /// Methods already deriving from `new_xpub` are skipped, so the return is
    /// the rotations that actually happened - possibly empty.
    pub async fn rotate_store_xpub(
        &self,
        store_id: Uuid,
        namespace: &str,
        new_xpub: &str,
        reason: Option<&str>,
    ) -> RepositoryResult<Vec<WalletRotation>> {
        self.rotate_methods(store_id, None, namespace, new_xpub, reason)
            .await
    }

    /// Shared body. `only` restricts the rotation to specific method ids;
    /// `None` means every method in the store.
    ///
    /// Everything happens on one connection. An earlier version called
    /// `wallet_for_store_xpub`, which opens its own transaction on a second
    /// pool connection - while this one already held a row lock on the outgoing
    /// wallet. A concurrent write to that row would then block the inner
    /// transaction on a lock the outer one holds, through an application-level
    /// edge Postgres cannot see: no deadlock is detected and both requests hang
    /// until timeout. With an N-sized pool, N concurrent rotations also
    /// exhausted the pool waiting on connections none of them could release.
    async fn rotate_methods(
        &self,
        store_id: Uuid,
        only: Option<&[Uuid]>,
        namespace: &str,
        new_xpub: &str,
        reason: Option<&str>,
    ) -> RepositoryResult<Vec<WalletRotation>> {
        let mut tx = self.pool.begin().await.map_err(sqlx_to_repo_error)?;

        let owner = sqlx::query("SELECT owner_id FROM stores WHERE id = $1")
            .bind(store_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(sqlx_to_repo_error)?
            .ok_or_else(|| crate::RepositoryError::NotFound("store not found".into()))?;
        let owner_id: Uuid = owner.get("owner_id");

        // Advisory locks first, then row locks - the order every writer in
        // `wallet` uses. Taking the wallet row lock first (as this used to)
        // inverts it against `create_wallet` and `set_primary_wallet`, both of
        // which hold the account lock before touching a wallet row.
        lock_account(&mut tx, owner_id).await?;
        lock_xpub(&mut tx, new_xpub).await?;
        reject_if_another_account_holds(&mut tx, owner_id, new_xpub).await?;

        let new_wallet = upsert_wallet(&mut tx, owner_id, namespace, new_xpub, None).await?;

        let methods =
            Self::methods_to_rotate(&mut tx, store_id, only, namespace, new_wallet.id).await?;

        let mut rotations = Vec::with_capacity(methods.len());
        let mut needs_override = false;

        for method in &methods {
            let rotation_row = sqlx::query(
                r#"
                INSERT INTO wallet_rotations
                    (store_id, previous_xpub, new_xpub, payment_method_id, previous_derivation_index, reason)
                VALUES ($1, $2, $3, $4, $5, $6)
                RETURNING id, store_id, previous_xpub, new_xpub, payment_method_id, previous_derivation_index, reason, rotated_at
                "#,
            )
            .bind(store_id)
            .bind(&method.xpub)
            .bind(new_xpub)
            .bind(method.payment_method_id)
            .bind(method.derivation_index)
            .bind(reason)
            .fetch_one(&mut *tx)
            .await
            .map_err(sqlx_to_repo_error)?;

            rotations.push(row_to_rotation(&rotation_row));

            if method.pinned {
                // Repoint the pin. No counter is touched: the destination
                // wallet already holds the only correct position for its key.
                sqlx::query("UPDATE store_payment_methods SET wallet_id = $1 WHERE id = $2")
                    .bind(new_wallet.id)
                    .bind(method.payment_method_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(sqlx_to_repo_error)?;
            } else {
                // An inheriting method stays inheriting; what has to move is
                // the thing it inherits from. Pinning it here would silently
                // undo `PUT /stores/{id}/wallet`, which exists precisely to
                // leave a store's methods following the store.
                needs_override = true;
            }
        }

        if needs_override {
            // Point the store at the new wallet. An override is written even
            // when the store had none and was following the account primary:
            // rotating one store must not move every other store on the
            // account, so the rotated store acquires an override of its own.
            //
            // Done once, after the loop, and never inside it. Moving the
            // override mid-loop would change what the remaining inheriting
            // methods resolve to, and each of them would then record a
            // rotation from the new key to itself - a fabricated row in the
            // table the migration trusts to tell honest provenance from
            // guesswork.
            sqlx::query(
                r#"
                INSERT INTO store_wallets (store_id, wallet_id, namespace)
                VALUES ($1, $2, $3)
                ON CONFLICT (store_id, namespace) DO UPDATE SET wallet_id = EXCLUDED.wallet_id
                "#,
            )
            .bind(store_id)
            .bind(new_wallet.id)
            .bind(namespace)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_to_repo_error)?;
        }

        tx.commit().await.map_err(sqlx_to_repo_error)?;

        Ok(rotations)
    }

    /// Resolve the methods in scope to the wallet each one currently derives
    /// from, locking those wallets.
    ///
    /// The lock matters: the index recorded in `wallet_rotations` has to be one
    /// no further address was issued past, so nothing may allocate between
    /// reading it and the repoint. Methods already on the destination wallet
    /// are filtered out here rather than in Rust, so they are not locked and
    /// not recorded.
    async fn methods_to_rotate(
        conn: &mut PgConnection,
        store_id: Uuid,
        only: Option<&[Uuid]>,
        namespace: &str,
        new_wallet_id: Uuid,
    ) -> RepositoryResult<Vec<MethodToRotate>> {
        // `$3::uuid[] IS NULL` is the "every method" case: a NULL array binding
        // rather than two spellings of the query that could drift apart.
        //
        // `pm.chain_namespace = $4` keeps a rotation inside the family of the
        // key being rotated to. A method on another family keeps its own
        // wallet; the new key could not be used for it in any case.
        let rows = sqlx::query(&format!(
            r#"
            SELECT pm.id AS payment_method_id,
                   pm.wallet_id IS NOT NULL AS pinned,
                   w.xpub,
                   w.derivation_index
            FROM store_payment_methods pm
            JOIN stores s ON s.id = pm.store_id
            JOIN wallets w ON w.id = COALESCE(pm.wallet_id, {})
                          AND w.namespace = {METHOD_NAMESPACE}
            WHERE pm.store_id = $1
              AND w.id <> $2
              AND ($3::uuid[] IS NULL OR pm.id = ANY($3))
              AND pm.chain_namespace = $4
            ORDER BY pm.id
            FOR UPDATE OF w
            "#,
            store_wallet_resolution(METHOD_NAMESPACE)
        ))
        .bind(store_id)
        .bind(new_wallet_id)
        .bind(only)
        .bind(namespace)
        .fetch_all(conn)
        .await
        .map_err(sqlx_to_repo_error)?;

        Ok(rows
            .iter()
            .map(|row| MethodToRotate {
                payment_method_id: row.get("payment_method_id"),
                xpub: row.get("xpub"),
                derivation_index: row.get("derivation_index"),
                pinned: row.get("pinned"),
            })
            .collect())
    }

    /// Distinct rotation events for a store since `since`. `rotated_at` is
    /// `NOW()` per insert and one rotation writes one row per method inside a
    /// single transaction, so distinct timestamps count events, not methods.
    pub async fn count_rotation_events_since(
        &self,
        store_id: Uuid,
        since: chrono::DateTime<chrono::Utc>,
    ) -> RepositoryResult<i64> {
        let row = sqlx::query(
            "SELECT COUNT(DISTINCT rotated_at) AS n FROM wallet_rotations WHERE store_id = $1 AND rotated_at >= $2",
        )
        .bind(store_id)
        .bind(since)
        .fetch_one(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;
        Ok(row.get("n"))
    }

    /// Get rotation history for a store.
    pub async fn get_rotation_history(
        &self,
        store_id: Uuid,
    ) -> RepositoryResult<Vec<WalletRotation>> {
        let rows = sqlx::query(
            r#"
            SELECT id, store_id, previous_xpub, new_xpub, payment_method_id, previous_derivation_index, reason, rotated_at
            FROM wallet_rotations
            WHERE store_id = $1
            ORDER BY rotated_at DESC
            "#,
        )
        .bind(store_id)
        .fetch_all(&self.pool)
        .await
        .map_err(sqlx_to_repo_error)?;

        Ok(rows.iter().map(row_to_rotation).collect())
    }
}
