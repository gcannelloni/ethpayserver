//! Account wallet repository behaviour against a real database.
//!
//! The invariant under test throughout: one xpub has one derivation counter,
//! and no two allocations from it ever return the same index.

use std::collections::HashSet;

use sqlx::Row;
use types::{ChainId, StorePaymentMethodReader, StorePaymentMethodWriter};
use uuid::Uuid;

use super::super::PgDataService;
use super::super::tests::create_test_service;
use crate::{RepositoryError, WalletReader, WalletWriter};

/// The chain family every wallet in this file belongs to unless it says
/// otherwise. Spelled out at each call rather than defaulted, because the
/// whole point of these signatures is that the family is never assumed.
const EVM: &str = types::NAMESPACE_EIP155;

/// Tron, for the tests about a family this server has a key for but the store
/// does not.
const TRON: &str = types::NAMESPACE_TRON;

/// A key no other test is using.
///
/// An xpub may only be registered to one account, so tests that share a
/// constant would refuse each other's setup once they run against the same
/// database - which is exactly the guard working. The repository never parses
/// these, so they need only be unique, not valid BIP-32.
fn unique_xpub(tag: &str) -> String {
    format!("xpub-{tag}-{}", Uuid::new_v4())
}

/// Seed a user and return its id.
async fn seed_user(service: &PgDataService) -> Uuid {
    let user_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, kdf_params, encrypted_symmetric_key, \
         recovery_verification_hash, kdf_salt_identifier) \
         VALUES ($1, '{}'::jsonb, '{}'::jsonb, 'h', 'passkey:' || $1::text)",
    )
    .bind(user_id)
    .execute(service.pool())
    .await
    .expect("seed user");
    user_id
}

/// Seed a store owned by `user_id`.
async fn seed_store_for(service: &PgDataService, user_id: Uuid) -> Uuid {
    let store_id = Uuid::new_v4();
    sqlx::query("INSERT INTO stores (id, name, owner_id) VALUES ($1, $2, $3)")
        .bind(store_id)
        .bind(format!("store-{store_id}"))
        .bind(user_id)
        .execute(service.pool())
        .await
        .expect("seed store");
    store_id
}

// =========================================================================
// The invariant
// =========================================================================

/// Two payment methods configured with the same xpub must share one counter.
///
/// This is the bug account wallets exist to remove, in the shape it actually occurs:
/// not two merchants, but one store accepting ETH and USDC on the same key.
/// Under the old schema each row counted independently and both returned 0,
/// then 1, then 2 - the same three addresses, twice.
#[tokio::test]
#[ignore]
async fn two_methods_on_one_xpub_never_get_the_same_index() {
    let service = create_test_service().await;
    let xpub_a = unique_xpub("a");
    let user = seed_user(&service).await;
    let store = seed_store_for(&service, user).await;

    let eth = StorePaymentMethodWriter::create_payment_method(
        &service,
        store,
        &ChainId::evm(1),
        None,
        "ETH",
        18,
        Some(&xpub_a),
    )
    .await
    .expect("create eth method");
    let usdc = StorePaymentMethodWriter::create_payment_method(
        &service,
        store,
        &ChainId::evm(1),
        Some("0x1111111111111111111111111111111111111111"),
        "USDC",
        6,
        Some(&xpub_a),
    )
    .await
    .expect("create usdc method");

    assert_eq!(
        eth.wallet_id, usdc.wallet_id,
        "the same xpub must resolve to the same wallet, or it has two counters"
    );

    let mut seen = HashSet::new();
    for _ in 0..4 {
        for method in [eth.id, usdc.id] {
            let index = StorePaymentMethodWriter::allocate_derivation(&service, method)
                .await
                .expect("allocate index")
                .index;
            assert!(
                seen.insert(index),
                "index {index} was issued twice across methods sharing one \
                 xpub - that is two customers sent to one address"
            );
        }
    }
    assert_eq!(seen.len(), 8);
}

/// Concurrent allocation on one wallet must hand out distinct indices.
///
/// `next_derivation_index` is a single `UPDATE ... RETURNING`, so the row lock
/// serialises callers. A read-then-write would fail this under READ COMMITTED:
/// both readers see the same value and both return it.
#[tokio::test]
#[ignore]
async fn concurrent_allocation_on_one_wallet_issues_distinct_indices() {
    let service = create_test_service().await;
    let xpub_a = unique_xpub("a");
    let user = seed_user(&service).await;
    let wallet = WalletWriter::create_wallet(&service, user, EVM, &xpub_a, None)
        .await
        .expect("create wallet");

    let service = std::sync::Arc::new(service);
    const N: usize = 32;

    let mut handles = Vec::with_capacity(N);
    for _ in 0..N {
        let svc = service.clone();
        let id = wallet.id;
        handles.push(tokio::spawn(async move {
            WalletWriter::next_derivation_index(&*svc, id).await
        }));
    }

    let mut indices = HashSet::new();
    for h in handles {
        let index = h.await.expect("join").expect("allocate");
        assert!(
            indices.insert(index),
            "index {index} was handed out twice under concurrency"
        );
    }

    assert_eq!(indices.len(), N, "every allocation must be distinct");
    assert_eq!(
        indices.iter().copied().max().unwrap(),
        N as i32 - 1,
        "indices must be contiguous from 0: a gap means an allocation was lost, \
         a duplicate means one was reused"
    );

    let after = WalletReader::get_wallet(&*service, wallet.id)
        .await
        .expect("read back")
        .expect("wallet exists");
    assert_eq!(
        after.derivation_index, N as i32,
        "the stored counter must be one past the last index issued"
    );
}

/// Re-adding an xpub the account already holds returns the same wallet.
#[tokio::test]
#[ignore]
async fn adding_a_known_xpub_does_not_create_a_second_counter() {
    let service = create_test_service().await;
    let xpub_a = unique_xpub("a");
    let user = seed_user(&service).await;

    let first = WalletWriter::create_wallet(&service, user, EVM, &xpub_a, Some("first"))
        .await
        .expect("create");
    WalletWriter::next_derivation_index(&service, first.id)
        .await
        .expect("allocate");

    let again = WalletWriter::create_wallet(&service, user, EVM, &xpub_a, Some("again"))
        .await
        .expect("re-add");

    assert_eq!(
        first.id, again.id,
        "a second row for a key the account already holds would be a second \
         counter on it"
    );
    assert_eq!(
        again.derivation_index, 1,
        "re-adding must not rewind the counter - that re-issues address 0"
    );
    assert_eq!(
        WalletReader::list_wallets(&service, user)
            .await
            .unwrap()
            .len(),
        1
    );
}

// =========================================================================
// Store resolution
// =========================================================================

