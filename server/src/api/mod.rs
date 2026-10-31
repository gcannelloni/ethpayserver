//! Unified API for ethpayserver.
//!
//! This module combines all API endpoints from different crates into a single router.

use std::sync::Arc;

use axum::{
    Router,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use auth::AuthenticationService;

use crate::state::PgAppState;

pub mod account_notices;
pub mod admin;
pub mod api_key_deprecation;
pub mod api_key_hash;
pub mod api_key_permissions;
pub mod api_key_rate_limit;
pub mod api_key_scope;
pub mod auth_freshness;
pub mod checkout;
pub mod dashboard;
pub mod entitlements;
pub mod extractors;
pub mod health;
pub mod http_metrics;
pub mod idempotency;
pub mod invoices;
mod openapi;
pub mod payouts;
pub mod plugins;
pub mod rate_limit;
pub mod rates;
pub mod refunds;
pub mod standing_warning;
pub mod stores;
pub mod users;
pub mod webhook_deliveries;
pub mod ws;

pub use extractors::{
    AdminAuth, AuthenticatedCaller, AuthenticatedUser, FreshlyAuthenticatedUser, MerchantReader,
    StandingPusher, StoreScopedUser,
};
pub use openapi::ApiDoc;

/// A status, optionally with a reason the caller can read.
///
/// Handlers mostly return bare `StatusCode`, and that stays true:
/// `From<StatusCode>` gives an empty reason, so `?` on existing permission and
/// lookup helpers is unchanged and those responses keep exactly the shape they
/// had. What this adds is somewhere for a refusal's own words to travel, for
/// the cases where the status alone does not say enough - originally so a
/// repository conflict could say what it refused (see `stores::repository_error`),
/// now shared with the invoice/payment filter builders for the same reason:
/// a bare 400 and a bare "store_id required" 400 are indistinguishable on the
/// wire, which is exactly the ambiguity that burned the client once.
#[derive(Debug)]
pub struct ApiErr(StatusCode, String);

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        // An empty reason stays a bare status, which is what every handler here
        // returned before and what `From<StatusCode>` produces.
        //
        // Note what this does NOT fix: both branches send an empty body, so the
        // client still Displays a reasonless error as "HTTP error 404: ",
        // trailing colon and all. The difference is only that the bare branch
        // sends no `content-type` for a body that does not exist. Filling the
        // reason is what removes the colon, and that is the caller's job.
        if self.1.is_empty() {
            self.0.into_response()
        } else {
            (self.0, self.1).into_response()
        }
    }
}

impl From<StatusCode> for ApiErr {
    fn from(status: StatusCode) -> Self {
        Self(status, String::new())
    }
}

impl From<(StatusCode, String)> for ApiErr {
    fn from((status, reason): (StatusCode, String)) -> Self {
        Self(status, reason)
    }
}

