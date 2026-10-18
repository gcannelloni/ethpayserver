#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Review finding, fixed: the filter's own unit tests exercised
//! `run_invoice_creation_filters` in isolation, but nothing called the actual
//! `create_invoice` handler with a `Deny`-returning filter registered on
//! `AppState` and checked what a merchant would actually see. The behaviour
//! ("a filter refusing invoice creation actually blocks it, and the merchant
//! sees a reason naming the subscription") is about behavior observable
//! through the endpoint, not the filter runner alone - the status code, the
//! reason surfacing through `invoice_error` into the response body, and that
//! the filter runs *after* a real permission check has already passed rather
//! than being indistinguishable from an auth rejection.
//!
//! Calls the handler function directly against a real database rather than
//! through the router: `AuthenticatedCaller` and `State` are plain data the
//! extractors produce, so nothing about this assertion depends on routing or
//! middleware, only on `create_invoice`'s own body.
//!
//! Review finding, fixed: this comment used to claim the filter
//! "runs after a real permission check has already passed rather than being
//! indistinguishable from an auth rejection" without a test asserting it.
//! Both rejections actually share HTTP 403 - `permission_denies_before_the_filter_is_ever_consulted`
//! below is the ordering test, and it distinguishes them by the `error` code
//! in the body (`forbidden` vs `invoice_creation_blocked`), not by status.
//!
//! Review finding, fixed: every test above builds an `AuthenticatedCaller`
//! struct literal by hand, so none of them exercise the path an actual
//! request takes - a header, a database row, `validate_api_key`, and the
//! `AuthenticatedCaller` extractor's own `FromRequestParts` impl. A bug that
//! drops the column on the way from the row to the struct (wrong bind order,
//! a branch that forgets to forward it) would pass every test above while
//! leaving the deadlock this file exists to guard completely unprotected.
//! `operator_flag_on_a_real_api_key_row_reaches_the_extractor_and_exempts_the_request`
//! and `non_operator_api_key_row_is_still_filtered_through_the_real_extractor`
//! insert a real `api_keys` row, run the actual header through
//! `AuthenticatedCaller::from_request_parts`, and only then hand the result
//! to `create_invoice`.

use data_service::test_support::pg_service;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Json;
use axum::extract::{FromRequestParts, State};
use axum::http::{Request, StatusCode};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use auth::{
    Result as AuthResult, Role, Session, SessionId, SessionService, Store, UserId, UserInfo,
};
use data_service::PgDataService;
use data_service::store_creation::StoreCreationWriter;
use rates::NoOpRateProvider;
use server::api::AuthenticatedCaller;
use server::api::invoices::{CreateInvoiceRequest, create_invoice};
use server::services::RedisEVMMonitor;
use server::services::plugins::{
    FilterVerdict, InvoiceCreationFilter, InvoiceCreationFilterRequest,
};
use server::state::PgAppState;

/// Not exercised: `create_invoice` never calls back into session management,
/// only reads the already-authenticated `UserInfo` this test constructs
/// directly.
struct UnusedSessionService;

#[async_trait]
impl SessionService for UnusedSessionService {
    async fn validate_session(&self, _session_id: SessionId) -> AuthResult<(UserInfo, Session)> {
        unimplemented!("not exercised by create_invoice")
    }
    async fn logout(&self, _session_id: SessionId) -> AuthResult<()> {
        unimplemented!("not exercised by create_invoice")
    }
    async fn logout_all(&self, _session_id: SessionId) -> AuthResult<()> {
        unimplemented!("not exercised by create_invoice")
    }
    async fn cleanup_stale_sessions(&self) -> AuthResult<u64> {
        unimplemented!("not exercised by create_invoice")
    }
}

struct AlwaysDeny;

#[async_trait]
impl InvoiceCreationFilter for AlwaysDeny {
    async fn filter_invoice_creation(
        &self,
        _request: InvoiceCreationFilterRequest,
    ) -> FilterVerdict {
        FilterVerdict::Deny {
            reason: "Your subscription lapsed; renew it to create new invoices.".to_string(),
        }
    }
}