/// A store with no override derives from the account primary; one with an
/// override derives from that instead; clearing it falls back again.
#[tokio::test]
#[ignore]
async fn a_store_without_an_override_uses_the_account_primary() {
    let service = create_test_service().await;
    let xpub_a = unique_xpub("a");
    let xpub_b = unique_xpub("b");
    let user = seed_user(&service).await;
    let store = seed_store_for(&service, user).await;

    assert!(
        WalletReader::resolve_store_wallet(&service, store, EVM)
            .await
            .unwrap()
            .is_none(),
        "an account with no wallet resolves to nothing, not to a default"
    );

    // The first wallet on an account becomes its primary.
    let primary = WalletWriter::create_wallet(&service, user, EVM, &xpub_a, Some("primary"))
        .await
        .unwrap();
    assert!(primary.is_primary);

    let resolved = WalletReader::resolve_store_wallet(&service, store, EVM)
        .await
        .unwrap()
        .expect("resolves to the primary");
    assert_eq!(resolved.id, primary.id);

    // Pin the store elsewhere.
    let other = WalletWriter::create_wallet(&service, user, EVM, &xpub_b, Some("other"))
        .await
        .unwrap();
    assert!(!other.is_primary, "only the first wallet is primary");

    WalletWriter::set_store_wallet(&service, store, other.id)
        .await
        .unwrap();
    assert_eq!(
        WalletReader::resolve_store_wallet(&service, store, EVM)
            .await
            .unwrap()
            .unwrap()
            .id,
        other.id,
        "an override must win over the primary"
    );

    // Moving the primary must not move an overridden store.
    WalletWriter::set_primary_wallet(&service, user, other.id)
        .await
        .unwrap();
    WalletWriter::set_store_wallet(&service, store, primary.id)
        .await
        .unwrap();
    assert_eq!(
        WalletReader::resolve_store_wallet(&service, store, EVM)
            .await
            .unwrap()
            .unwrap()
            .id,
        primary.id
    );

    // And clearing it falls back to whatever the primary now is.
    WalletWriter::clear_store_wallet(&service, store, EVM)
        .await
        .unwrap();
    assert_eq!(
        WalletReader::resolve_store_wallet(&service, store, EVM)
            .await
            .unwrap()
            .unwrap()
            .id,
        other.id,
        "with no override the store follows the current primary"
    );
}

/// A wallet on another account may not be pinned to this one's store.
#[tokio::test]
#[ignore]
async fn a_store_cannot_be_pinned_to_another_accounts_wallet() {
    let service = create_test_service().await;
    let xpub_a = unique_xpub("a");
    let mine = seed_user(&service).await;
    let theirs = seed_user(&service).await;
    let store = seed_store_for(&service, mine).await;

    let not_mine = WalletWriter::create_wallet(&service, theirs, EVM, &xpub_a, None)
        .await
        .unwrap();

    assert!(
        WalletWriter::set_store_wallet(&service, store, not_mine.id)
            .await
            .is_err(),
        "pinning a store to another account's key would send this merchant's \
         money to that one"
    );
}

// =========================================================================
// Primary and deletion
// =========================================================================

/// Promotion demotes the outgoing primary, in one transaction.
#[tokio::test]
#[ignore]
async fn promoting_a_wallet_demotes_the_previous_primary() {
    let service = create_test_service().await;
    let xpub_a = unique_xpub("a");
    let xpub_b = unique_xpub("b");
    let user = seed_user(&service).await;

    let a = WalletWriter::create_wallet(&service, user, EVM, &xpub_a, None)
        .await
        .unwrap();
    let b = WalletWriter::create_wallet(&service, user, EVM, &xpub_b, None)
        .await
        .unwrap();

    let promoted = WalletWriter::set_primary_wallet(&service, user, b.id)
        .await
        .unwrap();
    assert!(promoted.is_primary);

    assert_eq!(
        WalletReader::get_primary_wallet(&service, user, EVM)
            .await
            .unwrap()
            .unwrap()
            .id,
        b.id
    );
    assert!(
        !WalletReader::get_wallet(&service, a.id)
            .await
            .unwrap()
            .unwrap()
            .is_primary,
        "the outgoing primary must be demoted, not left as a second one"
    );
}

/// A wallet still in use cannot be deleted out from under the addresses it
/// derived.
#[tokio::test]
#[ignore]
async fn a_wallet_in_use_cannot_be_deleted() {
    let service = create_test_service().await;
    let xpub_a = unique_xpub("a");
    let user = seed_user(&service).await;
    let store = seed_store_for(&service, user).await;

    let method = StorePaymentMethodWriter::create_payment_method(
        &service,
        store,
        &ChainId::evm(1),
        None,
        "ETH",
        18,
        Some(&xpub_a),
    )
    .await
    .unwrap();

    let wallet_id = method
        .wallet_id
        .expect("a configured method resolves a wallet");
    assert!(
        WalletWriter::delete_wallet(&service, wallet_id)
            .await
            .is_err(),
        "deleting a wallet a payment method still derives from would strand \
         every address it has issued"
    );

    StorePaymentMethodWriter::delete_payment_method(&service, method.id)
        .await
        .unwrap();
    WalletWriter::delete_wallet(&service, wallet_id)
        .await
        .expect("deletable once nothing references it");
}

/// Rotation repoints at the new key's wallet and does not rewind a counter.
#[tokio::test]
#[ignore]
async fn rotation_repoints_without_resetting_the_counter() {
    let service = create_test_service().await;
    let xpub_a = unique_xpub("a");
    let xpub_b = unique_xpub("b");
    let user = seed_user(&service).await;
    let store = seed_store_for(&service, user).await;

    let method = StorePaymentMethodWriter::create_payment_method(
        &service,
        store,
        &ChainId::evm(1),
        None,
        "ETH",
        18,
        Some(&xpub_a),
    )
    .await
    .unwrap();
    for _ in 0..3 {
        StorePaymentMethodWriter::allocate_derivation(&service, method.id)
            .await
            .unwrap();
    }

    // The account has used &xpub_b before and it is already at index 7.
    let b = WalletWriter::create_wallet(&service, user, EVM, &xpub_b, None)
        .await
        .unwrap();
    for _ in 0..7 {
        WalletWriter::next_derivation_index(&service, b.id)
            .await
            .unwrap();
    }

    let rotation = service
        .rotate_payment_method_xpub(store, method.id, EVM, &xpub_b, Some("test"))
        .await
        .expect("rotate");
    assert_eq!(rotation.previous_xpub, xpub_a);
    assert_eq!(rotation.previous_derivation_index, 3);

    let after = StorePaymentMethodReader::get_payment_method(&service, method.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.wallet_id,
        Some(b.id),
        "the method must point at the new key"
    );
    assert_eq!(
        after.derivation_index,
        Some(7),
        "rotation must not reset a shared counter to zero - that re-issues \
         every address the key has already produced"
    );
}