/// Create the unified API router.
///
/// Mounts all sub-routers:
/// - `/health` - Health checks
/// - `/evm` - EVM operations (tokens, networks)
/// - `/auth` - Authentication
/// - `/stores` - Store management
#[allow(clippy::too_many_lines)] // route registration table — splitting hides the full API surface
pub fn router<A>(
    state: PgAppState<A>,
    enable_swagger: bool,
    rate_limiters: Option<Arc<rate_limit::RateLimitState>>,
    idempotency: Option<Arc<idempotency::IdempotencyState>>,
    api_key_rate_limiter: Option<Arc<api_key_rate_limit::ApiKeyRateLimitState>>,
) -> Router
where
    A: AuthenticationService + 'static,
{
    use axum::routing::{delete, patch, post, put};

    // Health endpoints with AppState
    let health_routes = Router::new()
        .route("/health", get(health::health_check::<A>))
        .route("/health/live", get(health::liveness))
        .route("/health/ready", get(health::readiness::<A>))
        .route("/health/deep", get(health::deep_health::<A>))
        .route("/health/chains", get(health::chains_health::<A>))
        .route("/metrics", get(health::prometheus_metrics::<A>))
        .with_state(state.clone());

    // Store endpoints
    let store_routes = Router::new()
        .route("/", get(stores::list_stores::<A>))
        .route("/", post(stores::create_store::<A>))
        .route("/{store_id}", get(stores::get_store::<A>))
        .route("/{store_id}", put(stores::update_store::<A>))
        .route("/{store_id}", delete(stores::delete_store::<A>))
        .route("/{store_id}/unarchive", post(stores::unarchive_store::<A>))
        .route("/{store_id}/invites", post(stores::create_store_invite::<A>))
        .route("/{store_id}/members", get(stores::list_store_members::<A>))
        .route("/{store_id}/members", post(stores::add_store_member::<A>))
        .route(
            "/{store_id}/members/{user_id}",
            put(stores::update_store_member::<A>),
        )
        .route(
            "/{store_id}/members/{user_id}",
            delete(stores::remove_store_member::<A>),
        )
        .route("/{store_id}/wallet", get(stores::get_store_wallet::<A>))
        .route("/{store_id}/wallet", put(stores::configure_store_wallet::<A>))
        .route("/{store_id}/wallet", delete(stores::delete_store_wallet::<A>))
        .route("/{store_id}/wallet/rotate", post(stores::rotate_store_wallet_audited::<A>))
        .route("/{store_id}/settings", get(stores::get_store_settings::<A>))
        .route("/{store_id}/settings", patch(stores::update_store_settings::<A>))
        .route("/{store_id}/webhook", get(stores::get_store_webhook::<A>))
        .route("/{store_id}/webhook", put(stores::configure_store_webhook::<A>))
        .route("/{store_id}/webhook", delete(stores::delete_store_webhook::<A>))
        // Payment methods
        .route("/{store_id}/payment-methods", get(stores::list_payment_methods::<A>))
        .route("/{store_id}/payment-methods", post(stores::create_payment_method::<A>))
        .route("/{store_id}/payment-methods/{method_id}", get(stores::get_payment_method::<A>))
        .route("/{store_id}/payment-methods/{method_id}", put(stores::update_payment_method::<A>))
        .route("/{store_id}/payment-methods/{method_id}", delete(stores::delete_payment_method::<A>))
        // Token Policy
        .route("/{store_id}/token-policy", get(stores::get_token_policy::<A>))
        .route("/{store_id}/token-policy", put(stores::set_token_policy::<A>))
        .route("/{store_id}/token-policy", delete(stores::delete_token_policy::<A>))
        // Settlement tolerance
        .route("/{store_id}/settlement-tolerance", get(stores::get_settlement_tolerance::<A>))
        .route("/{store_id}/settlement-tolerance", put(stores::set_settlement_tolerance::<A>))
        .route("/{store_id}/settlement-tolerance", delete(stores::delete_settlement_tolerance::<A>))
        // Payouts
        .route("/{store_id}/payouts", get(payouts::list_payouts::<A>))
        .route("/{store_id}/payouts", post(payouts::create_payout::<A>))
        .route("/{store_id}/payouts/{payout_id}", get(payouts::get_payout::<A>))
        // The merchant makes the payout from their own wallet, then records it
        // here; this server has no spending key and never broadcasts.
        .route(
            "/{store_id}/payouts/{payout_id}/settle",
            post(payouts::settle_payout::<A>),
        )
        // Releases the invoice claim a payout holds, so a payout recorded by
        // mistake does not lock that money out of every later payout.
        .route(
            "/{store_id}/payouts/{payout_id}/abandon",
            post(payouts::abandon_payout::<A>),
        )
        // Webhook deliveries
        .route(
            "/{store_id}/webhook-deliveries",
            get(webhook_deliveries::list_deliveries_for_store::<A>),
        )
        .route(
            "/{store_id}/webhook-deliveries/{delivery_id}/replay",
            post(webhook_deliveries::replay_delivery::<A>),
        )
        .with_state(state.clone());

    // Invoice endpoints (with idempotency middleware on POST)
    let mut invoice_routes = Router::new()
        .route("/", get(invoices::list_invoices::<A>))
        .route("/", post(invoices::create_invoice::<A>))
        .route("/export.csv", get(invoices::export_invoices_csv::<A>))
        .route(
            "/by-tx/{chain_id}/{tx_hash}",
            get(invoices::lookup_by_tx_hash::<A>),
        )
        .route("/{invoice_id}", get(invoices::get_invoice::<A>))
        .route(
            "/{invoice_id}/payments",
            get(invoices::get_invoice_payments::<A>),
        )
        .route(
            "/{invoice_id}/status",
            get(invoices::get_invoice_status::<A>),
        )
        .route(
            "/{invoice_id}/settlement-allowance",
            get(invoices::get_invoice_settlement_allowance::<A>),
        )
        .route("/{invoice_id}/cancel", post(invoices::cancel_invoice::<A>))
        .route("/{invoice_id}/refund", post(refunds::create_refund::<A>))
        .route("/{invoice_id}/refunds", get(refunds::list_refunds::<A>))
        .route(
            "/{invoice_id}/webhook-deliveries",
            get(webhook_deliveries::list_deliveries_for_invoice::<A>),
        )
        .with_state(state.clone());

    if let Some(idem) = idempotency {
        invoice_routes = invoice_routes.layer(axum::middleware::from_fn_with_state(
            idem,
            idempotency::middleware,
        ));
    }

    // Wallet endpoints (cross-store)
    let wallet_routes = Router::new()
        .route("/", get(stores::list_wallets::<A>))
        .route("/", post(stores::create_wallet::<A>))
        .route("/{wallet_id}", get(stores::get_wallet_by_id::<A>))
        .route("/{wallet_id}", patch(stores::update_wallet::<A>))
        .route("/{wallet_id}", delete(stores::delete_wallet::<A>))
        .route("/{wallet_id}/xpub", get(stores::export_wallet_xpub::<A>))
        .route(
            "/{wallet_id}/addresses",
            get(stores::list_wallet_addresses::<A>),
        )
        .with_state(state.clone());

    // Payment endpoints (store-scoped)
    let payment_routes = Router::new()
        .route("/", get(invoices::list_payments::<A>))
        .route("/export.csv", get(invoices::export_payments_csv::<A>))
        .route("/{payment_id}", get(invoices::get_payment::<A>))
        .with_state(state.clone());

    // WebSocket endpoint for real-time updates
    let ws_route = Router::new()
        .route("/ws", get(ws::ws_handler::<A>))
        .with_state(state.clone());

    // Rates endpoint
    let rates_routes = Router::new()
        .route("/", get(rates::get_rate::<A>))
        .with_state(state.clone());

    // Plugin static pages. Same route for merchant and admin views - the
    // handler resolves the viewer from the authenticated identity.
    let plugin_routes = Router::new()
        // What the client puts in its navigation. Declared in each plugin's
        // manifest, so building a menu runs no plugin code.
        .route("/", get(plugins::list_plugin_pages::<A>))
        .route("/{id}/pages/{*path}", get(plugins::get_page::<A>))
        // Core, not a plugin's: the standing gates invoice creation, so it is
        // stored by the host and lands whether or not any plugin is loaded.
        .route("/entitlements", entitlements::route::<A>())
        // Core too, for the same reason capability 7 itself is host-side: a
        // plugin cannot hold a recipient address, and this route is how a
        // service with a clock reaches it without one.
        .route("/account-notices", account_notices::route::<A>())
        .with_state(state.clone());

    // Dashboard endpoint
    let dashboard_routes = Router::new()
        .route("/stats", get(dashboard::get_stats::<A>))
        .route("/analytics", get(dashboard::get_analytics::<A>))
        .with_state(state.clone());

    // User endpoints (API keys)
    let user_routes = Router::new()
        // Refuses while the account's stores hold payments, payouts or refunds:
        // `users` cascades through `stores` into `invoices` and `payments`, so
        // deleting a merchant who traded would erase their financial history.
        .route("/me", delete(users::delete_account::<A>))
        // Email change (sensitive - see server/src/api/users/email.rs).
        // Set/change and remove require a fresh passkey or wallet login
        // (`FreshlyAuthenticatedUser`); confirm is unauthenticated by design
        // and gated on the verification token alone.
        .route("/me/email", post(users::request_email_change::<A>))
        .route("/me/email", delete(users::remove_email::<A>))
        .route("/me/email/status", get(users::get_email_status::<A>))
        .route("/me/standing", get(users::get_standing::<A>))
        // Owner-only read of a store's key-rotation history.
        .route(
            "/me/stores/{store_id}/wallet-rotations",
            get(users::get_wallet_rotations::<A>),
        )
        // Enrols the caller and creates an invoice: a POST, never a link.
        .route("/me/signup", post(users::signup::<A>))
        // Read-only: the plans a merchant may pass to `/me/signup`.
        .route("/me/plans", get(users::list_plans::<A>))
        .route(
            "/me/email/confirm",
            post(users::confirm_email_change::<A>),
        )
        // Redeeming a store invite is what creates the membership.
        .route("/me/invites/accept", post(stores::accept_store_invite::<A>))
        .route("/api-keys", get(users::list_api_keys::<A>))
        .route("/api-keys", post(users::create_api_key::<A>))
        .route("/api-keys/{id}", delete(users::revoke_api_key::<A>))
        .route(
            "/api-keys/{id}",
            axum::routing::patch(users::update_api_key::<A>),
        )
        .route(
            "/api-keys/{id}/rotate",
            axum::routing::post(users::rotate_api_key::<A>),
        )
        .route("/wallets", get(users::list_wallet_credentials::<A>))
        // Device enrollment (sensitive - see server/src/api/users/devices.rs).
        // Requires a fresh passkey or wallet login (`FreshlyAuthenticatedUser`),
        // same bar as the email-change endpoints above.
        .route("/me/devices", post(users::add_device::<A>))
        .route(
            "/wallets/{id}/reauth-challenge",
            axum::routing::post(users::create_wallet_reauth_challenge::<A>),
        )
        .route(
            "/wallets/{id}/primary",
            axum::routing::patch(users::set_primary_wallet_credential::<A>),
        )
        .with_state(state.clone());
    // Admin endpoints (ServerAdmin only)
    let admin_routes = Router::new()
        .route("/users", get(admin::list_users::<A>))
        .route("/users/{id}", delete(admin::delete_user_account::<A>))
        .route("/users/{id}/stores", get(admin::list_user_stores::<A>))
        .route("/stores", get(admin::list_stores::<A>))
        .route("/stores/{id}", delete(admin::hard_delete_store::<A>))
        .route(
            "/users/{id}/role",
            axum::routing::patch(admin::update_user_role::<A>),
        )
        .route(
            "/users/{id}/lock",
            axum::routing::post(admin::lock_user::<A>),
        )
        .route(
            "/users/{id}/unlock",
            axum::routing::post(admin::unlock_user::<A>),
        )
        .route("/settings", get(admin::get_settings::<A>))
        .route("/settings", axum::routing::put(admin::update_settings::<A>))
        .route("/safe-mode", get(admin::get_safe_mode::<A>))
        .route(
            "/plugins",
            get(admin::plugins::list_plugins::<A>).post(admin::plugins::install_plugin::<A>),
        )
        .route(
            "/plugins/{id}",
            delete(admin::plugins::uninstall_plugin::<A>),
        )
        .route(
            "/plugins/{id}/enable",
            post(admin::plugins::enable_plugin::<A>),
        )
        .route(
            "/plugins/{id}/disable",
            post(admin::plugins::disable_plugin::<A>),
        )
        .route(
            "/plugins/{id}/events",
            get(admin::plugins::plugin_events::<A>),
        )
        .route(
            "/plugins/{id}/accounts/{account_id}/cancel-subscription",
            post(admin::plugins::cancel_plugin_subscription::<A>),
        )
        .with_state(state.clone());

    // Auth API from auth crate (with optional CAPTCHA provider)
    let auth_state = match state.captcha_provider.clone() {
        Some(captcha) => auth::api::AuthState::with_captcha(state.auth_service.clone(), captcha),
        None => auth::api::AuthState::new(state.auth_service.clone()),
    };
    let auth_routes = auth::api::router(auth_state);

    // EVM API has its own state
    let evm_routes = evm::api::router(state.to_evm_state());

    // Public checkout endpoints (no auth required)
    let checkout_routes = Router::new()
        .route("/{invoice_id}", get(checkout::get_checkout::<A>))
        .route("/ws", get(checkout::checkout_ws_handler::<A>))
        .with_state(state.clone());

    // Combine all routes
    let mut app = Router::new()
        .merge(health_routes)
        .nest("/checkout", checkout_routes)
        .nest("/stores", store_routes)
        .nest("/wallets", wallet_routes)
        .nest("/invoices", invoice_routes)
        .nest("/payments", payment_routes)
        .merge(ws_route)
        .nest("/rates", rates_routes)
        .nest("/plugins", plugin_routes)
        .nest("/dashboard", dashboard_routes)
        .nest("/users", user_routes)
        .nest("/admin", admin_routes)
        .nest("/auth", auth_routes)
        .nest("/evm", evm_routes);

    // Apply per-API-key rate limiting (runs after IP-tier, both must pass)
    if let Some(api_key_limiter) = api_key_rate_limiter {
        app = app.layer(axum::middleware::from_fn_with_state(
            api_key_limiter,
            api_key_rate_limit::middleware,
        ));
    }

    // Apply IP-tier rate limiting middleware if configured
    if let Some(limiters) = rate_limiters {
        app = app.layer(axum::middleware::from_fn_with_state(
            limiters,
            rate_limit::middleware,
        ));
    }

    // Add Swagger UI if enabled
    if enable_swagger {
        // Merge all API docs
        let mut openapi = ApiDoc::openapi();
        let evm_openapi = evm::api::EvmApiDoc::openapi();
        let auth_openapi = auth::AuthApiDoc::openapi();

        // Merge paths from EVM API
        for (path, item) in evm_openapi.paths.paths {
            openapi.paths.paths.insert(format!("/evm{}", path), item);
        }

        // Merge paths from Auth API
        for (path, item) in auth_openapi.paths.paths {
            openapi.paths.paths.insert(format!("/auth{}", path), item);
        }

        // Merge schemas from EVM API
        if let Some(evm_components) = evm_openapi.components {
            let components = openapi.components.get_or_insert_with(Default::default);
            for (name, schema) in evm_components.schemas {
                components.schemas.insert(name, schema);
            }
        }

        // Merge schemas from Auth API
        if let Some(auth_components) = auth_openapi.components {
            let components = openapi.components.get_or_insert_with(Default::default);
            for (name, schema) in auth_components.schemas {
                components.schemas.insert(name, schema);
            }
        }

        app = app.merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", openapi));
    }

    // API-key deprecation response header — runs on every response so any
    // handler that was reached via a deprecated-but-in-grace key gets the
    // `X-API-Key-Deprecated` header stamped automatically.
    app = app.layer(axum::middleware::from_fn(api_key_deprecation::middleware));

    // Expiry warning header — stamped on responses to authenticated requests
    // from an account whose paid period is about to end.
    app = app.layer(axum::middleware::from_fn_with_state(
        Arc::new(standing_warning::StandingWarningState::new(
            state.data_service.clone(),
        )),
        standing_warning::middleware,
    ));

    // HTTP request metrics (innermost layer = runs closest to handlers)
    app = app.layer(axum::middleware::from_fn(http_metrics::middleware));

    app
}