async fn seed_user(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    // `kdf_params`/`encrypted_symmetric_key` must deserialize into their real
    // structs, not just be valid JSON - the new extractor-level tests below
    // are the first ones in this file to read the row back through
    // `UserRepository::get_user` rather than only ever constructing a
    // `UserInfo` by hand, and a bare `{}` fails that deserialization.
    sqlx::query(
        "INSERT INTO users (id, kdf_params, encrypted_symmetric_key, \
         recovery_verification_hash, kdf_salt_identifier) \
         VALUES ($1, \
             '{\"algorithm\":\"argon2id\",\"memory_kb\":65536,\"iterations\":3,\"parallelism\":4,\"salt\":\"\"}'::jsonb, \
             '{\"ciphertext\":\"\",\"iv\":\"\",\"mac\":\"\"}'::jsonb, \
             'h', 'passkey:' || $1::text)",
    )
    .bind(id)
    .execute(pool)
    .await
    .expect("seed user");
    id
}

fn user_info(id: Uuid) -> UserInfo {
    user_info_with_role(id, Role::User)
}

fn user_info_with_role(id: Uuid, role: Role) -> UserInfo {
    UserInfo {
        id: UserId(id),
        email: None,
        primary_wallet_address: None,
        created_at: chrono::Utc::now(),
        last_login_at: None,
        role,
    }
}

/// Every credential in this suite is unscoped; only `is_operator`/role vary.
fn caller_as(user: UserInfo, is_operator: bool) -> AuthenticatedCaller {
    AuthenticatedCaller {
        user,
        is_operator,
        key_scope: None,
    }
}

/// The caller most tests want: an ordinary, non-operator credential.
fn caller(id: Uuid) -> AuthenticatedCaller {
    caller_as(user_info(id), false)
}

/// Inserts a real, active `api_keys` row and returns the raw key a request
/// would present as `Authorization: Bearer <raw>`. Hashes with the same
/// SHA-256-hex scheme `validate_api_key` looks the row up by, independently
/// of that function, so this test proves the two agree rather than assuming it.
async fn seed_api_key(pool: &PgPool, user_id: Uuid, is_operator: bool) -> String {
    let raw_key = format!("ak_test_{}", Uuid::new_v4());
    let key_hash = hex::encode(Sha256::digest(raw_key.as_bytes()));
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, name, key_hash, key_prefix, is_operator) \
         VALUES ($1, $2, 'test key', $3, 'ak_test', $4)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(&key_hash)
    .bind(is_operator)
    .execute(pool)
    .await
    .expect("seed api key");
    raw_key
}

/// Runs the real `AuthenticatedCaller` extractor against a request carrying
/// `raw_key`, the same way axum would for an incoming `/invoices` request.
async fn authenticate_with_api_key(
    raw_key: &str,
    state: &PgAppState<UnusedSessionService>,
) -> Result<AuthenticatedCaller, (StatusCode, &'static str)> {
    let request = Request::builder()
        .header("authorization", format!("Bearer {raw_key}"))
        .body(())
        .expect("build request");
    let (mut parts, ()) = request.into_parts();
    AuthenticatedCaller::from_request_parts(&mut parts, state).await
}

fn invoice_request(store_id: Uuid) -> CreateInvoiceRequest {
    CreateInvoiceRequest {
        store_id,
        currency: "USD".to_string(),
        amount: "10.00".to_string(),
        expiration_seconds: None,
        metadata: None,
        customer_email: None,
        webhook_url: None,
        redirect_url: None,
    }
}

fn app_state(
    data_service: Arc<PgDataService>,
    filters: Vec<Arc<dyn InvoiceCreationFilter>>,
) -> PgAppState<UnusedSessionService> {
    let mut state = PgAppState::new(
        data_service,
        Arc::new(UnusedSessionService),
        None::<Arc<RedisEVMMonitor>>,
        Arc::new(NoOpRateProvider),
        Arc::new(server::services::email::NoopEmailSender),
    );
    state.invoice_creation_filters = filters;
    state
}