/// The derivation index reaches the payment option that used it.
#[tokio::test]
#[ignore]
async fn payment_options_record_the_wallet_and_index_they_used() {
    let service = create_test_service().await;
    let xpub_a = unique_xpub("a");
    let user = seed_user(&service).await;
    let store = seed_store_for(&service, user).await;

    let method = StorePaymentMethodWriter::create_payment_method(
        &service,
        store,
        &ChainId::evm(1),
        None,
        "ETH",
        18,
        Some(&xpub_a),
    )
    .await
    .unwrap();

    let invoice_id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO invoices (id, store_id, currency, amount, expires_at) \
         VALUES ($1, $2, 'USD', 100, NOW() + interval '1 hour')",
    )
    .bind(&invoice_id)
    .bind(store)
    .execute(service.pool())
    .await
    .unwrap();

    let allocation = StorePaymentMethodWriter::allocate_derivation(&service, method.id)
        .await
        .unwrap();
    let index = allocation.index;

    let option = types::PaymentOptionData {
        id: types::PaymentOptionId::new(),
        invoice_id: types::InvoiceId::from_string(invoice_id.clone()),
        payment_method_id: types::PaymentMethodId::new("ETH", &ChainId::evm(1)),
        chain_id: ChainId::evm(1),
        asset_symbol: "ETH".to_string(),
        token_address: None,
        decimals: 18,
        payment_address: format!("0x{:040x}", Uuid::new_v4().as_u128()),
        wallet_id: Some(allocation.wallet_id),
        derivation_index: Some(index),
        amount: "1".to_string(),
        rate: None,
        rate_at: None,
        is_active: true,
        created_at: chrono::Utc::now(),
    };
    types::PaymentOptionWriter::create(&service, &option)
        .await
        .unwrap();

    let read = types::PaymentOptionReader::get(&service, &option.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.wallet_id, method.wallet_id);
    assert_eq!(
        read.derivation_index,
        Some(index),
        "without the index the address cannot be tied back to a position on \
         the key"
    );

    let stored: Option<i32> =
        sqlx::query("SELECT derivation_index FROM payment_options WHERE id = $1")
            .bind(option.id.0)
            .fetch_one(service.pool())
            .await
            .unwrap()
            .get("derivation_index");
    assert_eq!(stored, Some(index));
}

// =========================================================================
// Cross-account exclusivity
// =========================================================================

/// An xpub another account already holds is refused, not silently duplicated.
///
/// Two accounts on one key is the same collision as two counters on one key,
/// reached from the other side: each counts independently, and both hand index
/// 0 to a different merchant's customer.
#[tokio::test]
#[ignore]
async fn an_xpub_another_account_holds_is_refused() {
    let service = create_test_service().await;
    let xpub_a = unique_xpub("a");
    let theirs = seed_user(&service).await;
    let mine = seed_user(&service).await;

    WalletWriter::create_wallet(&service, theirs, EVM, &xpub_a, None)
        .await
        .expect("they register it first");

    let err = WalletWriter::create_wallet(&service, mine, EVM, &xpub_a, None)
        .await
        .expect_err("a key already registered elsewhere must be refused");
    assert!(
        matches!(err, crate::RepositoryError::Conflict(_)),
        "must be a Conflict so the API can answer 409, got {err:?}"
    );

    // The same refusal has to hold on the other way in - configuring a payment
    // method by pasting an xpub - or the guard is trivially bypassed.
    let my_store = seed_store_for(&service, mine).await;
    let err = StorePaymentMethodWriter::create_payment_method(
        &service,
        my_store,
        &ChainId::evm(1),
        None,
        "ETH",
        18,
        Some(&xpub_a),
    )
    .await
    .expect_err("configuring a method with another account's key must be refused");
    assert!(
        matches!(err, crate::RepositoryError::Conflict(_)),
        "got {err:?}"
    );

    assert_eq!(
        WalletReader::list_wallets(&service, mine)
            .await
            .unwrap()
            .len(),
        0,
        "no wallet may have been created on the second account"
    );
}

/// Concurrent first-wallet creates on a fresh account must not race into a
/// unique violation.
///
/// `is_primary` is decided with `NOT EXISTS(...)` inside the insert, while the
/// insert's own conflict target is `(user_id, xpub)` - a different index from
/// the one enforcing a single primary. Two creates that both evaluate that to
/// true would otherwise collide on an index they were not conflicting against,
/// and the loser would surface as a 500. That is the ordinary "enable ETH,
/// enable USDC" flow.
#[tokio::test]
#[ignore]
async fn concurrent_first_wallet_creates_do_not_collide_on_primary() {
    let service = create_test_service().await;
    let xpub_a = unique_xpub("a");
    let xpub_b = unique_xpub("b");
    let user = seed_user(&service).await;
    let service = std::sync::Arc::new(service);

    let mut handles = Vec::new();
    for xpub in [xpub_a, xpub_b] {
        let svc = service.clone();
        handles.push(tokio::spawn(async move {
            WalletWriter::create_wallet(&*svc, user, EVM, &xpub, None).await
        }));
    }

    for h in handles {
        h.await
            .expect("join")
            .expect("neither create may fail - one of them losing is a 500");
    }

    let wallets = WalletReader::list_wallets(&*service, user).await.unwrap();
    assert_eq!(wallets.len(), 2);
    assert_eq!(
        wallets.iter().filter(|w| w.is_primary).count(),
        1,
        "exactly one primary must survive the race"
    );
}

// =========================================================================
// The override decides where money goes
// =========================================================================

/// Pinning a store to a wallet must change the addresses it derives, not just
/// what the settings page reports.
#[tokio::test]
#[ignore]
async fn setting_a_store_override_changes_where_derivation_happens() {
    let service = create_test_service().await;
    let xpub_a = unique_xpub("a");
    let xpub_b = unique_xpub("b");
    let user = seed_user(&service).await;
    let store = seed_store_for(&service, user).await;

    // Configured the ordinary way: the method is pinned to the key that was
    // pasted, exactly as the migration leaves existing methods.
    let method = StorePaymentMethodWriter::create_payment_method(
        &service,
        store,
        &ChainId::evm(1),
        None,
        "ETH",
        18,
        Some(&xpub_a),
    )
    .await
    .unwrap();
    let original = method.wallet_id.expect("resolves");

    let first = StorePaymentMethodWriter::allocate_derivation(&service, method.id)
        .await
        .unwrap();
    assert_eq!(first.wallet_id, original);
    assert_eq!(first.xpub, xpub_a);

    // Give the store its own wallet.
    let other = WalletWriter::create_wallet(&service, user, EVM, &xpub_b, Some("other"))
        .await
        .unwrap();
    WalletWriter::set_store_wallet(&service, store, other.id)
        .await
        .unwrap();

    let after = StorePaymentMethodWriter::allocate_derivation(&service, method.id)
        .await
        .expect("still derivable");
    assert_eq!(
        after.wallet_id, other.id,
        "the override must decide derivation, not merely be reported by GET"
    );
    assert_eq!(
        after.xpub, xpub_b,
        "an override that leaves the old key in use is worse than no override: \
         the merchant is told their money moved and it did not"
    );

    // And the read path must agree with what derivation just did.
    let reread = StorePaymentMethodReader::get_payment_method(&service, method.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reread.wallet_id, Some(other.id));
    assert_eq!(reread.xpub.as_deref(), Some(xpub_b.as_str()));

    // Clearing it falls back to the account primary, which is still &xpub_a.
    WalletWriter::clear_store_wallet(&service, store, EVM)
        .await
        .unwrap();
    let back = StorePaymentMethodWriter::allocate_derivation(&service, method.id)
        .await
        .unwrap();
    assert_eq!(
        back.wallet_id, original,
        "with the override gone the store follows the account primary"
    );
}

