//! Account wallet endpoints, and the per-store override that points at one.
//!
//! Wallets belong to the account, not to a store. A store derives
//! from its own override if it has been given one, and from the account
//! primary otherwise, and that walk is spelled once in the repository so
//! reads, allocation and rotation all use it.
//!
//! `GET /stores/{id}/wallet` reports that store-level walk by default, which
//! is only ever an approximation of where a given method's next invoice lands:
//! a method may be pinned to a wallet of its own, and address allocation
//! resolves through the *method* first, the store second. Pass
//! `payment_method_id` to ask the question allocation actually answers.

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use auth::repository::{StoreRepository, UserStoreRepository};
use auth::{SessionService, StoreId};
use data_service::{
    self, StorePaymentMethod, StorePaymentMethodReader, WalletReader, WalletWriter,
};
use evm::{XpubDeriver, looks_like_a_private_key, validate_xpub, xpub_depth_problem};
use types::NAMESPACE_EIP155;

use super::super::extractors::{
    AuthenticatedUser, FreshlyAuthenticatedUser, StoreScopedUser, key_grants_store_permission,
};
use super::{ApiErr, mask_xpub, repository_error, require_store_settings_permission};
use crate::state::PgAppState;
pub use api_types::{
    CreateWalletRequest, CreateWalletResponse, DerivedAddressEntry, RotateWalletRequest,
    RotateWalletResponse, RotationEntry, SetStoreWalletRequest, StoreWalletResponse,
    UpdateWalletRequest, WalletAddressesResponse, WalletResponse, WalletXpubResponse,
};

/// How many addresses a newly registered wallet hands back for checking.
///
/// Re-exported from `evm`, which also drives the offline `derive-xpub`
/// tool's printed check addresses - one constant, so the two lists a
/// merchant compares by eye can't silently drift apart.
pub use evm::VERIFICATION_ADDRESS_COUNT;

/// Query parameters for listing wallet addresses.
#[derive(Debug, Deserialize, IntoParams)]
pub struct WalletAddressesQuery {
    /// Number of addresses to derive (default 20, max 100).
    pub limit: Option<u32>,
    /// Starting index (default 0).
    pub offset: Option<u32>,
}

/// Which chain family an endpoint about a store's wallet is asking about.
///
/// A store may hold one override per family, so "the store's wallet" is not a
/// question with a single answer any more. Defaults to `eip155`, which is what
/// every caller written before families meant.
#[derive(Debug, Deserialize, IntoParams)]
pub struct StoreWalletQuery {
    /// CAIP-2 namespace: `eip155`, `tron`, ... . Defaults to `eip155`.
    /// Ignored when `payment_method_id` is given - the method's own chain
    /// says which family that is.
    pub namespace: Option<String>,
    /// Resolve through this payment method instead of the store's bare
    /// fallback.
    ///
    /// A method may be pinned to a wallet of its own - `pm.wallet_id`,
    /// `method_wallet()` in the repository - which the plain store-level
    /// walk (`namespace` alone) never looks at. Address allocation always
    /// resolves through the method, so a caller asking "where will this
    /// method's next invoice actually be collected" needs the method-scoped
    /// answer, not the store's: the two can name different wallets the
    /// moment a method is pinned and the account's primary or override moves
    /// out from under it.
    pub payment_method_id: Option<Uuid>,
}

impl StoreWalletQuery {
    fn namespace(&self) -> &str {
        self.namespace.as_deref().unwrap_or(NAMESPACE_EIP155)
    }
}

/// The method-scoped answer to `GET /stores/{store_id}/wallet`.
///
/// A superset of `StoreWalletResponse`: `is_override` here means exactly what
/// it means in the bare form, the store has an explicit override configured,
/// never whether this method's own pin happens to differ from it. Those are
/// different facts. A method can resolve to a wallet the store's bare walk
/// does not reach with no store override configured at all, if its pin has
/// not caught up with a primary-wallet change. `differs_from_store_wallet` is
/// that fact, named for what it is instead of folded into a field whose other
/// reader already means something else by it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MethodWalletResponse {
    #[serde(flatten)]
    pub store_wallet: StoreWalletResponse,
    /// True when this method's resolved wallet is not the wallet the store's
    /// bare fallback would land on right now - independent of `is_override`,
    /// and true even when the store has no override configured.
    pub differs_from_store_wallet: bool,
}