/// Ticket test 2, at the endpoint the review said was unverified.
#[tokio::test]
#[ignore]
async fn a_denying_filter_blocks_the_real_endpoint_with_the_reason() {
    let pg = pg_service().await;
    let owner = seed_user(pg.pool()).await;
    let store = Store::new(format!("store-{}", Uuid::new_v4()), UserId(owner));
    pg.create_store_owned_by(&store, UserId(owner))
        .await
        .expect("seed store owned by user, with the Owner role's cancreateinvoice permission");

    let state = app_state(Arc::new(pg), vec![Arc::new(AlwaysDeny)]);

    let result = create_invoice(
        caller(owner),
        State(state),
        Json(invoice_request(store.id.0)),
    )
    .await;

    let Err((status, Json(body))) = result else {
        panic!("a denying filter must refuse invoice creation");
    };
    assert_eq!(status, StatusCode::FORBIDDEN);
    let message = body["message"].as_str().expect("error body has a message");
    assert!(
        message.contains("subscription"),
        "the merchant must see why, got: {message}"
    );
}

/// Review finding, fixed: the module comment claimed the filter runs
/// after the permission check without a test pinning the order. A denying
/// filter is installed here too, so if the filter ran first - or the
/// permission check were skipped - this request would come back
/// `invoice_creation_blocked` instead. It must come back `forbidden`: a user
/// with no role on the store never reaches the filter at all.
#[tokio::test]
#[ignore]
async fn permission_denies_before_the_filter_is_ever_consulted() {
    let pg = pg_service().await;
    let owner = seed_user(pg.pool()).await;
    let stranger = seed_user(pg.pool()).await;
    let store = Store::new(format!("store-{}", Uuid::new_v4()), UserId(owner));
    pg.create_store_owned_by(&store, UserId(owner))
        .await
        .expect("seed store owned by user");

    let state = app_state(Arc::new(pg), vec![Arc::new(AlwaysDeny)]);

    let result = create_invoice(
        caller(stranger),
        State(state),
        Json(invoice_request(store.id.0)),
    )
    .await;

    let Err((status, Json(body))) = result else {
        panic!("a user with no role on the store must be refused");
    };
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        body["error"], "forbidden",
        "must fail on the permission check, not the filter: got {body:?}"
    );
}

/// The negative case: with no filters installed, the same owner/store passes
/// the filter stage untouched and fails downstream instead (no payment
/// methods configured) - proving the filter wiring only intervenes when a
/// filter actually denies, not on every request.
#[tokio::test]
#[ignore]
async fn no_filters_reaches_past_the_filter_stage() {
    let pg = pg_service().await;
    let owner = seed_user(pg.pool()).await;
    let store = Store::new(format!("store-{}", Uuid::new_v4()), UserId(owner));
    pg.create_store_owned_by(&store, UserId(owner))
        .await
        .expect("seed store owned by user");

    let state = app_state(Arc::new(pg), Vec::new());

    let result = create_invoice(
        caller(owner),
        State(state),
        Json(invoice_request(store.id.0)),
    )
    .await;

    let Err((status, Json(body))) = result else {
        panic!("expected a downstream failure past the filter stage (no wallet configured)");
    };
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "no_payment_methods");
}

/// The deadlock the operator exemption exists to prevent.
///
/// A plugin refuses invoice creation for a merchant in arrears. The invoice
/// that *renews* a subscription is itself created with the operator's own
/// credential - so without the exemption, a plugin that refuses (a bug, or
/// simply being down while its manifest fails closed) refuses the renewal
/// that would have cleared the refusal, and the only way out is editing the
/// database by hand.
///
/// Uses the same `AlwaysDeny` filter as the tests above and the same store
/// an ordinary credential gets blocked on, which proves the difference is
/// the credential's `is_operator` property and not the filter or the store.
#[tokio::test]
#[ignore]
async fn operator_credential_is_never_filtered() {
    let pg = pg_service().await;
    let owner = seed_user(pg.pool()).await;
    let store = Store::new(format!("store-{}", Uuid::new_v4()), UserId(owner));
    pg.create_store_owned_by(&store, UserId(owner))
        .await
        .expect("seed store owned by user");

    let state = app_state(Arc::new(pg), vec![Arc::new(AlwaysDeny)]);

    let result = create_invoice(
        caller_as(user_info(owner), true),
        State(state),
        Json(invoice_request(store.id.0)),
    )
    .await;

    if let Err((status, Json(body))) = &result {
        assert_ne!(
            body["error"].as_str(),
            Some("invoice_creation_blocked"),
            "the operator credential was filtered; a plugin can now deadlock its own renewals (status {status})"
        );
    }
}