/// A pin survives a primary change elsewhere on the account - and that is
/// exactly what makes `resolve_store_wallet` an unsafe stand-in for "the
/// wallet this method derives from".
///
/// `set_store_wallet` unpins every method in its family precisely so an
/// explicit override is not cosmetic (see the test above). Promoting a
/// *different* wallet to primary has no such release: nothing walks the
/// store's payment methods when `is_primary` moves, because most of them are
/// meant to follow it. A method pinned by its own xpub is the one case that
/// must not, and does not - which means the moment a second wallet becomes
/// primary, the store's bare resolution and this method's actual resolution
/// name two different wallets, indefinitely, with nothing that reconciles
/// them again.
#[tokio::test]
#[ignore]
async fn a_pin_outlives_a_primary_change_and_diverges_from_the_store() {
    let service = create_test_service().await;
    let xpub_a = unique_xpub("a");
    let xpub_b = unique_xpub("b");
    let user = seed_user(&service).await;
    let store = seed_store_for(&service, user).await;

    // Configured by pasting a key, same as the e2e merchant flow: the method
    // is pinned to whatever wallet that key resolves to, which - being the
    // account's first wallet in this family - is also the primary. Bare
    // store resolution and the method's own resolution agree, for now.
    let method = StorePaymentMethodWriter::create_payment_method(
        &service,
        store,
        &ChainId::evm(1),
        None,
        "ETH",
        18,
        Some(&xpub_a),
    )
    .await
    .unwrap();
    let pinned_wallet = method.wallet_id.expect("resolves");

    let mut seen = HashSet::new();
    for _ in 0..3 {
        let allocation = StorePaymentMethodWriter::allocate_derivation(&service, method.id)
            .await
            .unwrap();
        assert_eq!(allocation.wallet_id, pinned_wallet);
        assert!(seen.insert(allocation.index));
    }

    // A second wallet appears on the account - not pinned to this store, not
    // even touching it - and is later made primary. Neither step is store
    // configuration; both are account-level actions the store's owner can
    // take for reasons that have nothing to do with this method.
    let second = WalletWriter::create_wallet(&service, user, EVM, &xpub_b, Some("second"))
        .await
        .unwrap();
    assert!(
        !second.is_primary,
        "the first wallet keeps primary until asked"
    );
    WalletWriter::set_primary_wallet(&service, user, second.id)
        .await
        .unwrap();

    // The store's bare resolution followed the primary, as documented.
    assert_eq!(
        WalletReader::resolve_store_wallet(&service, store, EVM)
            .await
            .unwrap()
            .unwrap()
            .id,
        second.id,
        "with no override the store follows whichever wallet is primary now"
    );

    // The pinned method did not move, and must not have: it was never asked
    // to. Re-reading it and allocating from it again both have to agree with
    // where it always derived, not with what the store now reports.
    let reread = StorePaymentMethodReader::get_payment_method(&service, method.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reread.wallet_id,
        Some(pinned_wallet),
        "a pin is not a snapshot of the primary at creation time - it must \
         survive the primary moving to somewhere else"
    );

    let after = StorePaymentMethodWriter::allocate_derivation(&service, method.id)
        .await
        .unwrap();
    assert_eq!(
        after.wallet_id, pinned_wallet,
        "allocation must keep deriving from the pin, not from whatever the \
         store now resolves to"
    );
    assert!(
        seen.insert(after.index),
        "index {} was reissued after the primary changed - the exact hazard \
         a static-looking counter on the wrong wallet hides",
        after.index
    );

    // The two resolutions of "this store's wallet" now genuinely disagree
    // (`second` above, `pinned_wallet` here) - not a bug in either alone, but
    // proof that the store's bare resolution is the wrong question to ask
    // about a pinned method. `GET /stores/{id}/wallet?payment_method_id=...`
    // asks the right one instead.
}

/// A method with nothing to resolve to is listed, not hidden, and refuses to
/// allocate rather than inventing a key.
#[tokio::test]
#[ignore]
async fn a_method_with_no_resolvable_wallet_is_visible_but_cannot_allocate() {
    let service = create_test_service().await;
    let xpub_a = unique_xpub("a");
    let user = seed_user(&service).await;
    let store = seed_store_for(&service, user).await;

    let method = StorePaymentMethodWriter::create_payment_method(
        &service,
        store,
        &ChainId::evm(1),
        None,
        "ETH",
        18,
        Some(&xpub_a),
    )
    .await
    .unwrap();

    // Unpin it and remove every fallback.
    sqlx::query("UPDATE store_payment_methods SET wallet_id = NULL WHERE id = $1")
        .bind(method.id)
        .execute(service.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE wallets SET is_primary = FALSE WHERE user_id = $1")
        .bind(user)
        .execute(service.pool())
        .await
        .unwrap();

    let listed = StorePaymentMethodReader::get_payment_methods(&service, store)
        .await
        .unwrap();
    assert_eq!(
        listed.len(),
        1,
        "an unresolvable method must still be listed - a merchant has to be \
         able to see the thing they created in order to fix it"
    );
    assert!(listed[0].wallet_id.is_none());
    assert!(listed[0].xpub.is_none());

    assert!(
        StorePaymentMethodWriter::allocate_derivation(&service, method.id)
            .await
            .is_err(),
        "with no key to derive from, allocation must fail rather than guess"
    );
}

/// Allocation must return the key belonging to the counter it moved.
///
/// A caller that reads the method, then allocates, then derives from the xpub
/// it read is pairing two different wallets whenever anything committed in
/// between. Here the rotation is the "in between": the stale read still says
/// &xpub_a, and the allocation must not.
#[tokio::test]
#[ignore]
async fn allocation_returns_the_key_of_the_wallet_whose_counter_moved() {
    let service = create_test_service().await;
    let xpub_a = unique_xpub("a");
    let xpub_b = unique_xpub("b");
    let user = seed_user(&service).await;
    let store = seed_store_for(&service, user).await;

    let stale = StorePaymentMethodWriter::create_payment_method(
        &service,
        store,
        &ChainId::evm(1),
        None,
        "ETH",
        18,
        Some(&xpub_a),
    )
    .await
    .unwrap();
    assert_eq!(stale.xpub.as_deref(), Some(xpub_a.as_str()));

    service
        .rotate_payment_method_xpub(store, stale.id, EVM, &xpub_b, Some("test"))
        .await
        .expect("rotate");

    let allocation = StorePaymentMethodWriter::allocate_derivation(&service, stale.id)
        .await
        .unwrap();
    assert_eq!(
        allocation.xpub, xpub_b,
        "the allocation must carry the rotated key, not the one a caller read \
         before the rotation - deriving from the stale pair burns an index on \
         one wallet and hands out an address the other will issue again"
    );

    // And the index must have come from that same wallet's counter.
    //
    // Read the wallet the allocation names, not the store's resolution: a
    // rotation pins the method, and a pin outranks the store. The store here
    // still resolves to the account primary, which is the key that was rotated
    // away from - so asserting against it would be asserting the wrong wallet.
    let b = WalletReader::get_wallet(&service, allocation.wallet_id)
        .await
        .unwrap()
        .expect("the allocation names a real wallet");
    assert_eq!(b.xpub, xpub_b);
    assert_eq!(
        b.derivation_index,
        allocation.index + 1,
        "the counter that advanced must be the one the key came from"
    );
}

/// Rotation moves the store override too, when it named the key being retired.
#[tokio::test]
#[ignore]
async fn rotation_moves_a_store_override_off_the_retired_key() {
    let service = create_test_service().await;
    let xpub_a = unique_xpub("a");
    let xpub_b = unique_xpub("b");
    let user = seed_user(&service).await;
    let store = seed_store_for(&service, user).await;

    let method = StorePaymentMethodWriter::create_payment_method(
        &service,
        store,
        &ChainId::evm(1),
        None,
        "ETH",
        18,
        Some(&xpub_a),
    )
    .await
    .unwrap();
    let old = method.wallet_id.unwrap();
    WalletWriter::set_store_wallet(&service, store, old)
        .await
        .unwrap();

    service
        .rotate_payment_method_xpub(store, method.id, EVM, &xpub_b, Some("compromise"))
        .await
        .unwrap();

    let resolved = WalletReader::resolve_store_wallet(&service, store, EVM)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        resolved.xpub, xpub_b,
        "leaving the override on the retired key means anything unpinned \
         resolves straight back to the xpub that was just rotated away"
    );
}

