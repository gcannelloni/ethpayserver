//! The store owner's own wallet rotation history (`GET /users/me/stores/{id}/wallet-rotations`).
//!
//! Read-only. Rotations are written on every key change; this is the reader,
//! so an owner can see every key their store has moved through. Xpubs are
//! masked: the history is for recognising a change, not for copying keys.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use auth::repository::StoreRepository;
use auth::{SessionService, UserId};

use crate::api::extractors::AuthenticatedUser;
use crate::api::stores::mask_xpub;
use crate::state::PgAppState;

#[derive(Debug, Serialize, ToSchema)]
pub struct WalletRotationEntry {
    pub id: Uuid,
    pub payment_method_id: Uuid,
    pub previous_xpub_masked: String,
    pub new_xpub_masked: String,
    pub previous_derivation_index: i32,
    pub reason: Option<String>,
    pub rotated_at: DateTime<Utc>,
}

/// Owner only. A store the caller does not own is reported as not found, the
/// same as one that does not exist, so the endpoint does not reveal other
/// tenants' store ids.
pub(crate) fn may_read_rotation_history(caller: UserId, owner: UserId) -> bool {
    caller == owner
}

/// The store owner's wallet rotation history, newest first.
#[utoipa::path(
    get,
    path = "/users/me/stores/{store_id}/wallet-rotations",
    tag = "users",
    security(("bearer_auth" = [])),
    params(("store_id" = Uuid, Path, description = "Store ID")),
    responses(
        (status = 200, description = "Rotation history, newest first", body = [WalletRotationEntry]),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Store not found, or not owned by the caller"),
        (status = 503, description = "Database unavailable; retry"),
    )
)]
pub async fn get_wallet_rotations<A>(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<PgAppState<A>>,
    Path(store_id): Path<Uuid>,
) -> Result<Json<Vec<WalletRotationEntry>>, (StatusCode, &'static str)>
where
    A: SessionService + 'static,
{
    let store = state
        .data_service
        .get_store(auth::StoreId(store_id))
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "store lookup failed for rotation history");
            (StatusCode::SERVICE_UNAVAILABLE, "temporarily unavailable")
        })?;

    match store {
        Some(store) if may_read_rotation_history(user.id, store.owner_id) => {}
        _ => return Err((StatusCode::NOT_FOUND, "store not found")),
    }

    let rotations = state
        .data_service
        .get_rotation_history(store_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "rotation history read failed");
            (StatusCode::SERVICE_UNAVAILABLE, "temporarily unavailable")
        })?;

    Ok(Json(
        rotations
            .into_iter()
            .map(|r| WalletRotationEntry {
                id: r.id,
                payment_method_id: r.payment_method_id,
                previous_xpub_masked: mask_xpub(&r.previous_xpub),
                new_xpub_masked: mask_xpub(&r.new_xpub),
                previous_derivation_index: r.previous_derivation_index,
                reason: r.reason,
                rotated_at: r.rotated_at,
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_owner_may_read_their_own_history() {
        let me = UserId(Uuid::from_u128(1));
        assert!(may_read_rotation_history(me, me));
    }

    #[test]
    fn another_tenant_may_not_read_it() {
        let owner = UserId(Uuid::from_u128(1));
        let other = UserId(Uuid::from_u128(2));
        assert!(!may_read_rotation_history(other, owner));
    }
}