/// Add every outer HTTP layer `bin/server.rs::main` puts on top of
/// [`router`]: request tracing, permissive CORS, then the Sentry
/// performance-tracing pair (a transaction per request, named from the
/// matched route pattern via `tower-axum-matched-path` rather than the raw
/// request URI — without it, every distinct invoice/store/etc. id mints its
/// own transaction name, unbounded cardinality and an unreadable performance
/// page).
///
/// Pulled out of `main` so the integration test in
/// `server/tests/sentry_transaction_naming.rs` calls this exact function, in
/// the exact order it adds these layers, instead of hand-copying `.layer()`
/// calls. An earlier version of this function added only the two Sentry
/// layers and left `TraceLayer`/`CorsLayer` inline in `main`, ahead of it —
/// so the test exercised a router `main` never actually serves, and a future
/// layer inserted between the two call sites in `main` would have regressed
/// route-pattern naming with nothing to catch it. There is now only one call
/// site for the full stack, in `main` and in the test alike.
pub fn with_sentry_performance_tracing(router: Router) -> Router {
    let router = router
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(
            tower_http::cors::CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any),
        );
    // Axum runs middleware in the reverse order it's `.layer()`-ed, so
    // `NewSentryLayer` must be added last to end up outermost of
    // `SentryHttpLayer`, per sentry-tower's documented ordering.
    router
        .layer(sentry::integrations::tower::SentryHttpLayer::new().enable_transaction())
        .layer(sentry::integrations::tower::NewSentryLayer::<
            axum::extract::Request,
        >::new_from_top())
}
