#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Wallet rotation history and the rotation audit, driven through the
//! production router with real credentials against a real Postgres.
//!
//! `GET /users/me/stores/{id}/wallet-rotations` is read by its owner, refused
//! to anyone else, and never returns a full xpub. `POST /stores/{id}/wallet/rotate`
//! raises the not-fresh alert when the caller is not a freshly signed-in
//! session (an API key never is), and stays silent for a fresh session.
//!
//! Needs `DATABASE_URL` and fails, naming it, when it is unset; runs in CI's
//! `--run-ignored` step.

use data_service::test_support::pg_service;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{Duration, Utc};
use data_service::PgDataService;
use data_service::store_creation::StoreCreationWriter;
use evm::{ChainFamily, HdWallet, generate_mnemonic};
use metrics_exporter_prometheus::PrometheusBuilder;
use rates::NoOpRateProvider;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use auth::{AuthConfig, AuthService, Store, UserId};
use server::services::RedisEVMMonitor;
use server::state::PgAppState;
use types::{ChainId, StorePaymentMethodWriter};

/// A valid, never-before-registered xpub. A fixed literal is refused as already
/// claimed once any other test in the shared database has used it.
fn fresh_xpub() -> String {
    let mnemonic = generate_mnemonic(24).expect("generate mnemonic");
    HdWallet::from_mnemonic(&mnemonic, "")
        .expect("derive wallet from mnemonic")
        .account_xpub_string_for(ChainFamily::Evm)
        .expect("account xpub")
}

fn app(pg: &Arc<PgDataService>) -> Router {
    let auth_service = Arc::new(AuthService::with_config(
        Arc::clone(pg),
        AuthConfig::default(),
    ));
    let state = PgAppState::new(
        Arc::clone(pg),
        auth_service,
        None::<Arc<RedisEVMMonitor>>,
        Arc::new(NoOpRateProvider),
        Arc::new(server::services::email::NoopEmailSender),
    );
    server::api::router(state, false, None, None, None)
}

/// A merchant user with an unscoped API key. Returns the user id and raw key.
async fn seed_user(pool: &PgPool) -> (Uuid, String) {
    let user_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, kdf_params, encrypted_symmetric_key, \
         recovery_verification_hash, kdf_salt_identifier) \
         VALUES ($1, \
             '{\"algorithm\":\"argon2id\",\"memory_kb\":65536,\"iterations\":3,\"parallelism\":4,\"salt\":\"AAAA\"}'::jsonb, \
             '{\"ciphertext\":\"AAAA\",\"iv\":\"AAAA\",\"mac\":\"AAAA\"}'::jsonb, \
             'h', 'passkey:' || $1::text)",
    )
    .bind(user_id)
    .execute(pool)
    .await
    .expect("seed user");

    let key = format!("ak_rotation_history_{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, name, key_hash, key_prefix, permissions) \
         VALUES ($1, $2, 'rotation history key', $3, 'ak_test', NULL)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(hex::encode(Sha256::digest(key.as_bytes())))
    .execute(pool)
    .await
    .expect("seed api key");
    (user_id, key)
}

/// A device and a session for `user_id` created at `created_at`. The session
/// is what `FreshlyAuthenticatedUser` reads the age of.
async fn seed_session(pool: &PgPool, user_id: Uuid, created_at: chrono::DateTime<Utc>) -> Uuid {
    let device_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO devices (id, user_id, name, device_type, encrypted_symmetric_key, kdf_params, is_active) \
         VALUES ($1, $2, 'seed device', 'browser', \
             '{\"ciphertext\":\"AAAA\",\"iv\":\"AAAA\",\"mac\":\"AAAA\"}'::jsonb, \
             '{\"algorithm\":\"argon2id\",\"memory_kb\":65536,\"iterations\":3,\"parallelism\":4,\"salt\":\"AAAA\"}'::jsonb, \
             TRUE)",
    )
    .bind(device_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("seed device");

    let session_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO sessions (id, user_id, device_id, created_at, expires_at, last_activity_at) \
         VALUES ($1, $2, $3, $4, NOW() + INTERVAL '1 day', NOW())",
    )
    .bind(session_id)
    .bind(user_id)
    .bind(device_id)
    .bind(created_at)
    .execute(pool)
    .await
    .expect("seed session");
    session_id
}

/// A store owned by `owner` with one EVM method on a fresh key, rotated once
/// onto another fresh key so the history holds one event. Returns the store and
/// both xpubs, so a test can check they never appear in a response.
async fn store_with_one_rotation(pg: &PgDataService, owner: Uuid) -> (Uuid, String, String) {
    let (old_xpub, new_xpub) = (fresh_xpub(), fresh_xpub());
    let store = Store::new(format!("store-{}", Uuid::new_v4()), UserId(owner));
    pg.create_store_owned_by(&store, UserId(owner))
        .await
        .expect("seed store owned by user");
    StorePaymentMethodWriter::create_payment_method(
        pg,
        store.id.0,
        &ChainId::evm(1),
        None,
        "ETH",
        18,
        Some(&old_xpub),
    )
    .await
    .expect("seed payment method on the old key");
    pg.rotate_store_xpub(store.id.0, "eip155", &new_xpub, Some("test rotation"))
        .await
        .expect("rotate once");
    (store.id.0, old_xpub, new_xpub)
}