/// Provenance must never be what stops a wallet being deleted.
#[tokio::test]
#[ignore]
async fn a_wallet_is_deletable_once_only_history_refers_to_it() {
    let service = create_test_service().await;
    let xpub_a = unique_xpub("a");
    let user = seed_user(&service).await;
    let store = seed_store_for(&service, user).await;

    let method = StorePaymentMethodWriter::create_payment_method(
        &service,
        store,
        &ChainId::evm(1),
        None,
        "ETH",
        18,
        Some(&xpub_a),
    )
    .await
    .unwrap();
    let wallet_id = method.wallet_id.unwrap();

    let invoice_id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO invoices (id, store_id, currency, amount, expires_at) \
         VALUES ($1, $2, 'USD', 100, NOW() + interval '1 hour')",
    )
    .bind(&invoice_id)
    .bind(store)
    .execute(service.pool())
    .await
    .unwrap();

    let allocation = StorePaymentMethodWriter::allocate_derivation(&service, method.id)
        .await
        .unwrap();
    let option = types::PaymentOptionData {
        id: types::PaymentOptionId::new(),
        invoice_id: types::InvoiceId::from_string(invoice_id),
        payment_method_id: types::PaymentMethodId::new("ETH", &ChainId::evm(1)),
        chain_id: ChainId::evm(1),
        asset_symbol: "ETH".to_string(),
        token_address: None,
        decimals: 18,
        payment_address: format!("0x{:040x}", Uuid::new_v4().as_u128()),
        wallet_id: Some(allocation.wallet_id),
        derivation_index: Some(allocation.index),
        amount: "1".to_string(),
        rate: None,
        rate_at: None,
        is_active: true,
        created_at: chrono::Utc::now(),
    };
    types::PaymentOptionWriter::create(&service, &option)
        .await
        .unwrap();

    // Remove the live references, leaving only the payment option's record of
    // history behind.
    StorePaymentMethodWriter::delete_payment_method(&service, method.id)
        .await
        .unwrap();
    WalletWriter::clear_store_wallet(&service, store, EVM)
        .await
        .unwrap();

    WalletWriter::delete_wallet(&service, wallet_id)
        .await
        .expect(
            "history must not pin a wallet forever behind a misleading \
             'still in use'",
        );

    let after = types::PaymentOptionReader::get(&service, &option.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.wallet_id, None,
        "provenance degrades to unknown, which the column already means"
    );
    assert_eq!(
        after.payment_address, option.payment_address,
        "the address itself is authoritative and must survive"
    );
}

/// Adding the same native asset twice updates one row rather than creating a
/// second - the NULL gap in the composite unique index.
#[tokio::test]
#[ignore]
async fn re_adding_a_native_asset_updates_rather_than_duplicating() {
    let service = create_test_service().await;
    let xpub_a = unique_xpub("a");
    let user = seed_user(&service).await;
    let store = seed_store_for(&service, user).await;

    let first = StorePaymentMethodWriter::create_payment_method(
        &service,
        store,
        &ChainId::evm(1),
        None,
        "ETH",
        18,
        Some(&xpub_a),
    )
    .await
    .unwrap();
    let second = StorePaymentMethodWriter::create_payment_method(
        &service,
        store,
        &ChainId::evm(1),
        None,
        "ETH",
        18,
        Some(&xpub_a),
    )
    .await
    .unwrap();

    assert_eq!(
        first.id, second.id,
        "token_address is NULL for native assets and NULL is distinct from \
         NULL, so without the partial unique index this inserts a second row \
         - a second method on the same asset, and formerly a second counter"
    );
    assert_eq!(
        StorePaymentMethodReader::get_payment_methods(&service, store)
            .await
            .unwrap()
            .len(),
        1
    );
}

/// Rotating a whole store records one rotation per method and no fabricated
/// ones.
///
/// Rotating method by method moved the store override on the first iteration,
/// so every inheriting method after it resolved to the NEW wallet and recorded
/// a rotation from the new key to itself. `wallet_rotations` is the table the
/// migration trusts to tell an honest provenance stamp from a guess, and those
/// rows are neither honest nor harmless.
#[tokio::test]
#[ignore]
async fn rotating_a_store_records_no_rotation_from_a_key_to_itself() {
    let service = create_test_service().await;
    let xpub_a = unique_xpub("a");
    let xpub_b = unique_xpub("b");
    let user = seed_user(&service).await;
    let store = seed_store_for(&service, user).await;

    for (chain, token, symbol) in [
        (1u64, None, "ETH"),
        (1, Some("0xtok"), "USDC"),
        (137, None, "MATIC"),
    ] {
        StorePaymentMethodWriter::create_payment_method(
            &service,
            store,
            &ChainId::evm(chain),
            token,
            symbol,
            18,
            Some(&xpub_a),
        )
        .await
        .unwrap();
    }

    // Hand the store its own wallet, which unpins all three methods - the
    // state that made every iteration after the first record a self-rotation.
    let wallet_a = WalletReader::resolve_store_wallet(&service, store, EVM)
        .await
        .unwrap()
        .unwrap();
    WalletWriter::set_store_wallet(&service, store, wallet_a.id)
        .await
        .unwrap();

    let rotations = service
        .rotate_store_xpub(store, EVM, &xpub_b, Some("compromise"))
        .await
        .expect("rotate the store");

    assert_eq!(rotations.len(), 3, "every method rotates exactly once");
    for rotation in &rotations {
        assert_eq!(
            rotation.previous_xpub, xpub_a,
            "a rotation from the new key to itself is a fabricated audit row"
        );
    }

    let history = service.get_rotation_history(store).await.unwrap();
    assert_eq!(
        history.len(),
        3,
        "the audit trail must hold one row per method, not one per method plus \
         one per method the override overtook"
    );

    let resolved = WalletReader::resolve_store_wallet(&service, store, EVM)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        resolved.xpub, xpub_b,
        "the store must end up deriving from the new key"
    );
}