/// The specific widening the exemption must not become: `ServerAdmin` alone
/// is not enough. An admin session without the explicitly-granted operator
/// property is filtered exactly like any other credential.
#[tokio::test]
#[ignore]
async fn admin_without_the_operator_property_is_still_filtered() {
    let pg = pg_service().await;
    let admin = seed_user(pg.pool()).await;
    let store = Store::new(format!("store-{}", Uuid::new_v4()), UserId(admin));
    pg.create_store_owned_by(&store, UserId(admin))
        .await
        .expect("seed store owned by user");

    let state = app_state(Arc::new(pg), vec![Arc::new(AlwaysDeny)]);

    let result = create_invoice(
        caller_as(user_info_with_role(admin, Role::ServerAdmin), false),
        State(state),
        Json(invoice_request(store.id.0)),
    )
    .await;

    let Err((_, Json(body))) = result else {
        panic!("an admin credential without is_operator must still be filtered");
    };
    assert_eq!(body["error"].as_str(), Some("invoice_creation_blocked"));
}

/// The account a filter is told about must be the store's **owner**, not the
/// caller and not a placeholder.
///
/// Billing is per merchant: a wrong account here bills or refuses the wrong
/// merchant, and every other test in this file passes whatever value is in
/// that field. This is the only one that reads it.
#[tokio::test]
#[ignore]
async fn the_filter_is_told_which_account_owns_the_store() {
    use std::sync::Mutex;

    struct Recording(Arc<Mutex<Vec<InvoiceCreationFilterRequest>>>);

    #[async_trait]
    impl InvoiceCreationFilter for Recording {
        async fn filter_invoice_creation(
            &self,
            request: InvoiceCreationFilterRequest,
        ) -> FilterVerdict {
            self.0.lock().unwrap().push(request);
            FilterVerdict::Allow
        }
    }

    let pg = pg_service().await;
    let owner = seed_user(pg.pool()).await;
    let store = Store::new(format!("store-{}", Uuid::new_v4()), UserId(owner));
    pg.create_store_owned_by(&store, UserId(owner))
        .await
        .expect("seed store");

    let seen = Arc::new(Mutex::new(Vec::new()));
    let state = app_state(Arc::new(pg), vec![Arc::new(Recording(seen.clone()))]);

    let _ = create_invoice(
        caller(owner),
        State(state),
        Json(invoice_request(store.id.0)),
    )
    .await;

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "the filter was never consulted");
    assert_eq!(
        seen[0].store_id.0, store.id.0,
        "the filter was told about the wrong store"
    );
    assert_eq!(
        seen[0].account_id,
        UserId(owner),
        "the filter was told about the wrong account; billing would act on the wrong merchant"
    );
}

/// The deadlock case again, but through the wiring the tests above skip:
/// a real `api_keys` row with `is_operator` set, a real bearer header, and
/// the actual `AuthenticatedCaller` extractor - not a hand-built struct
/// literal. If the column were dropped anywhere between the database row and
/// the extractor's output, this is the test that would notice.
#[tokio::test]
#[ignore]
async fn operator_flag_on_a_real_api_key_row_reaches_the_extractor_and_exempts_the_request() {
    let pg = pg_service().await;
    let owner = seed_user(pg.pool()).await;
    let store = Store::new(format!("store-{}", Uuid::new_v4()), UserId(owner));
    pg.create_store_owned_by(&store, UserId(owner))
        .await
        .expect("seed store owned by user");
    let raw_key = seed_api_key(pg.pool(), owner, true).await;

    let state = app_state(Arc::new(pg), vec![Arc::new(AlwaysDeny)]);

    let caller = authenticate_with_api_key(&raw_key, &state)
        .await
        .expect("a real, active, operator-flagged api key must authenticate");
    assert!(
        caller.is_operator,
        "the extractor lost is_operator between the database row and AuthenticatedCaller"
    );

    let result = create_invoice(caller, State(state), Json(invoice_request(store.id.0))).await;

    if let Err((status, Json(body))) = &result {
        assert_ne!(
            body["error"].as_str(),
            Some("invoice_creation_blocked"),
            "an operator key authenticated through the real extractor was filtered anyway (status {status})"
        );
    }
}