/// What `GET /stores/{store_id}/wallet` answers - the shape depends on
/// whether `payment_method_id` was given.
///
/// Kept as an enum, rather than always returning the superset
/// `MethodWalletResponse`, so the bare form's wire shape is unchanged for
/// callers who never pass `payment_method_id`.
pub enum StoreWalletResult {
    Bare(StoreWalletResponse),
    Method(MethodWalletResponse),
}

impl IntoResponse for StoreWalletResult {
    fn into_response(self) -> Response {
        match self {
            Self::Bare(r) => Json(r).into_response(),
            Self::Method(r) => Json(r).into_response(),
        }
    }
}

/// Derive the first `count` addresses of a wallet, rendered for its family.
///
/// Used both to answer `GET /wallets/{id}/addresses` and to hand a merchant
/// something to check a freshly registered key against. The derivation path
/// carries the coin type, which is the only part of it that differs between
/// families and the only part worth a merchant's attention.
pub(super) fn derive_entries(
    wallet: &data_service::Wallet,
    offset: u32,
    count: u32,
) -> Result<Vec<DerivedAddressEntry>, StatusCode> {
    // The namespace comes from the wallet row, never from the request: it is
    // what the key was registered as, and deriving under anything else
    // produces addresses the merchant's own wallet does not watch.
    let deriver = XpubDeriver::from_xpub(&wallet.namespace, &wallet.xpub)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut addresses = Vec::with_capacity(count as usize);
    for i in offset..offset.saturating_add(count) {
        addresses.push(DerivedAddressEntry {
            address: deriver
                .derive_address(i)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
            index: i,
            derivation_path: deriver.derivation_path(i),
            used: (i as i64) < wallet.derivation_index as i64,
        });
    }

    Ok(addresses)
}

/// Load a wallet and confirm it belongs to the caller.
///
/// Ownership is the whole authorization story for a wallet - there is no
/// sharing - so a wallet on another account is reported as 404 rather than
/// 403: confirming it exists would let anyone probe for wallet ids.
async fn owned_wallet<A: SessionService>(
    state: &PgAppState<A>,
    user: &auth::UserInfo,
    wallet_id: Uuid,
) -> Result<data_service::Wallet, StatusCode> {
    let wallet = WalletReader::get_wallet(&*state.data_service, wallet_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    if wallet.user_id != user.id.0 {
        return Err(StatusCode::NOT_FOUND);
    }

    Ok(wallet)
}

/// List the account's wallets.
#[utoipa::path(
    get,
    path = "/wallets",
    tag = "stores",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "The account's wallets", body = Vec<WalletResponse>),
        (status = 401, description = "Unauthorized"),
    )
)]
pub async fn list_wallets<A>(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
) -> Result<Json<Vec<WalletResponse>>, StatusCode>
where
    A: SessionService + 'static,
{
    let wallets = WalletReader::list_wallets(&*state.data_service, user.id.0)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(wallets.into_iter().map(Into::into).collect()))
}