/// The rate count counts rotation events, not the rows they write.
///
/// One store-wide rotation writes one row per method, all stamped with the same
/// transaction time. Counting rows would read a single rotation over three
/// methods as three, and trip the rate alert on a routine change.
#[tokio::test]
#[ignore]
async fn rotation_rate_count_counts_events_not_method_rows() {
    let service = create_test_service().await;
    let xpub_a = unique_xpub("a");
    let user = seed_user(&service).await;
    let store = seed_store_for(&service, user).await;

    for (chain, token, symbol) in [
        (1u64, None, "ETH"),
        (1, Some("0xtok"), "USDC"),
        (137, None, "MATIC"),
    ] {
        StorePaymentMethodWriter::create_payment_method(
            &service,
            store,
            &ChainId::evm(chain),
            token,
            symbol,
            18,
            Some(&xpub_a),
        )
        .await
        .unwrap();
    }

    let wallet_a = WalletReader::resolve_store_wallet(&service, store, EVM)
        .await
        .unwrap()
        .unwrap();
    WalletWriter::set_store_wallet(&service, store, wallet_a.id)
        .await
        .unwrap();

    let since = chrono::Utc::now() - chrono::Duration::hours(24);
    assert_eq!(
        service
            .count_rotation_events_since(store, since)
            .await
            .unwrap(),
        0,
        "no rotation yet"
    );

    service
        .rotate_store_xpub(store, EVM, &unique_xpub("b"), Some("first"))
        .await
        .expect("first rotation");
    assert_eq!(
        service
            .count_rotation_events_since(store, since)
            .await
            .unwrap(),
        1,
        "one rotation over three methods is one event"
    );

    service
        .rotate_store_xpub(store, EVM, &unique_xpub("c"), Some("second"))
        .await
        .expect("second rotation");
    assert_eq!(
        service
            .count_rotation_events_since(store, since)
            .await
            .unwrap(),
        2,
        "two rotations are two events"
    );

    let after_both = chrono::Utc::now() + chrono::Duration::hours(1);
    assert_eq!(
        service
            .count_rotation_events_since(store, after_both)
            .await
            .unwrap(),
        0,
        "the window is a lower bound on rotated_at"
    );
}

/// Rotating one store does not move any other store on the account.
///
/// A store with no override follows the account primary. Rotating it by
/// changing that primary would silently repoint every sibling store; the
/// rotated store gets an override of its own instead.
#[tokio::test]
#[ignore]
async fn rotating_one_store_leaves_its_siblings_where_they_were() {
    let service = create_test_service().await;
    let xpub_a = unique_xpub("a");
    let xpub_b = unique_xpub("b");
    let user = seed_user(&service).await;
    let rotated = seed_store_for(&service, user).await;
    let sibling = seed_store_for(&service, user).await;

    // Both stores derive from the same account primary, neither pinned.
    StorePaymentMethodWriter::create_payment_method(
        &service,
        rotated,
        &ChainId::evm(1),
        None,
        "ETH",
        18,
        Some(&xpub_a),
    )
    .await
    .unwrap();
    StorePaymentMethodWriter::create_payment_method(
        &service,
        sibling,
        &ChainId::evm(1),
        None,
        "ETH",
        18,
        Some(&xpub_a),
    )
    .await
    .unwrap();
    WalletWriter::clear_store_wallet(&service, rotated, EVM)
        .await
        .unwrap();
    WalletWriter::clear_store_wallet(&service, sibling, EVM)
        .await
        .unwrap();

    service
        .rotate_store_xpub(rotated, EVM, &xpub_b, Some("compromise"))
        .await
        .expect("rotate one store");

    let moved = WalletReader::resolve_store_wallet(&service, rotated, EVM)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(moved.xpub, xpub_b, "the rotated store moves");

    let untouched = WalletReader::resolve_store_wallet(&service, sibling, EVM)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        untouched.xpub, xpub_a,
        "rotating one store must not repoint every other store on the account - \
         that is what changing the account primary would have done"
    );
}

/// Rotating onto a key another account holds is refused, and changes nothing.
///
/// The refusal already existed for `POST /wallets`; the point here is that the
/// whole-store rotation is one transaction, so a rejection partway through
/// leaves no method moved. A per-method loop left the store split across the
/// compromised key and the new one.
#[tokio::test]
#[ignore]
async fn a_refused_rotation_leaves_the_store_entirely_unmoved() {
    let service = create_test_service().await;
    let mine = unique_xpub("mine");
    let theirs = unique_xpub("theirs");

    let other_user = seed_user(&service).await;
    WalletWriter::create_wallet(&service, other_user, EVM, &theirs, None)
        .await
        .unwrap();

    let user = seed_user(&service).await;
    let store = seed_store_for(&service, user).await;
    for (chain, symbol) in [(1u64, "ETH"), (137, "MATIC")] {
        StorePaymentMethodWriter::create_payment_method(
            &service,
            store,
            &ChainId::evm(chain),
            None,
            symbol,
            18,
            Some(&mine),
        )
        .await
        .unwrap();
    }

    let err = service
        .rotate_store_xpub(store, EVM, &theirs, Some("compromise"))
        .await
        .expect_err("another account holds that key");
    assert!(
        matches!(err, crate::RepositoryError::Conflict(_)),
        "the caller has to be able to tell this from a server fault: {err:?}"
    );

    let methods = StorePaymentMethodReader::get_payment_methods(&service, store)
        .await
        .unwrap();
    assert_eq!(methods.len(), 2);
    for method in &methods {
        assert_eq!(
            method.xpub.as_deref(),
            Some(mine.as_str()),
            "a refused rotation must leave every method where it was, not some \
             of them on the key the merchant just declared compromised"
        );
    }

    assert!(
        service
            .get_rotation_history(store)
            .await
            .unwrap()
            .is_empty(),
        "nothing moved, so nothing is recorded as having moved"
    );
}

// =========================================================================
// Paste the key once
//
// A payment method used to require its own xpub, so the same key was retyped
// per chain, per token and per store. These cover the case where it is omitted
// and the method follows the store's resolution instead.
// =========================================================================