/// One request through the router. Returns the status and the parsed body,
/// with `Value::Null` for a body that is not JSON.
async fn send(
    app: &Router,
    method: &str,
    uri: String,
    bearer: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(token) = bearer {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn get_history(app: &Router, store: Uuid, bearer: Option<&str>) -> (StatusCode, Value) {
    send(
        app,
        "GET",
        format!("/users/me/stores/{store}/wallet-rotations"),
        bearer,
        Value::Null,
    )
    .await
}

async fn rotate(app: &Router, store: Uuid, bearer: &str) -> (StatusCode, Value) {
    send(
        app,
        "POST",
        format!("/stores/{store}/wallet/rotate"),
        Some(bearer),
        json!({ "xpub": fresh_xpub(), "reason": "alert wiring", "namespace": "eip155" }),
    )
    .await
}

#[tokio::test]
#[ignore]
async fn an_unauthenticated_history_read_is_refused() {
    let pg = Arc::new(pg_service().await);
    let (owner, _) = seed_user(pg.pool()).await;
    let (store, _, _) = store_with_one_rotation(&pg, owner).await;
    let (status, _) = get_history(&app(&pg), store, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[ignore]
async fn the_owner_reads_their_history_with_xpubs_masked() {
    let pg = Arc::new(pg_service().await);
    let (owner, key) = seed_user(pg.pool()).await;
    let (store, old_xpub, new_xpub) = store_with_one_rotation(&pg, owner).await;

    let (status, body) = get_history(&app(&pg), store, Some(&key)).await;
    assert_eq!(status, StatusCode::OK);
    let entries = body.as_array().expect("history is a JSON array");
    assert_eq!(entries.len(), 1, "one rotation, one method, one row");
    assert_eq!(entries[0]["previous_derivation_index"], 0);

    let raw = body.to_string();
    assert!(
        !raw.contains(&old_xpub),
        "the old xpub must not leave masked"
    );
    assert!(
        !raw.contains(&new_xpub),
        "the new xpub must not leave masked"
    );
}

#[tokio::test]
#[ignore]
async fn another_merchant_cannot_read_a_stores_history() {
    let pg = Arc::new(pg_service().await);
    let (owner, owner_key) = seed_user(pg.pool()).await;
    let (_, other_key) = seed_user(pg.pool()).await;
    let (store, _, _) = store_with_one_rotation(&pg, owner).await;
    let app = app(&pg);

    let (status, body) = get_history(&app, store, Some(&other_key)).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a store not owned by the caller reads as not found"
    );
    assert!(
        !body.to_string().contains("previous_xpub_masked"),
        "no history entries leak to a non-owner"
    );

    // Positive control: the same store answers its owner, so the refusal above
    // is the owner check and not a route that refuses everyone.
    let (status, _) = get_history(&app, store, Some(&owner_key)).await;
    assert_eq!(status, StatusCode::OK);
}

/// Sends one rotation through the router with `bearer` and returns the
/// rendered metrics from a local recorder. The router runs on this test's
/// thread (current-thread runtime), so the thread-local recorder sees the
/// alert counter the handler emits.
async fn rotation_metrics(pg: &Arc<PgDataService>, store: Uuid, bearer: &str) -> String {
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let (status, body) = rotate(&app(pg), store, bearer).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the rotation itself succeeds: {body}"
    );
    assert_eq!(
        body["methods_rotated"], 1,
        "the rotation must have happened"
    );
    handle.render()
}

#[tokio::test]
#[ignore]
async fn a_rotation_by_an_api_key_raises_the_not_fresh_alert() {
    let pg = Arc::new(pg_service().await);
    let (owner, key) = seed_user(pg.pool()).await;
    let (store, _, _) = store_with_one_rotation(&pg, owner).await;

    // An API key never satisfies `FreshlyAuthenticatedUser`.
    let rendered = rotation_metrics(&pg, store, &key).await;
    assert!(
        rendered
            .lines()
            .any(|l| l
                .starts_with("ethpayserver_wallet_rotation_alerts_total{reason=\"not_fresh\"} 1")),
        "a rotation without a fresh session must alert; got:\n{rendered}"
    );
    assert!(
        !rendered.contains("reason=\"rate\""),
        "two events in the window is under the rate threshold"
    );
}

#[tokio::test]
#[ignore]
async fn a_rotation_from_a_stale_session_raises_the_not_fresh_alert() {
    let pg = Arc::new(pg_service().await);
    let (owner, _) = seed_user(pg.pool()).await;
    let (store, _, _) = store_with_one_rotation(&pg, owner).await;
    // Valid for a day, but signed in long enough ago that it is not fresh.
    let session = seed_session(pg.pool(), owner, Utc::now() - Duration::minutes(30)).await;

    let rendered = rotation_metrics(&pg, store, &session.to_string()).await;
    assert!(
        rendered
            .lines()
            .any(|l| l
                .starts_with("ethpayserver_wallet_rotation_alerts_total{reason=\"not_fresh\"} 1")),
        "a stale session must alert; got:\n{rendered}"
    );
}

#[tokio::test]
#[ignore]
async fn a_rotation_from_a_fresh_session_raises_no_not_fresh_alert() {
    let pg = Arc::new(pg_service().await);
    let (owner, _) = seed_user(pg.pool()).await;
    let (store, _, _) = store_with_one_rotation(&pg, owner).await;
    let session = seed_session(pg.pool(), owner, Utc::now()).await;

    // Positive control for the two alerts above: the same wiring with a fresh
    // session must stay silent, or the alert fires for everyone.
    let rendered = rotation_metrics(&pg, store, &session.to_string()).await;
    assert!(
        !rendered.contains("reason=\"not_fresh\""),
        "a fresh session must not alert; got:\n{rendered}"
    );
}