/// Add a wallet to the account, for one chain family.
///
/// Adding an xpub the account already holds *in the same family* returns the
/// existing wallet rather than a duplicate: two rows on one key would be two
/// derivation counters on it, and that is exactly the collision the
/// account-level model removes. The same bytes registered for a different
/// family is a different wallet - see `WalletWriter::create_wallet`.
///
/// The response carries the first few addresses the key derives, and that is
/// the point of this endpoint rather than a nicety. An account-level xpub has
/// its BIP-44 coin type baked in and its parent unreachable, so nothing here
/// can check that the key the merchant pasted is a key for the family they
/// picked: `m/44'/60'/0'` and `m/44'/195'/0'` are the same bytes in the same
/// alphabet with the same version prefix. A mismatch is accepted, derives
/// perfectly valid addresses, and is discovered only when a customer has paid
/// and the merchant's wallet shows nothing. Comparing three addresses against
/// their own wallet, before any invoice quotes one, is the only check that
/// exists.
#[utoipa::path(
    post,
    path = "/wallets",
    tag = "stores",
    security(("bearer_auth" = [])),
    request_body = CreateWalletRequest,
    responses(
        (status = 201, description = "Wallet added, with addresses to verify it",
         body = CreateWalletResponse),
        (status = 400, description = "Invalid xpub, or a chain family this server \
                                      cannot derive for"),
        (status = 401, description = "Unauthorized"),
        (status = 409, description = "Another account already holds this xpub"),
    )
)]
pub async fn create_wallet<A>(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
    Json(req): Json<CreateWalletRequest>,
) -> Result<(StatusCode, Json<CreateWalletResponse>), ApiErr>
where
    A: SessionService + 'static,
{
    if !validate_xpub(&req.xpub) {
        return Err(StatusCode::BAD_REQUEST.into());
    }
    if let Some(reason) = xpub_depth_problem(&req.xpub) {
        return Err((StatusCode::BAD_REQUEST, reason).into());
    }

    // Refuse a family this build cannot derive for, before the key is stored.
    // Accepting `solana` here would write a row nothing can ever derive from,
    // and the merchant would see a wallet listed on their account that no
    // payment method can use and no error explains.
    if evm::family_for_namespace(&req.namespace).is_err() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "unsupported_namespace: this server cannot derive addresses for \
                 `{}` keys",
                req.namespace
            ),
        )
            .into());
    }

    let wallet = WalletWriter::create_wallet(
        &*state.data_service,
        user.id.0,
        &req.namespace,
        &req.xpub,
        req.name.as_deref(),
    )
    .await
    .map_err(repository_error)?;

    // Derived from index 0 rather than from the wallet's current position:
    // these are for checking the key, not for use, and index 0 is the one the
    // merchant's own wallet shows first. Re-registering an existing wallet
    // therefore returns the same three addresses it did the first time.
    let verification_addresses = derive_entries(&wallet, 0, VERIFICATION_ADDRESS_COUNT)?;

    Ok((
        StatusCode::CREATED,
        Json(CreateWalletResponse {
            wallet: wallet.into(),
            verification_addresses,
        }),
    ))
}

/// Get one of the account's wallets.
#[utoipa::path(
    get,
    path = "/wallets/{wallet_id}",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(("wallet_id" = Uuid, Path, description = "Wallet ID")),
    responses(
        (status = 200, description = "Wallet details", body = WalletResponse),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "No such wallet on this account"),
    )
)]
pub async fn get_wallet_by_id<A>(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
    Path(wallet_id): Path<Uuid>,
) -> Result<Json<WalletResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    Ok(Json(owned_wallet(&state, &user, wallet_id).await?.into()))
}

/// Rename a wallet, or make it the account primary.
#[utoipa::path(
    patch,
    path = "/wallets/{wallet_id}",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(("wallet_id" = Uuid, Path, description = "Wallet ID")),
    request_body = UpdateWalletRequest,
    responses(
        (status = 200, description = "Updated wallet", body = WalletResponse),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "No such wallet on this account"),
    )
)]
pub async fn update_wallet<A>(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
    Path(wallet_id): Path<Uuid>,
    Json(req): Json<UpdateWalletRequest>,
) -> Result<Json<WalletResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    let mut wallet = owned_wallet(&state, &user, wallet_id).await?;

    if req.name.is_some() {
        wallet = WalletWriter::rename_wallet(&*state.data_service, wallet_id, req.name.as_deref())
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }

    if req.is_primary == Some(true) {
        wallet = WalletWriter::set_primary_wallet(&*state.data_service, user.id.0, wallet_id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }

    Ok(Json(wallet.into()))
}