#[tokio::test]
#[ignore]
async fn a_method_with_no_key_uses_the_account_primary() {
    let service = create_test_service().await;
    let xpub = unique_xpub("primary");
    let user = seed_user(&service).await;
    let store = seed_store_for(&service, user).await;

    // The key arrives once, as a wallet on the account.
    let wallet = WalletWriter::create_wallet(&service, user, EVM, &xpub, Some("main"))
        .await
        .expect("create wallet");

    let method = StorePaymentMethodWriter::create_payment_method(
        &service,
        store,
        &ChainId::evm(1),
        None,
        "ETH",
        18,
        None,
    )
    .await
    .expect("create method without a key");

    assert_eq!(
        method.wallet_id,
        Some(wallet.id),
        "an unpinned method must read through to the account primary"
    );

    // And it can actually derive, which is the thing that matters.
    let allocation = StorePaymentMethodWriter::allocate_derivation(&service, method.id)
        .await
        .expect("allocate from the inherited key");
    assert_eq!(allocation.wallet_id, wallet.id);
    assert_eq!(allocation.xpub, xpub);
}

#[tokio::test]
#[ignore]
async fn a_method_with_no_key_is_refused_when_nothing_resolves() {
    // The failure this moves earlier. Without the check the method is created,
    // looks correct in the list, and fails at the first invoice - with a
    // customer waiting.
    let service = create_test_service().await;
    let user = seed_user(&service).await;
    let store = seed_store_for(&service, user).await;

    let result = StorePaymentMethodWriter::create_payment_method(
        &service,
        store,
        &ChainId::evm(1),
        None,
        "ETH",
        18,
        None,
    )
    .await;

    match result {
        Err(RepositoryError::Conflict(msg)) => {
            assert!(
                msg.contains("receiving key"),
                "the refusal must say what is missing, got: {msg}"
            );
        }
        Err(other) => panic!("expected a Conflict the merchant can act on, got {other:?}"),
        Ok(_) => panic!("a method with no key and nothing to resolve to must be refused"),
    }
}

#[tokio::test]
#[ignore]
async fn an_unpinned_method_follows_the_store_wallet() {
    // Unpinned means it tracks resolution rather than freezing today's answer:
    // point the store at another key and the method moves with it.
    let service = create_test_service().await;
    let user = seed_user(&service).await;
    let store = seed_store_for(&service, user).await;

    let primary = WalletWriter::create_wallet(&service, user, EVM, &unique_xpub("p"), None)
        .await
        .expect("primary");
    let other = WalletWriter::create_wallet(&service, user, EVM, &unique_xpub("o"), None)
        .await
        .expect("other");

    let method = StorePaymentMethodWriter::create_payment_method(
        &service,
        store,
        &ChainId::evm(1),
        None,
        "ETH",
        18,
        None,
    )
    .await
    .expect("create method");
    assert_eq!(method.wallet_id, Some(primary.id));

    WalletWriter::set_store_wallet(&service, store, other.id)
        .await
        .expect("point the store at the other key");

    let reread = StorePaymentMethodReader::get_enabled_payment_methods(&service, store)
        .await
        .expect("reread");
    assert_eq!(
        reread[0].wallet_id,
        Some(other.id),
        "an unpinned method must follow the store's wallet, not the one it saw at creation"
    );
}

// =========================================================================
// Chain families
// =========================================================================

/// Insert a payment method directly, bypassing `create_payment_method`.
///
/// Needed because the repository now refuses to create an unpinned method on a
/// family the store cannot resolve - which is the defence, and which means the
/// state under test here cannot be reached through the front door. A row like
/// this is still reachable: an account can register a Tron key, enable Tron,
/// and later delete or re-primary that key.
async fn seed_unpinned_method(
    service: &PgDataService,
    store_id: Uuid,
    chain_id: &ChainId,
    symbol: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO store_payment_methods \
         (id, store_id, chain_id, token_address, asset_symbol, decimals, wallet_id) \
         VALUES ($1, $2, $3, NULL, $4, 6, NULL)",
    )
    .bind(id)
    .bind(store_id)
    .bind(chain_id.as_str())
    .bind(symbol)
    .execute(service.pool())
    .await
    .expect("seed payment method");
    id
}

/// A `tron:` method on a store whose account holds only an Ethereum key must
/// fail to allocate, not derive from that key.
///
/// The whole ticket in one assertion. An account xpub has its BIP-44 coin type
/// baked in - 60 for the key this store has - and Tron's is 195. Deriving from
/// it anyway succeeds at every step: the bytes are a valid secp256k1 address
/// and base58check renders them as a checksum-correct `T...` string. The
/// merchant's Tron wallet, which derives at `m/44'/195'`, never shows the
/// money that arrives there.
///
/// `NotFound` is what `server/src/api/invoices/payment_options.rs` turns into
/// the 409 `no_receiving_key` a merchant can act on.
///
/// The Ethereum method on the SAME store is asserted to still allocate. That
/// is what makes this a test about families rather than about a broken store:
/// remove the namespace filter from resolution and the Tron method starts
/// allocating from the Ethereum wallet - the failure this exists to catch -
/// while a test that only checked "Tron fails" would also pass if resolution
/// had simply stopped working.
#[tokio::test]
#[ignore]
async fn a_tron_method_will_not_derive_from_an_ethereum_wallet() {
    let service = create_test_service().await;
    let user = seed_user(&service).await;
    let store = seed_store_for(&service, user).await;

    let evm_wallet = WalletWriter::create_wallet(&service, user, EVM, &unique_xpub("evm"), None)
        .await
        .expect("register an ethereum wallet");

    let tron_method = seed_unpinned_method(
        &service,
        store,
        &ChainId::parse("tron:728126428").unwrap(),
        "USDT",
    )
    .await;
    let evm_method = seed_unpinned_method(&service, store, &ChainId::evm(1), "ETH").await;

    let refused = StorePaymentMethodWriter::allocate_derivation(&service, tron_method).await;
    assert!(
        matches!(refused, Err(RepositoryError::NotFound(_))),
        "a tron method resolved to a wallet on an account that holds only an \
         ethereum key: {refused:?}"
    );

    let allocated = StorePaymentMethodWriter::allocate_derivation(&service, evm_method)
        .await
        .expect("the ethereum method on the same store must still allocate");
    assert_eq!(allocated.wallet_id, evm_wallet.id);
    assert_eq!(allocated.namespace, EVM);

    // And the counter was not burnt by the refusal. A wallet that advanced for
    // an allocation nobody received would leak addresses the merchant is still
    // watching for.
    let after = WalletReader::get_wallet(&service, evm_wallet.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.derivation_index, 1);
}