/// The companion negative case: an ordinary `api_keys` row (`is_operator`
/// false, the column's default) authenticated through the same real
/// extractor path is filtered exactly like the hand-built caller in
/// `a_denying_filter_blocks_the_real_endpoint_with_the_reason` above.
#[tokio::test]
#[ignore]
async fn non_operator_api_key_row_is_still_filtered_through_the_real_extractor() {
    let pg = pg_service().await;
    let owner = seed_user(pg.pool()).await;
    let store = Store::new(format!("store-{}", Uuid::new_v4()), UserId(owner));
    pg.create_store_owned_by(&store, UserId(owner))
        .await
        .expect("seed store owned by user");
    let raw_key = seed_api_key(pg.pool(), owner, false).await;

    let state = app_state(Arc::new(pg), vec![Arc::new(AlwaysDeny)]);

    let caller = authenticate_with_api_key(&raw_key, &state)
        .await
        .expect("a real, active api key must authenticate");
    assert!(!caller.is_operator);

    let result = create_invoice(caller, State(state), Json(invoice_request(store.id.0))).await;

    let Err((_, Json(body))) = result else {
        panic!("an ordinary api key credential must still be filtered");
    };
    assert_eq!(body["error"].as_str(), Some("invoice_creation_blocked"));
}

/// The control for giving the synthetic payment test a fresh merchant account
/// per run instead of exempting it.
///
/// A filter that judges by account - as a volume limit does - must refuse the
/// account that is over its limit and must not refuse a new account beside it,
/// through the same handler with the same filter installed. Both halves are
/// needed: "the fresh account gets through" alone is also what a limit that
/// has stopped applying looks like. Remove the filter stage from
/// `create_invoice` and the first assertion goes red; make the filter refuse
/// everyone and the second does.
#[tokio::test]
#[ignore]
async fn an_over_limit_account_is_refused_while_a_new_account_beside_it_is_not() {
    struct RefuseAccount(UserId);

    #[async_trait]
    impl InvoiceCreationFilter for RefuseAccount {
        async fn filter_invoice_creation(
            &self,
            request: InvoiceCreationFilterRequest,
        ) -> FilterVerdict {
            if request.account_id == self.0 {
                FilterVerdict::Deny {
                    reason: "You have settled more than your plan allows in the last 30 days."
                        .to_string(),
                }
            } else {
                FilterVerdict::Allow
            }
        }
    }

    let pg = Arc::new(pg_service().await);
    let over_limit = seed_user(pg.pool()).await;
    let fresh = seed_user(pg.pool()).await;
    let over_limit_store = Store::new(format!("store-{}", Uuid::new_v4()), UserId(over_limit));
    let fresh_store = Store::new(format!("store-{}", Uuid::new_v4()), UserId(fresh));
    pg.create_store_owned_by(&over_limit_store, UserId(over_limit))
        .await
        .expect("seed over-limit merchant's store");
    pg.create_store_owned_by(&fresh_store, UserId(fresh))
        .await
        .expect("seed fresh merchant's store");

    let filters: Vec<Arc<dyn InvoiceCreationFilter>> =
        vec![Arc::new(RefuseAccount(UserId(over_limit)))];

    let refused = create_invoice(
        caller(over_limit),
        State(app_state(pg.clone(), filters.clone())),
        Json(invoice_request(over_limit_store.id.0)),
    )
    .await;
    let Err((status, Json(body))) = refused else {
        panic!("an over-limit merchant must still be refused invoice creation");
    };
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "invoice_creation_blocked");

    // Past the filter stage: the store has no payment method, which is the
    // next thing `create_invoice` checks.
    let admitted = create_invoice(
        caller(fresh),
        State(app_state(pg, filters)),
        Json(invoice_request(fresh_store.id.0)),
    )
    .await;
    let Err((status, Json(body))) = admitted else {
        panic!("expected a failure past the filter stage (no wallet configured)");
    };
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "no_payment_methods");
}