/// Remove a wallet from the account.
///
/// Refused while a store or a payment method still points at it - those are
/// live routing, and dropping the key underneath them would leave the next
/// invoice with nothing to derive from.
///
/// Not refused by history. `payment_options.wallet_id` is ON DELETE SET NULL,
/// so options this wallet issued addresses for keep their `payment_address`
/// and lose only the provenance stamp; their `derivation_index` is left in
/// place, since it is still true of an xpub the merchant holds a copy of.
/// Pinning a wallet forever because it once issued an address was the
/// alternative, and it makes every wallet permanently undeletable.
///
/// A monitored address outlives the wallet, then. That is survivable - the
/// address is recorded literally and attribution is keyed on it, not on the
/// wallet - but it does mean deleting a wallet with invoices still open costs
/// the answer to "which key produced this address".
#[utoipa::path(
    delete,
    path = "/wallets/{wallet_id}",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(("wallet_id" = Uuid, Path, description = "Wallet ID")),
    responses(
        (status = 204, description = "Wallet deleted"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "No such wallet on this account"),
        (status = 409, description = "Still in use by a store or payment method"),
    )
)]
pub async fn delete_wallet<A>(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
    Path(wallet_id): Path<Uuid>,
) -> Result<StatusCode, ApiErr>
where
    A: SessionService + 'static,
{
    owned_wallet(&state, &user, wallet_id).await?;

    WalletWriter::delete_wallet(&*state.data_service, wallet_id)
        .await
        .map_err(repository_error)?;

    Ok(StatusCode::NO_CONTENT)
}

/// Export the full (unmasked) xpub for a wallet.
#[utoipa::path(
    get,
    path = "/wallets/{wallet_id}/xpub",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(("wallet_id" = Uuid, Path, description = "Wallet ID")),
    responses(
        (status = 200, description = "Full xpub export", body = WalletXpubResponse),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "No such wallet on this account"),
    )
)]
pub async fn export_wallet_xpub<A>(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
    Path(wallet_id): Path<Uuid>,
) -> Result<Json<WalletXpubResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    let wallet = owned_wallet(&state, &user, wallet_id).await?;

    Ok(Json(WalletXpubResponse {
        id: wallet.id,
        user_id: wallet.user_id,
        namespace: wallet.namespace,
        xpub: wallet.xpub,
        derivation_index: wallet.derivation_index,
        name: wallet.name,
        created_at: wallet.created_at,
    }))
}

/// List derived addresses for a wallet.
///
/// Addresses below the wallet's current index have been handed out. Rendered
/// in the wallet's own family encoding, with the coin type in each derivation
/// path - the path used to be hardcoded as `m/44'/60'/0'/0/{i}`, which is a
/// claim about the key rather than a fact read from it.
#[utoipa::path(
    get,
    path = "/wallets/{wallet_id}/addresses",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(
        ("wallet_id" = Uuid, Path, description = "Wallet ID"),
        WalletAddressesQuery,
    ),
    responses(
        (status = 200, description = "Derived addresses", body = WalletAddressesResponse),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "No such wallet on this account"),
        (status = 500, description = "Address derivation failed"),
    )
)]
pub async fn list_wallet_addresses<A>(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
    Path(wallet_id): Path<Uuid>,
    Query(query): Query<WalletAddressesQuery>,
) -> Result<Json<WalletAddressesResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    let wallet = owned_wallet(&state, &user, wallet_id).await?;

    let limit = query.limit.unwrap_or(20).min(100);
    let offset = query.offset.unwrap_or(0);

    Ok(Json(WalletAddressesResponse {
        wallet_id: wallet.id,
        derivation_index: wallet.derivation_index,
        addresses: derive_entries(&wallet, offset, limit)?,
    }))
}