/// Once the account holds a Tron key, the same method allocates from it - and
/// from that one only.
///
/// The other half: the refusal above has to be "no key for this family", not
/// "Tron is refused". Two wallets, two independent counters, and each method
/// reaches its own.
#[tokio::test]
#[ignore]
async fn each_family_allocates_from_its_own_wallet() {
    let service = create_test_service().await;
    let user = seed_user(&service).await;
    let store = seed_store_for(&service, user).await;

    let evm_wallet = WalletWriter::create_wallet(&service, user, EVM, &unique_xpub("evm"), None)
        .await
        .expect("register an ethereum wallet");
    let tron_wallet = WalletWriter::create_wallet(&service, user, TRON, &unique_xpub("tron"), None)
        .await
        .expect("register a tron wallet");

    // The first wallet in each family is that family's primary, so neither
    // needs promoting and neither demoted the other.
    assert!(evm_wallet.is_primary && tron_wallet.is_primary);

    let tron_method = seed_unpinned_method(
        &service,
        store,
        &ChainId::parse("tron:728126428").unwrap(),
        "USDT",
    )
    .await;
    let evm_method = seed_unpinned_method(&service, store, &ChainId::evm(1), "ETH").await;

    let from_tron = StorePaymentMethodWriter::allocate_derivation(&service, tron_method)
        .await
        .expect("allocate on tron");
    let from_evm = StorePaymentMethodWriter::allocate_derivation(&service, evm_method)
        .await
        .expect("allocate on ethereum");

    assert_eq!(from_tron.wallet_id, tron_wallet.id);
    assert_eq!(from_tron.namespace, TRON);
    assert_eq!(from_evm.wallet_id, evm_wallet.id);
    assert_eq!(from_evm.namespace, EVM);

    // Separate counters. Both start at 0 precisely because they are different
    // keys on different chains: sharing a counter would be the only thing
    // worth avoiding here, and sharing an index is not.
    assert_eq!(from_tron.index, 0);
    assert_eq!(from_evm.index, 0);
    assert_ne!(from_tron.xpub, from_evm.xpub);
}

/// The same bytes registered for two families are two wallets, not one.
///
/// `create_wallet` documents that re-registering a key returns the existing
/// row. Keyed on the account and the key alone, a merchant who pastes the same
/// xpub for Tron that they already registered for Ethereum is handed the
/// Ethereum wallet back - and every Tron address quoted afterwards is derived
/// at coin type 60.
#[tokio::test]
#[ignore]
async fn one_xpub_registered_for_two_families_is_two_wallets() {
    let service = create_test_service().await;
    let user = seed_user(&service).await;
    let shared = unique_xpub("shared");

    let as_evm = WalletWriter::create_wallet(&service, user, EVM, &shared, Some("eth"))
        .await
        .expect("register for ethereum");
    let as_tron = WalletWriter::create_wallet(&service, user, TRON, &shared, Some("tron"))
        .await
        .expect("register the same key for tron");

    assert_ne!(
        as_evm.id, as_tron.id,
        "registering a key for tron returned the account's ethereum wallet"
    );
    assert_eq!(as_tron.namespace, TRON);

    // Re-registering within one family still returns the existing row, which
    // is the property that stops a second counter appearing on one key.
    let again = WalletWriter::create_wallet(&service, user, EVM, &shared, None)
        .await
        .expect("re-register for ethereum");
    assert_eq!(again.id, as_evm.id);
}

/// Pinning a store to a wallet says nothing about its other families.
///
/// `set_store_wallet` releases a store's payment methods so the override
/// actually decides where money goes. Released account-wide, "pin this store
/// to my Tron wallet" would unpin the store's ETH and USDC methods too and
/// hand them to the account's Ethereum primary - a change of where real money
/// is collected, ordered by a request that named a different chain.
#[tokio::test]
#[ignore]
async fn pinning_a_store_for_one_family_leaves_the_others_alone() {
    let service = create_test_service().await;
    let user = seed_user(&service).await;
    let store = seed_store_for(&service, user).await;

    let evm_primary = WalletWriter::create_wallet(&service, user, EVM, &unique_xpub("p"), None)
        .await
        .expect("ethereum primary");
    let evm_pinned = WalletWriter::create_wallet(&service, user, EVM, &unique_xpub("q"), None)
        .await
        .expect("a second ethereum wallet");
    let tron = WalletWriter::create_wallet(&service, user, TRON, &unique_xpub("t"), None)
        .await
        .expect("tron wallet");

    // An ETH method pinned to the non-primary Ethereum wallet.
    let eth = StorePaymentMethodWriter::create_payment_method(
        &service,
        store,
        &ChainId::evm(1),
        None,
        "ETH",
        18,
        Some(&evm_pinned.xpub),
    )
    .await
    .expect("create eth method");
    assert_eq!(eth.wallet_id, Some(evm_pinned.id));

    WalletWriter::set_store_wallet(&service, store, tron.id)
        .await
        .expect("pin the store to its tron wallet");

    let still_pinned = StorePaymentMethodReader::get_payment_method(&service, eth.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        still_pinned.wallet_id,
        Some(evm_pinned.id),
        "pinning a tron wallet moved where this store's ETH is collected"
    );

    // The store's Ethereum resolution is untouched too: still the account
    // primary, not the wallet the tron override names.
    let resolved = WalletReader::resolve_store_wallet(&service, store, EVM)
        .await
        .unwrap()
        .expect("ethereum still resolves");
    assert_eq!(resolved.id, evm_primary.id);

    let tron_resolved = WalletReader::resolve_store_wallet(&service, store, TRON)
        .await
        .unwrap()
        .expect("tron resolves to the override");
    assert_eq!(tron_resolved.id, tron.id);
}

/// Promoting a wallet demotes its own family's primary and no other.
///
/// An account-wide demotion would leave every EVM store with no primary the
/// moment a merchant made their first Tron wallet the main one, and they would
/// find out when a customer tried to pay in ETH.
#[tokio::test]
#[ignore]
async fn promoting_a_wallet_only_demotes_its_own_family() {
    let service = create_test_service().await;
    let user = seed_user(&service).await;

    let evm = WalletWriter::create_wallet(&service, user, EVM, &unique_xpub("e"), None)
        .await
        .expect("ethereum wallet");
    let tron_first = WalletWriter::create_wallet(&service, user, TRON, &unique_xpub("t1"), None)
        .await
        .expect("first tron wallet");
    let tron_second = WalletWriter::create_wallet(&service, user, TRON, &unique_xpub("t2"), None)
        .await
        .expect("second tron wallet");

    assert!(evm.is_primary);
    assert!(tron_first.is_primary);
    assert!(!tron_second.is_primary);

    WalletWriter::set_primary_wallet(&service, user, tron_second.id)
        .await
        .expect("promote the second tron wallet");

    assert_eq!(
        WalletReader::get_primary_wallet(&service, user, TRON)
            .await
            .unwrap()
            .map(|w| w.id),
        Some(tron_second.id)
    );
    assert_eq!(
        WalletReader::get_primary_wallet(&service, user, EVM)
            .await
            .unwrap()
            .map(|w| w.id),
        Some(evm.id),
        "promoting a tron wallet demoted the account's ethereum primary"
    );
}