/// Get the wallet a store derives from, for one chain family - or, given
/// `payment_method_id`, the wallet that specific method derives from.
///
/// Per family, because that is what resolution is. A store with an Ethereum
/// key and no Tron one has an answer for `eip155` and none for `tron`, and
/// collapsing the two would report a key that Tron payments will never be
/// collected on.
///
/// The bare (no `payment_method_id`) form answers for the store's own
/// override and the account primary - it does not see a method's own pin.
/// That is a real gap for a caller who needs to know where a *specific*
/// method's next invoice lands: `derive_payment_address` resolves through
/// the method (its pin first, the store second), so a pinned method can
/// derive from a wallet this endpoint never mentions the moment the account's
/// primary or override moves elsewhere. Passing `payment_method_id` asks the
/// same question `allocate_derivation` answers instead.
///
/// `is_override` means the same thing in both forms - the store has an
/// explicit override configured - never whether the method-scoped answer
/// differs from the bare one. A pinned method can resolve away from the
/// store's bare walk with no override in play at all, so that second fact
/// travels as its own field, `differs_from_store_wallet`, present only on the
/// method-scoped response. See `MethodWalletResponse`.
#[utoipa::path(
    get,
    path = "/stores/{store_id}/wallet",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(("store_id" = Uuid, Path, description = "Store ID"), StoreWalletQuery),
    responses(
        (status = 200, description = "Resolved wallet. With `payment_method_id`, the \
                                      body additionally carries \
                                      `differs_from_store_wallet` (see \
                                      `MethodWalletResponse`); `is_override` always \
                                      means the store has an explicit override \
                                      configured, in both forms.",
         body = StoreWalletResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "No wallet for this family: the store has no \
                                      override for it and the account no primary. Or, \
                                      with `payment_method_id`, no such method on this \
                                      store, or one with nothing to derive from."),
    )
)]
pub async fn get_store_wallet<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
    Query(query): Query<StoreWalletQuery>,
) -> Result<StoreWalletResult, StatusCode>
where
    A: SessionService + 'static,
{
    const VIEW_SETTINGS: &str = "ethpay.store.canviewstoresettings";
    let has_permission = state
        .data_service
        .user_has_store_permission(user.id, StoreId(store_id), VIEW_SETTINGS)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        && key_grants_store_permission(key_scope.as_deref(), VIEW_SETTINGS, StoreId(store_id));

    if !has_permission {
        return Err(StatusCode::FORBIDDEN);
    }

    if let Some(method_id) = query.payment_method_id {
        return get_payment_method_wallet(&state, store_id, method_id).await;
    }

    let namespace = query.namespace();

    let wallet = WalletReader::resolve_store_wallet(&*state.data_service, store_id, namespace)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let is_override =
        WalletReader::get_store_wallet_override(&*state.data_service, store_id, namespace)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .is_some();

    Ok(StoreWalletResult::Bare(StoreWalletResponse {
        store_id,
        wallet: wallet.into(),
        is_override,
    }))
}

/// The wallet one payment method actually derives from - its own pin if it
/// has one, else whatever the store resolves to.
///
/// Reads `StorePaymentMethod::wallet_id`, which is already the resolved
/// answer (see `method_wallet()` in the repository): the same walk
/// `allocate_derivation` uses to pick the wallet whose counter it advances.
/// Reporting anything else here would be a second, independent spelling of
/// that walk - exactly the drift this endpoint exists to rule out.
async fn get_payment_method_wallet<A>(
    state: &PgAppState<A>,
    store_id: Uuid,
    method_id: Uuid,
) -> Result<StoreWalletResult, StatusCode>
where
    A: SessionService + 'static,
{
    let method = StorePaymentMethodReader::get_payment_method(&*state.data_service, method_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .filter(|m| m.store_id == store_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let wallet_id = method.wallet_id.ok_or(StatusCode::NOT_FOUND)?;
    let wallet = WalletReader::get_wallet(&*state.data_service, wallet_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let namespace = method.chain_id.namespace();
    let store_wallet =
        WalletReader::resolve_store_wallet(&*state.data_service, store_id, namespace)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Same meaning as the bare form: the store has an explicit override
    // configured. Not whether this method's pin happens to differ from it -
    // see `differs_from_store_wallet` below for that.
    let is_override =
        WalletReader::get_store_wallet_override(&*state.data_service, store_id, namespace)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .is_some();

    // Whether this method's own resolution differs from the store's bare
    // fallback - true even with no override configured, when a pin has not
    // caught up with a primary-wallet change.
    let differs_from_store_wallet =
        store_wallet.is_none_or(|store_wallet| store_wallet.id != wallet_id);

    Ok(StoreWalletResult::Method(MethodWalletResponse {
        store_wallet: StoreWalletResponse {
            store_id,
            wallet: wallet.into(),
            is_override,
        },
        differs_from_store_wallet,
    }))
}

/// Pin a store to one of the account's wallets.
///
/// This moves where the store's money goes. Its payment methods stop deriving
/// from whatever key each was configured with and follow this wallet, so the
/// next invoice is paid to an address derived from it. Addresses already issued
/// are untouched and stay watched - in-flight invoices still resolve.
///
/// Nothing is reset: the destination wallet keeps its own counter, so no
/// address it has already produced is issued again.
#[utoipa::path(
    put,
    path = "/stores/{store_id}/wallet",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(("store_id" = Uuid, Path, description = "Store ID")),
    request_body = SetStoreWalletRequest,
    responses(
        (status = 200, description = "Override set", body = StoreWalletResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Store or wallet not found"),
    )
)]
pub async fn configure_store_wallet<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
    Json(req): Json<SetStoreWalletRequest>,
) -> Result<Json<StoreWalletResponse>, StatusCode>
where
    A: SessionService + 'static,
{
    require_store_settings_permission(&state, &user, key_scope.as_deref(), store_id).await?;

    let _ = state
        .data_service
        .get_store(StoreId(store_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    // The repository refuses a wallet belonging to another account, so no
    // ownership check is duplicated here.
    WalletWriter::set_store_wallet(&*state.data_service, store_id, req.wallet_id)
        .await
        .map_err(|e| match e {
            data_service::RepositoryError::NotFound(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        })?;

    let wallet = WalletReader::get_wallet(&*state.data_service, req.wallet_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(StoreWalletResponse {
        store_id,
        wallet: wallet.into(),
        is_override: true,
    }))
}

/// Drop a store's override for one chain family, so it follows that family's
/// account primary again.
///
/// This no longer deletes a wallet - it only stops pinning one. The xpub, its
/// counter and the addresses derived from it are untouched, which is the point:
/// under the old per-store wallet, "delete" destroyed the counter and the next
/// configuration started again at index 0.
///
/// Like `PUT`, this moves where money goes: the store's payment methods are
/// released, so the next invoice derives from the account primary.
#[utoipa::path(
    delete,
    path = "/stores/{store_id}/wallet",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(("store_id" = Uuid, Path, description = "Store ID"), StoreWalletQuery),
    responses(
        (status = 204, description = "Override cleared for this family"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
    )
)]
pub async fn delete_store_wallet<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
    Query(query): Query<StoreWalletQuery>,
) -> Result<StatusCode, StatusCode>
where
    A: SessionService + 'static,
{
    require_store_settings_permission(&state, &user, key_scope.as_deref(), store_id).await?;

    // Scoped to one family, mirroring `PUT`. Clearing every override would
    // move where a store's Ethereum payments go in response to a request about
    // Tron.
    WalletWriter::clear_store_wallet(&*state.data_service, store_id, query.namespace())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(StatusCode::NO_CONTENT)
}

// =============================================================================
// Wallet XPub Rotation
// =============================================================================

/// Rotate the xpub a store's payment methods derive from, on one chain family.
///
/// Points every payment method for this store *on that family* at the account
/// wallet holding the new xpub, creating it if the account does not already
/// have it. Methods on other families are untouched: a key belongs to one
/// family, and repointing a Tron method at a new Ethereum key would derive its
/// addresses at coin type 60 on a chain whose wallets look under 195. Old
/// addresses remain watched until their parent invoices resolve, and a
/// rotation record is kept per payment method.
///
/// Note what no longer happens: derivation indices are not reset to zero. The
/// destination wallet carries its own position, so an xpub the account has
/// used before resumes where it left off instead of re-issuing addresses that
/// may already hold funds.
///
/// Scope is the store, not the account. A method pinned to a wallet is
/// repointed; a method that was inheriting stays inheriting, and what moves
/// instead is the store's override - written here even if the store had none
/// and was following the account primary. Rotating one store must not move
/// every other store on the account, so the rotated store acquires an override
/// of its own. To rotate the account, change the primary.
#[utoipa::path(
    post,
    path = "/stores/{store_id}/wallet/rotate",
    tag = "stores",
    security(("bearer_auth" = [])),
    params(("store_id" = Uuid, Path, description = "Store ID")),
    request_body = RotateWalletRequest,
    responses(
        (status = 200, description = "Wallet rotated", body = RotateWalletResponse),
        (status = 400, description = "Invalid xpub, or every method already uses it"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "No payment methods found for store"),
        (status = 409, description = "That xpub is registered to another account"),
    )
)]
pub async fn rotate_store_wallet<A>(
    StoreScopedUser(user, key_scope): StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
    Json(req): Json<RotateWalletRequest>,
) -> Result<Json<RotateWalletResponse>, ApiErr>
where
    A: SessionService + 'static,
{
    require_store_settings_permission(&state, &user, key_scope.as_deref(), store_id).await?;

    // Validate the new xpub. The bare 400 this used to return said nothing,
    // so a dashboard could only ever show a generic failure for a typo and
    // for a pasted spending key alike - the one boundary this ticket calls
    // out as mattering most. Naming which one fired costs nothing extra: the
    // rejection itself is still `validate_xpub`'s version-byte check, not
    // this string match.
    if !validate_xpub(&req.xpub) {
        let reason = if looks_like_a_private_key(&req.xpub) {
            "invalid_xpub: this looks like a private key, not a public one - rotation needs an \
             extended PUBLIC key, and the private key must never leave the wallet software \
             that holds it"
        } else {
            "invalid_xpub: not a valid base58-encoded extended public key"
        };
        return Err((StatusCode::BAD_REQUEST, reason.to_string()).into());
    }
    if let Some(reason) = xpub_depth_problem(&req.xpub) {
        return Err((StatusCode::BAD_REQUEST, reason).into());
    }

    // And the family it is being rotated into. A namespace this build cannot
    // derive for would store a key nothing can use, in response to a request
    // made because the previous key was compromised.
    if evm::family_for_namespace(&req.namespace).is_err() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "unsupported_namespace: this server cannot derive addresses for \
                 `{}` keys",
                req.namespace
            ),
        )
            .into());
    }

    // Verify store exists
    let _ = state
        .data_service
        .get_store(auth::StoreId(store_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Get all payment methods for this store
    let methods = StorePaymentMethodReader::get_payment_methods(&*state.data_service, store_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if methods.is_empty() {
        return Err(StatusCode::NOT_FOUND.into());
    }

    // One call, one transaction. Rotating method by method meant a failure
    // partway through left the store half moved - some methods on the key the
    // merchant just declared compromised, some on the new one - and the 500
    // said nothing about where it stopped.
    let rotations = state
        .data_service
        .rotate_store_xpub(store_id, &req.namespace, &req.xpub, req.reason.as_deref())
        .await
        .map_err(repository_error)?;

    // Nothing moved means every method on this family was already deriving
    // from this key - or the store has no methods on it at all.
    if rotations.is_empty() {
        return Err(StatusCode::BAD_REQUEST.into());
    }

    // Chain and asset are cosmetic labels on the response, and they come from
    // the snapshot above rather than from the rotation, which records only what
    // it changed. A method created between the snapshot and the rotation would
    // be rotated without a label; it is still counted and still identified by
    // id, which is what a caller acts on.
    let by_id: HashMap<Uuid, &StorePaymentMethod> = methods.iter().map(|m| (m.id, m)).collect();

    let rotations: Vec<RotationEntry> = rotations
        .into_iter()
        .map(|rotation| RotationEntry {
            payment_method_id: rotation.payment_method_id,
            chain_id: by_id
                .get(&rotation.payment_method_id)
                .map(|m| m.chain_id.clone()),
            asset_symbol: by_id
                .get(&rotation.payment_method_id)
                .map(|m| m.asset_symbol.clone()),
            id: rotation.id,
            previous_xpub_masked: mask_xpub(&rotation.previous_xpub),
            previous_derivation_index: rotation.previous_derivation_index,
            rotated_at: rotation.rotated_at,
        })
        .collect();

    Ok(Json(RotateWalletResponse {
        store_id,
        new_xpub_masked: mask_xpub(&req.xpub),
        methods_rotated: rotations.len(),
        rotations,
    }))
}

/// Window over which rotation events are counted for the rate alert.
const ROTATION_RATE_WINDOW_HOURS: i64 = 24;

/// More rotation events than this in the window is an alert. A merchant
/// rotating a key for a real compromise does it once; several in a day is
/// either an incident or someone else holding the session.
const ROTATION_RATE_THRESHOLD: i64 = 3;

/// Reasons a completed rotation should alert. Empty when it is routine.
/// `fresh` is whether the caller held a freshly-authenticated session.
pub(crate) fn rotation_alerts(fresh: bool, events_in_window: i64) -> Vec<&'static str> {
    let mut reasons = Vec::new();
    if events_in_window > ROTATION_RATE_THRESHOLD {
        reasons.push("rate");
    }
    if !fresh {
        reasons.push("not_fresh");
    }
    reasons
}

/// `rotate_store_wallet`, plus the audit. The rotation is not refused for a
/// stale session or a burst of rotations: the alert is the response, since a
/// refusal would stop a merchant escaping a compromised key. Alerts go out as
/// a counter (the alert rule lives in the monitoring service) and a warning
/// that names the store and the reason, never the xpub.
pub async fn rotate_store_wallet_audited<A>(
    fresh: Result<FreshlyAuthenticatedUser, (StatusCode, &'static str)>,
    scoped: StoreScopedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
    Json(req): Json<RotateWalletRequest>,
) -> Result<Json<RotateWalletResponse>, ApiErr>
where
    A: SessionService + 'static,
{
    let fresh = fresh.is_ok();
    let response =
        rotate_store_wallet(scoped, State(state.clone()), Path(store_id), Json(req)).await?;

    // The rotation has already happened. A failed count must not turn that
    // into an error, so it is logged and the rate check is skipped.
    let since = Utc::now() - Duration::hours(ROTATION_RATE_WINDOW_HOURS);
    let events = state
        .data_service
        .count_rotation_events_since(store_id, since)
        .await
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, %store_id, "rotation rate count failed");
            0
        });

    for reason in rotation_alerts(fresh, events) {
        metrics::counter!("ethpayserver_wallet_rotation_alerts_total", "reason" => reason)
            .increment(1);
        tracing::warn!(%store_id, reason, events, "wallet rotation alert");
    }

    Ok(response)
}

#[cfg(test)]
mod audit_tests {
    use super::rotation_alerts;

    #[test]
    fn routine_rotation_from_a_fresh_session_raises_nothing() {
        assert!(rotation_alerts(true, 1).is_empty());
        assert!(
            rotation_alerts(true, 3).is_empty(),
            "the threshold itself is not over it"
        );
    }

    #[test]
    fn a_rotation_without_a_fresh_session_alerts() {
        assert_eq!(rotation_alerts(false, 1), vec!["not_fresh"]);
    }

    #[test]
    fn a_burst_of_rotations_alerts_on_rate() {
        assert_eq!(rotation_alerts(true, 4), vec!["rate"]);
    }

    #[test]
    fn both_conditions_report_both_reasons() {
        assert_eq!(rotation_alerts(false, 4), vec!["rate", "not_fresh"]);
    }
}
