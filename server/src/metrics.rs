//! Application metrics for ethpayserver.
//!
//! Provides application-level metrics for monitoring invoice processing,
//! payment detection, webhook delivery, and service health. Every metric goes
//! through the vendor-neutral `metrics` facade and is recorded to both
//! Prometheus (`/metrics`, scraped from outside the process) and Sentry
//! (pushed from inside, so it can raise threshold alerts) - see
//! [`FanoutRecorder`].
//!
//! ## Alerting on a gauge in Sentry
//!
//! Sentry buckets metric points over a flush window and exposes the bucket
//! through an aggregation (min/max/avg/sum/...), not the raw last-write value
//! Prometheus's `/metrics` shows. Picking the wrong aggregation for a health
//! alert can hide the exact failure it exists to catch: `avg` on
//! `payserver_chain_healthy` (1/0) would only dip below 1, not hit 0, while
//! the chain is unhealthy for part of a window. The alert rule on
//! `payserver_chain_healthy` should use `min` (catches any 0 in the window);
//! the rule on `payserver_chain_block_lag` should use `max` (catches the
//! worst lag in the window, not one smoothed by a moment of catching up).
//! This is configured where the alert rule itself lives, in Sentry, not in
//! this module - there is nothing here to set.
//!
//! **Verification status: the SDK-level contract is confirmed, the alert
//! itself is not.** `fanout_recorder_forwards_real_chain_health_gauges_to_sentry`
//! (in this module's tests) proves `payserver_chain_healthy` and
//! `payserver_chain_block_lag` reach Sentry as `Gauge`-typed points with the
//! right value and `chain_id` attribute, through the real
//! `set_chain_healthy`/`set_chain_blocks` call sites - everything this
//! process controls. Whether a threshold alert rule configured against that
//! gauge actually fires is a property of the Sentry project the rule lives
//! in, not of this code, and there is no test-double for "does the alert
//! fire" - it needs a live Sentry project to check by creating the rule and
//! triggering it. That check was not done: no live Sentry project was
//! available in this change. Per the ticket that added this module, an
//! unconfirmed alert is the one finding that decides whether metrics also
//! ship to a second vendor (Grafana) as a fallback alerting path - treat
//! this as unconfirmed, not as working, until someone with access to the
//! real Sentry project creates the two rules and watches one fire. That
//! check has been split out into its own follow-up for whoever has that
//! access, rather than left to be rediscovered here on every future change
//! to this module. It is deliberately not cited by ticket ID here - this
//! is a public repository, and this codebase's convention (see the repo's
//! own contributor docs) is that tracker references never go in source,
//! only in commit messages and PR titles. The absence of an ID in this
//! comment is that policy applied, not an unlinked or forgotten follow-up.
//!
//! ## Volume cost before mainnet
//!
//! Sentry bills metrics ingest at $0.50/GB beyond a 5GB/month included quota
//! (sentry.io/pricing, checked 2026-09-22). Serializing an actual captured
//! [`sentry::protocol::Metric`] gives the per-point wire size:
//!
//! - A `payserver_chain_healthy`/`payserver_chain_block_lag` gauge (one
//!   `chain_id` attribute) is ~200 bytes. At the default 15s poll
//!   (`CHAIN_HEALTH_METRICS_INTERVAL_SECS`), [`set_chain_blocks`] and
//!   [`set_chain_healthy`] together emit 4 gauges per poll per chain: 5760
//!   polls/day * 4 * ~200B ≈ 4.4MB/day, ~130MB/month, per configured chain -
//!   negligible even with a dozen chains watched.
//! - [`record_http_request`] fires unconditionally on every HTTP request via
//!   middleware: a counter + a histogram observation, each carrying
//!   `method`/`path`/`status`, ~300 bytes apiece, ~600B/request. That reaches
//!   the free tier's line at ~8.9M requests/month, and every request past
//!   that costs ~$0.28/million. This is the metric worth sizing against real
//!   mainnet traffic before launch - not the two gauges this ticket is
//!   actually about, which cost nothing by comparison.

use metrics::{
    Counter, CounterFn, Gauge, GaugeFn, Histogram, HistogramFn, Key, KeyName, Metadata, Recorder,
    SharedString, Unit, counter, describe_counter, describe_gauge, describe_histogram, gauge,
    histogram,
};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle, PrometheusRecorder};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Global metrics handle for rendering.
static METRICS_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Initialize the metrics recorder: Prometheus for `/metrics`, and Sentry's
/// Application Metrics API alongside it so gauges like
/// `payserver_chain_healthy` can raise threshold alerts.
///
/// `metrics::set_global_recorder` accepts only one recorder per process, so
/// [`FanoutRecorder`] wraps the Prometheus recorder rather than the two being
/// installed side by side - every one of the ~69 call sites across the
/// codebase keeps using the same `metrics` facade macros unchanged.
///
/// Must be called once at startup, and after `evm::telemetry::init_sentry` -
/// `sentry::metrics::*().capture()` writes to `Hub::current()`, so a call
/// here before Sentry has configured the process Hub would silently capture
/// against a disabled client. `server/src/bin/server.rs` calls `init_sentry`
/// first for this reason; keep that order if this call site ever moves.
///
/// That ordering is checked at runtime, not just documented: this refuses to
/// install the recorder (returning an error rather than a panic, since a
/// misordered boot should fail the same way any other startup precondition
/// does) unless [`sentry_hub_has_client`] confirms `init_sentry` already ran.
/// Panics if called twice.
pub fn init_metrics() -> anyhow::Result<()> {
    anyhow::ensure!(
        sentry_hub_has_client(),
        "init_metrics called before evm::telemetry::init_sentry - every metric captured \
         to Sentry from this point on would write to a Hub with no client bound and be \
         silently dropped, including payserver_chain_healthy/payserver_chain_block_lag"
    );

    let prometheus = PrometheusBuilder::new().build_recorder();
    let handle = prometheus.handle();
    metrics::set_global_recorder(FanoutRecorder { prometheus })
        .map_err(|_| anyhow::anyhow!("metrics recorder already installed"))?;

    // Store handle globally - panic if already set (programming error)
    if METRICS_HANDLE.set(handle).is_err() {
        panic!("init_metrics called twice");
    }

    // Describe all metrics
    describe_counters();
    describe_gauges();
    describe_histograms();

    Ok(())
}

/// Whether `sentry::init` has already bound a client to the process `Hub`.
///
/// `sentry::init` calls `Hub::current().bind_client(Some(client))`
/// unconditionally - even with no `SENTRY_DSN` set, it binds a client that's
/// simply disabled (drops events instead of sending them) rather than
/// leaving the client slot empty. So this distinguishes "`init_sentry` ran"
/// from "`init_sentry` has not run yet", not "a DSN is configured" - exactly
/// the distinction [`init_metrics`] needs, since a disabled client still
/// makes `sentry::metrics::*().capture()` a safe no-op, while no client at
/// all means the call silently vanishes with no client to even be disabled.
fn sentry_hub_has_client() -> bool {
    sentry::Hub::current().client().is_some()
}

/// Forwards every counter/gauge/histogram registration to the wrapped
/// Prometheus recorder - so `/metrics` renders exactly as before - and, on
/// every recorded value, also captures the same observation through Sentry's
/// metrics API. The Sentry side is vendor-specific and lives only here; every
/// call site still goes through the vendor-neutral `metrics` facade.
struct FanoutRecorder {
    prometheus: PrometheusRecorder,
}

impl Recorder for FanoutRecorder {
    fn describe_counter(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.prometheus.describe_counter(key, unit, description);
    }

    fn describe_gauge(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.prometheus.describe_gauge(key, unit, description);
    }

    fn describe_histogram(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.prometheus.describe_histogram(key, unit, description);
    }

    fn register_counter(&self, key: &Key, metadata: &Metadata<'_>) -> Counter {
        let prometheus = self.prometheus.register_counter(key, metadata);
        Counter::from_arc(Arc::new(SentryCounter {
            prometheus,
            key: key.clone(),
        }))
    }

    fn register_gauge(&self, key: &Key, metadata: &Metadata<'_>) -> Gauge {
        let prometheus = self.prometheus.register_gauge(key, metadata);
        Gauge::from_arc(Arc::new(SentryGauge {
            prometheus,
            key: key.clone(),
        }))
    }

    fn register_histogram(&self, key: &Key, metadata: &Metadata<'_>) -> Histogram {
        let prometheus = self.prometheus.register_histogram(key, metadata);
        Histogram::from_arc(Arc::new(SentryHistogram {
            prometheus,
            key: key.clone(),
        }))
    }
}

/// Sends a metric to Sentry with the key's labels attached as attributes, so
/// e.g. `payserver_chain_healthy{chain_id="1"}` can be filtered/alerted on
/// per chain in Sentry the same way it can be queried per chain in Prometheus.
///
/// Values go through the same secret/PII scrubber the error path uses
/// ([`evm::telemetry::redact_secrets`]). The error path's `before_send` hook
/// only runs on error events - this is a separate envelope - so without this
/// a call site that labels a metric with an address, tx hash, or email would
/// leak it to Sentry with none of the protection `scrub_event` gives
/// everything else.
fn sentry_labels(key: &Key) -> impl Iterator<Item = (String, String)> + '_ {
    key.labels().map(|label| {
        (
            label.key().to_string(),
            evm::telemetry::redact_secrets(label.value()),
        )
    })
}

struct SentryCounter {
    prometheus: Counter,
    key: Key,
}

impl CounterFn for SentryCounter {
    fn increment(&self, value: u64) {
        // `value as f64` is lossy above 2^53 - fine for every counter today
        // (each is an event count, not an asset-denominated total - see the
        // module doc), but this trait boundary has no way to stop a future
        // counter from carrying wei/satoshi volume instead, at which point
        // Prometheus would stay exact while this silently rounds. Sentry's
        // counter API is `f64`-only, so there is no lossless alternative
        // here - a call site that starts recording amounts through a
        // `Counter` needs to keep the exact total in Prometheus and treat
        // the Sentry side as an approximation, not the other way round.
        self.prometheus.increment(value);
        let metric = sentry_labels(&self.key).fold(
            sentry::metrics::counter(self.key.name().to_string(), value as f64),
            |metric, (k, v)| metric.attribute(k, v),
        );
        metric.capture();
    }

    fn absolute(&self, value: u64) {
        // No call site sets a counter to an absolute value today, and Sentry
        // counters are delta-additive with no "set to X" verb to translate
        // this into - forward to Prometheus, which does support it, only.
        // Warn (release builds too, unlike debug_assert) so a future call
        // site that starts doing this doesn't silently diverge Sentry from
        // Prometheus with no signal at all - same treatment as the gauge
        // increment()/decrement() case below.
        tracing::warn!(
            metric = %self.key.name(),
            "counter.absolute() is not forwarded to Sentry - only increment() is"
        );
        debug_assert!(
            false,
            "counter {} called absolute() - not forwarded to Sentry",
            self.key.name()
        );
        self.prometheus.absolute(value);
    }
}

struct SentryGauge {
    prometheus: Gauge,
    key: Key,
}

impl GaugeFn for SentryGauge {
    fn increment(&self, value: f64) {
        // No call site increments/decrements a gauge today (every gauge here
        // is `.set()`); Sentry gauges take a point-in-time value, and a bare
        // delta can't be turned into one without duplicating the Prometheus
        // atomic here, so only `set` is forwarded to Sentry. Warn rather than
        // silently diverge if that ever changes - a gauge feeding a threshold
        // alert (e.g. `payserver_chain_healthy`) would go stale in Sentry
        // while `/metrics` kept moving, with nothing else to say so.
        tracing::warn!(
            metric = %self.key.name(),
            "gauge.increment() is not forwarded to Sentry - only set() is"
        );
        debug_assert!(
            false,
            "gauge {} called increment() - not forwarded to Sentry",
            self.key.name()
        );
        self.prometheus.increment(value);
    }

    fn decrement(&self, value: f64) {
        tracing::warn!(
            metric = %self.key.name(),
            "gauge.decrement() is not forwarded to Sentry - only set() is"
        );
        debug_assert!(
            false,
            "gauge {} called decrement() - not forwarded to Sentry",
            self.key.name()
        );
        self.prometheus.decrement(value);
    }

    fn set(&self, value: f64) {
        self.prometheus.set(value);
        let metric = sentry_labels(&self.key).fold(
            sentry::metrics::gauge(self.key.name().to_string(), value),
            |metric, (k, v)| metric.attribute(k, v),
        );
        metric.capture();
    }
}

struct SentryHistogram {
    prometheus: Histogram,
    key: Key,
}

impl HistogramFn for SentryHistogram {
    fn record(&self, value: f64) {
        self.prometheus.record(value);
        let metric = sentry_labels(&self.key).fold(
            sentry::metrics::distribution(self.key.name().to_string(), value),
            |metric, (k, v)| metric.attribute(k, v),
        );
        metric.capture();
    }
}

/// Render metrics in Prometheus format.
///
/// Returns None if metrics were not initialized.
pub fn render() -> Option<String> {
    METRICS_HANDLE.get().map(|h| h.render())
}

fn describe_counters() {
    // Invoice metrics
    describe_counter!(
        "ethpayserver_wallet_rotation_alerts_total",
        "Wallet key rotations that raised an alert, by reason (rate, not_fresh)"
    );
    describe_counter!(
        "ethpayserver_invoices_created_total",
        "Total number of invoices created"
    );
    describe_counter!(
        "ethpayserver_invoices_paid_total",
        "Total number of invoices fully paid"
    );
    describe_counter!(
        "ethpayserver_invoices_expired_total",
        "Total number of invoices that expired"
    );
    describe_counter!(
        "ethpayserver_invoices_cancelled_total",
        "Total number of invoices cancelled"
    );

    // Payment metrics
    describe_counter!(
        "ethpayserver_payments_detected_total",
        "Total number of payments detected"
    );
    describe_counter!(
        "ethpayserver_payments_confirmed_total",
        "Total number of payments confirmed"
    );

    // Webhook metrics
    describe_counter!(
        "ethpayserver_webhooks_queued_total",
        "Total number of webhooks queued"
    );
    describe_counter!(
        "ethpayserver_webhooks_delivered_total",
        "Total number of webhooks successfully delivered"
    );
    describe_counter!(
        "ethpayserver_webhooks_failed_total",
        "Total number of webhook delivery failures"
    );

    // Webhook delivery tracking
    describe_counter!(
        "ethpayserver_webhook_deliveries_total",
        "Total webhook deliveries by status (delivered, retrying, permanent_failed)"
    );

    // Store metrics
    describe_counter!(
        "ethpayserver_stores_created_total",
        "Total number of stores created"
    );

    // Rate limiting metrics
    describe_counter!(
        "ethpayserver_rate_limited_total",
        "Total number of requests rejected by rate limiting"
    );

    // Refund metrics
    //
    // No `initiated` counter here: `record_refund_initiated` was removed
    // along with its only caller when POST /invoices/{id}/refund stopped
    // creating refund rows. Registering a description for a counter nothing
    // increments would be the same "implies a capability" problem this
    // change exists to fix, just in the metrics namespace instead of the API
    // surface.
    describe_counter!(
        "ethpayserver_refunds_confirmed_total",
        "Total number of refunds confirmed"
    );
    describe_counter!(
        "ethpayserver_refunds_failed_total",
        "Total number of refunds that failed"
    );

    // Payout metrics
    describe_counter!(
        "ethpayserver_payouts_initiated_total",
        "Total number of payouts initiated"
    );
    describe_counter!(
        "ethpayserver_payouts_confirmed_total",
        "Total number of payouts confirmed"
    );
    describe_counter!(
        "ethpayserver_payouts_failed_total",
        "Total number of payouts that failed"
    );
}

fn describe_gauges() {
    describe_gauge!(
        "ethpayserver_webhook_queue_depth",
        "Current number of webhooks in the queue"
    );
    describe_gauge!(
        "ethpayserver_webhook_ready_queue_depth",
        "Number of webhooks ready for immediate delivery"
    );
    describe_gauge!(
        "ethpayserver_watched_addresses",
        "Current number of watched addresses per chain"
    );
    describe_gauge!(
        "payserver_chain_current_block",
        "Current block height on chain, as reported by the RPC source"
    );
    describe_gauge!(
        "payserver_chain_last_processed_block",
        "Last block height processed by the monitor"
    );
    describe_gauge!(
        "payserver_chain_block_lag",
        "Blocks between chain head and the last block the monitor has processed"
    );
    describe_gauge!(
        "payserver_chain_healthy",
        "Whether the chain's monitor connection is healthy (1) or not (0)"
    );
    describe_gauge!(
        "ethpayserver_registered_users",
        "Total number of registered users"
    );
    describe_gauge!("ethpayserver_stores", "Total number of stores");

    // DB pool gauges
    describe_gauge!(
        "ethpayserver_db_pool_connections",
        "Current number of connections in the DB pool by state"
    );

    describe_gauge!(
        "ethpayserver_idempotency_healthy",
        "Whether the idempotency control is connected to Redis and its last \
         GET/SET NX succeeded (1) or not (0) - 0 covers all three ways it \
         can be degraded: not yet connected at boot, a cache GET failure, \
         and a lock SET NX failure"
    );
}

fn describe_histograms() {
    describe_histogram!(
        "ethpayserver_payment_confirmation_duration_seconds",
        "Time from payment detected to confirmed"
    );
    describe_histogram!(
        "ethpayserver_webhook_delivery_duration_seconds",
        "HTTP round-trip time per webhook delivery attempt"
    );
    describe_histogram!(
        "ethpayserver_webhook_retry_attempts",
        "Webhook delivery attempt number on failure"
    );
    describe_histogram!(
        "ethpayserver_http_request_duration_seconds",
        "API request latency"
    );
    describe_counter!("ethpayserver_http_requests_total", "Total HTTP requests");

    // DB pool histograms
    describe_histogram!(
        "ethpayserver_db_pool_wait_duration_seconds",
        "Time spent waiting to acquire a DB pool connection"
    );

    // RPC histograms and counters
    describe_histogram!(
        "ethpayserver_rpc_request_duration_seconds",
        "Duration of individual RPC calls by chain and method"
    );
    describe_counter!(
        "ethpayserver_rpc_requests_total",
        "Total RPC requests by chain, method, and status"
    );
    describe_counter!(
        "ethpayserver_rpc_errors_total",
        "Total RPC errors by chain, method, and error kind"
    );
}

// ============================================================================
// Metric recording functions
// ============================================================================

/// Record an invoice creation.
pub fn record_invoice_created(currency: &str) {
    counter!("ethpayserver_invoices_created_total", "currency" => currency.to_string())
        .increment(1);
}

/// Record an invoice paid.
pub fn record_invoice_paid() {
    counter!("ethpayserver_invoices_paid_total").increment(1);
}

/// Record an invoice expiration.
pub fn record_invoice_expired() {
    counter!("ethpayserver_invoices_expired_total").increment(1);
}

/// Record an invoice cancellation.
pub fn record_invoice_cancelled() {
    counter!("ethpayserver_invoices_cancelled_total").increment(1);
}

/// Record a payment detection.
pub fn record_payment_detected(chain_id: &types::ChainId, asset_symbol: &str) {
    counter!(
        "ethpayserver_payments_detected_total",
        "chain_id" => chain_id.to_string(),
        "asset_symbol" => asset_symbol.to_string()
    )
    .increment(1);
}

/// Record a payment confirmation.
pub fn record_payment_confirmed(chain_id: &types::ChainId, asset_symbol: &str) {
    counter!(
        "ethpayserver_payments_confirmed_total",
        "chain_id" => chain_id.to_string(),
        "asset_symbol" => asset_symbol.to_string()
    )
    .increment(1);
}

/// Record a webhook queued.
pub fn record_webhook_queued(event_type: &str) {
    counter!(
        "ethpayserver_webhooks_queued_total",
        "event_type" => event_type.to_string()
    )
    .increment(1);
}

/// Record a successful webhook delivery.
pub fn record_webhook_delivered(event_type: &str) {
    counter!(
        "ethpayserver_webhooks_delivered_total",
        "event_type" => event_type.to_string()
    )
    .increment(1);
}

/// Record a failed webhook delivery.
pub fn record_webhook_failed(event_type: &str) {
    counter!(
        "ethpayserver_webhooks_failed_total",
        "event_type" => event_type.to_string()
    )
    .increment(1);
}

/// Record a webhook delivery outcome by status (delivered, retrying, permanent_failed).
pub fn record_webhook_delivery_status(status: &str) {
    counter!(
        "ethpayserver_webhook_deliveries_total",
        "status" => status.to_string()
    )
    .increment(1);
}

/// Record a webhook retry attempt number as a histogram observation.
pub fn record_webhook_retry_attempt(attempt: u32) {
    histogram!("ethpayserver_webhook_retry_attempts").record(f64::from(attempt));
}

/// Update the webhook queue depth gauge (total jobs in ZSET).
pub fn set_webhook_queue_depth(depth: u64) {
    gauge!("ethpayserver_webhook_queue_depth").set(depth as f64);
}

/// Update the ready-queue depth gauge (jobs with `scheduled_at` <= now).
pub fn set_webhook_ready_queue_depth(depth: u64) {
    gauge!("ethpayserver_webhook_ready_queue_depth").set(depth as f64);
}

/// Update the watched addresses gauge for a chain.
pub fn set_watched_addresses(chain_id: u64, count: usize) {
    gauge!(
        "ethpayserver_watched_addresses",
        "chain_id" => chain_id.to_string()
    )
    .set(count as f64);
}

/// Update the per-chain block gauges: current head, last processed, and the
/// lag between them. `None` values (source not yet connected) are skipped
/// rather than recorded as zero, which would read as fully caught up.
pub fn set_chain_blocks(
    chain_id: u64,
    current_block: Option<u64>,
    last_processed_block: Option<u64>,
) {
    let chain_id = chain_id.to_string();

    if let Some(current) = current_block {
        gauge!("payserver_chain_current_block", "chain_id" => chain_id.clone()).set(current as f64);
    }
    if let Some(last_processed) = last_processed_block {
        gauge!("payserver_chain_last_processed_block", "chain_id" => chain_id.clone())
            .set(last_processed as f64);
    }
    if let (Some(current), Some(last_processed)) = (current_block, last_processed_block) {
        let lag = current.saturating_sub(last_processed);
        gauge!("payserver_chain_block_lag", "chain_id" => chain_id).set(lag as f64);
    }
}

/// Update the per-chain healthy gauge (1 = healthy, 0 = not).
pub fn set_chain_healthy(chain_id: u64, is_healthy: bool) {
    gauge!(
        "payserver_chain_healthy",
        "chain_id" => chain_id.to_string()
    )
    .set(if is_healthy { 1.0 } else { 0.0 });
}

/// Update the idempotency-control health gauge (1 = healthy, 0 = not). See
/// `describe_gauges` for what "not" covers.
pub fn set_idempotency_healthy(healthy: bool) {
    gauge!("ethpayserver_idempotency_healthy").set(if healthy { 1.0 } else { 0.0 });
}

/// Update the registered users gauge.
pub fn set_registered_users(count: u64) {
    gauge!("ethpayserver_registered_users").set(count as f64);
}

/// Update the stores gauge.
pub fn set_stores(count: u64) {
    gauge!("ethpayserver_stores").set(count as f64);
}

/// Record a store creation.
pub fn record_store_created() {
    counter!("ethpayserver_stores_created_total").increment(1);
}

/// Record a payout initiation.
pub fn record_payout_initiated(chain_id: &types::ChainId, asset_symbol: &str) {
    counter!(
        "ethpayserver_payouts_initiated_total",
        "chain_id" => chain_id.to_string(),
        "asset_symbol" => asset_symbol.to_string()
    )
    .increment(1);
}

/// Record a rate-limited request.
pub fn record_rate_limited(tier: &str) {
    counter!(
        "ethpayserver_rate_limited_total",
        "tier" => tier.to_string()
    )
    .increment(1);
}

// ============================================================================
// Histogram recording functions
// ============================================================================

/// Record the duration from payment detected to confirmed.
pub fn record_payment_confirmation_duration(
    chain_id: &types::ChainId,
    asset_symbol: &str,
    duration: Duration,
) {
    histogram!(
        "ethpayserver_payment_confirmation_duration_seconds",
        "chain_id" => chain_id.to_string(),
        "asset_symbol" => asset_symbol.to_string()
    )
    .record(duration.as_secs_f64());
}

/// Record a webhook delivery round-trip duration.
pub fn record_webhook_delivery_duration(event_type: &str, success: bool, duration: Duration) {
    histogram!(
        "ethpayserver_webhook_delivery_duration_seconds",
        "event_type" => event_type.to_string(),
        "status" => if success { "ok" } else { "error" }.to_string()
    )
    .record(duration.as_secs_f64());
}

/// Record an HTTP request.
pub fn record_http_request(method: &str, path: &str, status: u16, duration: Duration) {
    let labels = [
        ("method", method.to_string()),
        ("path", path.to_string()),
        ("status", status.to_string()),
    ];
    counter!("ethpayserver_http_requests_total", &labels).increment(1);
    histogram!("ethpayserver_http_request_duration_seconds", &labels)
        .record(duration.as_secs_f64());
}

// ============================================================================
// DB pool metric functions
// ============================================================================

/// Set the DB pool connections gauge for a given state (idle or used).
pub fn set_db_pool_connections(state: &str, count: u64) {
    gauge!(
        "ethpayserver_db_pool_connections",
        "state" => state.to_string()
    )
    .set(count as f64);
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics_exporter_prometheus::PrometheusBuilder;

    /// Install a per-test Prometheus recorder and return its handle.
    ///
    /// Must be called at most once per test process. Since the global recorder
    /// is a singleton, parallel tests that each call this will race. In
    /// practice the CI runner executes server unit tests in a single thread
    /// (--test-threads=1 for lib tests) so this is safe.
    #[allow(clippy::expect_used)]
    fn test_recorder() -> metrics_exporter_prometheus::PrometheusHandle {
        let builder = PrometheusBuilder::new();
        let handle = builder.install_recorder().expect("recorder already set");
        describe_counters();
        describe_gauges();
        describe_histograms();
        handle
    }

    // This test's own process has not called the real, unscoped
    // `sentry::init` - it relies on nextest running every `#[test]` as its
    // own process (the same isolation
    // `init_metrics_installs_a_working_recorder_once_sentry_is_ready` below
    // depends on), not on nothing in this binary calling it: that test does
    // call `evm::telemetry::init_sentry` a few tests down, which binds a real
    // client to *its* process's Hub. Every Sentry-touching test above that
    // line instead goes through `sentry::test::with_captured_envelopes`,
    // which binds a client to a scoped `Hub` for the duration of its closure
    // only and restores the previous (clientless) `Hub` afterwards. So in
    // this test's own process the `Hub` reliably has no client bound, the
    // same as in a fresh process that hasn't called
    // `evm::telemetry::init_sentry` yet - which is what makes the next two
    // tests meaningful rather than order-dependent. Under a runner that
    // doesn't isolate tests by process (plain `cargo test`, or a future
    // harness change), this would become order-dependent for real.
    #[test]
    fn sentry_hub_has_client_is_false_before_sentry_init() {
        assert!(
            !sentry_hub_has_client(),
            "no test in this file calls the real sentry::init, so the process Hub must \
             not have a client bound here - if it does, something is leaking a bound \
             client across tests and every test below this in the run order is suspect"
        );
    }

    // Pins the runtime guard `init_metrics` added specifically because the
    // Sentry-forwarding half of every metric depends on an ordering
    // invariant (`init_sentry` before `init_metrics`) that used to be
    // documented only in a comment - nothing would have caught it if
    // `server/src/bin/server.rs` ever got the two calls backwards. This
    // drives `init_metrics` for real, with no client bound to the Hub (see
    // `sentry_hub_has_client_is_false_before_sentry_init` above), and checks
    // it fails closed instead of installing a recorder that would silently
    // drop every metric sent to Sentry, including
    // `payserver_chain_healthy`/`payserver_chain_block_lag`.
    //
    // Safe to call the real `init_metrics` here even though
    // `metrics::set_global_recorder` is a process-wide singleton: the
    // ordering check runs first and returns an error before that call is
    // ever reached, so this can't poison the recorder for any other test.
    #[test]
    #[allow(clippy::expect_used)]
    fn init_metrics_refuses_to_start_before_sentry_is_initialized() {
        let err = init_metrics().expect_err(
            "init_metrics must fail closed when called before evm::telemetry::init_sentry \
             has bound a client to the process Hub, not silently install a recorder that \
             drops every metric it tries to send to Sentry",
        );
        assert!(
            err.to_string().contains("init_sentry"),
            "error should name the missing precondition, got: {err}"
        );
    }

    // The test above pins the fail-closed path; nothing before this pinned
    // the success path, which is the one that actually runs in production.
    // Binds a real (disabled, since no `SENTRY_DSN` is set in a test
    // process) client via `evm::telemetry::init_sentry` - the exact
    // precondition `init_metrics` checks for - then drives a metric through
    // the recorder that call installs and confirms it reaches `render()`,
    // the same function `/metrics` calls in production. Safe to run as its
    // own process under nextest, same as `test_recorder()` and the guard
    // test above: each gets `metrics::set_global_recorder`'s one shot per
    // process to itself.
    #[test]
    #[allow(clippy::expect_used)]
    fn init_metrics_installs_a_working_recorder_once_sentry_is_ready() {
        let _guard = evm::telemetry::init_sentry(None);
        assert!(
            sentry_hub_has_client(),
            "init_sentry should have bound a client (disabled, since no DSN) to the process Hub"
        );

        init_metrics().expect(
            "init_metrics should succeed once init_sentry has bound a client to the process Hub",
        );

        counter!("test_init_metrics_end_to_end").increment(1);
        let output = render().expect("init_metrics should have stored a handle for render()");
        assert!(
            output.contains("test_init_metrics_end_to_end"),
            "the recorder init_metrics actually installs in production should render a value \
             recorded through it, not just the pieces exercised in isolation elsewhere in this \
             file: {output}"
        );
    }

    // `init_metrics_refuses_to_start_before_sentry_is_initialized` and
    // `init_metrics_installs_a_working_recorder_once_sentry_is_ready` pin the
    // ordering guard's logic in isolation - neither one drives the actual
    // call site the guard exists to protect. `server/src/bin/server.rs`
    // can't be driven in a unit test (it needs a live database, env config,
    // and a bound TCP listener before it gets anywhere near these two
    // calls), so this checks the real entry point the same cheap way
    // `server/src/api/invoices/tests/query.rs` checks handler wiring it
    // can't instantiate either: read the source and confirm the ordering
    // textually, rather than trusting the doc comment above `init_metrics`
    // that says to keep it.
    #[test]
    #[allow(clippy::expect_used)]
    fn server_main_calls_init_sentry_before_init_metrics() {
        let src = include_str!("bin/server.rs");
        let sentry_pos = src
            .find("evm::telemetry::init_sentry(")
            .expect("server.rs should still call evm::telemetry::init_sentry");
        let metrics_pos = src
            .find("metrics::init_metrics()")
            .expect("server.rs should still call metrics::init_metrics()");
        assert!(
            sentry_pos < metrics_pos,
            "server.rs must call init_sentry before init_metrics - init_metrics's runtime \
             guard depends on it, and a boot that got these backwards would fail closed at \
             startup instead of silently dropping every Sentry-forwarded metric, including \
             payserver_chain_healthy/payserver_chain_block_lag"
        );
    }

    // Exercises `FanoutRecorder` directly, without going through
    // `metrics::set_global_recorder` (a process-wide singleton other tests
    // in this module also touch). Proves the Prometheus side - the
    // `/metrics` escape hatch this ticket promises to keep - still gets
    // every value the facade records, even though every counter/gauge/
    // histogram handle now also carries a Sentry-forwarding wrapper.
    //
    // The Sentry-forwarding side of these same handles is covered by
    // `fanout_recorder_forwards_to_sentry_with_correct_types` below.
    #[test]
    fn fanout_recorder_still_updates_prometheus() {
        let prometheus = PrometheusBuilder::new().build_recorder();
        let recorder = FanoutRecorder { prometheus };
        let metadata = Metadata::new(module_path!(), metrics::Level::INFO, None);

        let counter_key = Key::from_parts("test_fanout_counter", vec![]);
        recorder
            .register_counter(&counter_key, &metadata)
            .increment(1);

        let gauge_key = Key::from_parts(
            "test_fanout_gauge",
            vec![metrics::Label::new("chain_id", "1")],
        );
        recorder.register_gauge(&gauge_key, &metadata).set(3.0);

        let histogram_key = Key::from_parts("test_fanout_histogram", vec![]);
        recorder
            .register_histogram(&histogram_key, &metadata)
            .record(0.5);

        let output = recorder.prometheus.handle().render();
        assert!(output.contains("test_fanout_counter 1"));
        assert!(output.contains("test_fanout_gauge{chain_id=\"1\"} 3"));
        assert!(output.contains("test_fanout_histogram"));
    }

    // The three verbs below (`absolute`, gauge `increment`/`decrement`) are
    // guarded by a `debug_assert!` precisely because no call site uses them
    // today - a guard nothing exercises is a guard nobody knows still works.
    // These pin that it actually fires in a debug/test build (the build
    // nextest runs in CI) if that ever stops being true, rather than the
    // no-call-site claim quietly going stale.
    #[test]
    #[should_panic(expected = "not forwarded to Sentry")]
    fn counter_absolute_panics_in_debug_builds() {
        let recorder = FanoutRecorder {
            prometheus: PrometheusBuilder::new().build_recorder(),
        };
        let metadata = Metadata::new(module_path!(), metrics::Level::INFO, None);
        let key = Key::from_parts("test_absolute_guard", vec![]);
        recorder.register_counter(&key, &metadata).absolute(5);
    }

    #[test]
    #[should_panic(expected = "not forwarded to Sentry")]
    fn gauge_increment_panics_in_debug_builds() {
        let recorder = FanoutRecorder {
            prometheus: PrometheusBuilder::new().build_recorder(),
        };
        let metadata = Metadata::new(module_path!(), metrics::Level::INFO, None);
        let key = Key::from_parts("test_increment_guard", vec![]);
        recorder.register_gauge(&key, &metadata).increment(1.0);
    }

    #[test]
    #[should_panic(expected = "not forwarded to Sentry")]
    fn gauge_decrement_panics_in_debug_builds() {
        let recorder = FanoutRecorder {
            prometheus: PrometheusBuilder::new().build_recorder(),
        };
        let metadata = Metadata::new(module_path!(), metrics::Level::INFO, None);
        let key = Key::from_parts("test_decrement_guard", vec![]);
        recorder.register_gauge(&key, &metadata).decrement(1.0);
    }

    // Proves the half of `FanoutRecorder` the test above can't: that a
    // counter, a gauge, and a histogram each reach Sentry with the right
    // `MetricType` and the label attached as an attribute -
    // `payserver_chain_healthy`/`payserver_chain_block_lag` are gauges with a
    // `chain_id` label, and an alert rule needs both the type and the
    // attribute to work.
    //
    // `sentry::test::with_captured_envelopes` binds a fresh, isolated `Hub`
    // with a `TestTransport` for the duration of the closure - it never
    // touches the process-global `Hub`, so unlike a real `sentry::init` it
    // doesn't race with `set_global_recorder`-based tests elsewhere in this
    // module or in this file's other tests.
    #[test]
    #[allow(clippy::expect_used)]
    fn fanout_recorder_forwards_to_sentry_with_correct_types() {
        let recorder = FanoutRecorder {
            prometheus: PrometheusBuilder::new().build_recorder(),
        };
        let metadata = Metadata::new(module_path!(), metrics::Level::INFO, None);

        let envelopes = sentry::test::with_captured_envelopes(|| {
            let counter_key = Key::from_parts(
                "test_sentry_counter",
                vec![metrics::Label::new("chain_id", "1")],
            );
            recorder
                .register_counter(&counter_key, &metadata)
                .increment(2);

            let gauge_key = Key::from_parts(
                "test_sentry_gauge",
                vec![metrics::Label::new("chain_id", "1")],
            );
            recorder.register_gauge(&gauge_key, &metadata).set(7.0);

            let histogram_key = Key::from_parts("test_sentry_histogram", vec![]);
            recorder
                .register_histogram(&histogram_key, &metadata)
                .record(0.25);

            // Sentry batches metrics (flushed every 100 items or 5 seconds)
            // rather than sending them immediately - without this, the
            // closure would return and the test hub would be torn down
            // before anything reached the transport.
            if let Some(client) = sentry::Hub::current().client() {
                client.flush(Some(Duration::from_secs(5)));
            }
        });

        let metrics: Vec<&sentry::protocol::Metric> = envelopes
            .iter()
            .flat_map(sentry::Envelope::items)
            .filter_map(|item| match item {
                sentry::protocol::EnvelopeItem::ItemContainer(
                    sentry::protocol::ItemContainer::Metrics(metrics),
                ) => Some(metrics.as_slice()),
                _ => None,
            })
            .flatten()
            .collect();

        let counter = metrics
            .iter()
            .find(|m| m.name.as_ref() == "test_sentry_counter")
            .expect("counter metric was not captured by Sentry");
        assert_eq!(counter.r#type, sentry::protocol::MetricType::Counter);
        assert_eq!(counter.value, 2.0);
        assert_eq!(
            counter
                .attributes
                .get("chain_id")
                .and_then(|a| a.0.as_str()),
            Some("1")
        );

        let gauge = metrics
            .iter()
            .find(|m| m.name.as_ref() == "test_sentry_gauge")
            .expect("gauge metric was not captured by Sentry");
        assert_eq!(gauge.r#type, sentry::protocol::MetricType::Gauge);
        assert_eq!(gauge.value, 7.0);
        assert_eq!(
            gauge.attributes.get("chain_id").and_then(|a| a.0.as_str()),
            Some("1")
        );

        let histogram = metrics
            .iter()
            .find(|m| m.name.as_ref() == "test_sentry_histogram")
            .expect("histogram metric was not captured by Sentry");
        assert_eq!(histogram.r#type, sentry::protocol::MetricType::Distribution);
        assert_eq!(histogram.value, 0.25);
    }

    // The test above proves the wire format is right for arbitrary keys; this
    // one proves it for the two gauges the ticket is actually about, reached
    // the way production reaches them - through `set_chain_blocks` and
    // `set_chain_healthy`, not a hand-built `Key`. If either function ever
    // stopped using a plain `.set()`, or the recorder mapped the wrong
    // `MetricType`, this goes red; nothing else in the suite would notice,
    // since `payserver_chain_healthy`/`payserver_chain_block_lag` silently
    // going stale in Sentry is exactly the "monitor falls behind and nothing
    // says so" failure this migration exists to close.
    //
    // `metrics::with_local_recorder` scopes the recorder to this thread for
    // the duration of the closure - unlike `metrics::set_global_recorder`,
    // it's a thread-local, not a process-wide singleton other tests would
    // race against, so the real `set_chain_blocks`/`set_chain_healthy` call
    // sites can be exercised here without installing anything globally.
    #[test]
    #[allow(clippy::expect_used)]
    fn fanout_recorder_forwards_real_chain_health_gauges_to_sentry() {
        let recorder = FanoutRecorder {
            prometheus: PrometheusBuilder::new().build_recorder(),
        };

        let envelopes = sentry::test::with_captured_envelopes(|| {
            metrics::with_local_recorder(&recorder, || {
                // Chain head advances to 103, the monitor's last processed
                // block stays at 100: the monitor has silently fallen behind.
                set_chain_blocks(1, Some(103), Some(100));
                set_chain_healthy(1, false);
            });

            // Sentry batches metrics rather than sending them immediately -
            // without this, the closure would return and the test hub would
            // be torn down before anything reached the transport.
            if let Some(client) = sentry::Hub::current().client() {
                client.flush(Some(Duration::from_secs(5)));
            }
        });

        let metrics: Vec<&sentry::protocol::Metric> = envelopes
            .iter()
            .flat_map(sentry::Envelope::items)
            .filter_map(|item| match item {
                sentry::protocol::EnvelopeItem::ItemContainer(
                    sentry::protocol::ItemContainer::Metrics(metrics),
                ) => Some(metrics.as_slice()),
                _ => None,
            })
            .flatten()
            .collect();

        let healthy = metrics
            .iter()
            .find(|m| m.name.as_ref() == "payserver_chain_healthy")
            .expect("payserver_chain_healthy was not captured by Sentry");
        assert_eq!(healthy.r#type, sentry::protocol::MetricType::Gauge);
        assert_eq!(healthy.value, 0.0);
        assert_eq!(
            healthy
                .attributes
                .get("chain_id")
                .and_then(|a| a.0.as_str()),
            Some("1")
        );

        let lag = metrics
            .iter()
            .find(|m| m.name.as_ref() == "payserver_chain_block_lag")
            .expect("payserver_chain_block_lag was not captured by Sentry");
        assert_eq!(lag.r#type, sentry::protocol::MetricType::Gauge);
        assert_eq!(lag.value, 3.0);
        assert_eq!(
            lag.attributes.get("chain_id").and_then(|a| a.0.as_str()),
            Some("1")
        );
    }

    // The error path scrubs secrets/PII via `scrub_event` before an event
    // leaves the process; this is the equivalent check for the metrics path,
    // which is a separate envelope `scrub_event` never sees. No call site
    // labels a metric with an address today, but nothing stops one from
    // starting to - this proves that if it did, the address wouldn't reach
    // Sentry raw.
    #[test]
    #[allow(clippy::expect_used)]
    fn sentry_labels_scrubs_secret_shaped_values() {
        let recorder = FanoutRecorder {
            prometheus: PrometheusBuilder::new().build_recorder(),
        };
        let metadata = Metadata::new(module_path!(), metrics::Level::INFO, None);

        let envelopes = sentry::test::with_captured_envelopes(|| {
            let key = Key::from_parts(
                "test_scrub_gauge",
                vec![metrics::Label::new(
                    "address",
                    "0x71C7656EC7ab88b098defB751B7401B5f6d8976F",
                )],
            );
            recorder.register_gauge(&key, &metadata).set(1.0);

            if let Some(client) = sentry::Hub::current().client() {
                client.flush(Some(Duration::from_secs(5)));
            }
        });

        let metric = envelopes
            .iter()
            .flat_map(sentry::Envelope::items)
            .filter_map(|item| match item {
                sentry::protocol::EnvelopeItem::ItemContainer(
                    sentry::protocol::ItemContainer::Metrics(metrics),
                ) => Some(metrics.as_slice()),
                _ => None,
            })
            .flatten()
            .find(|m| m.name.as_ref() == "test_scrub_gauge")
            .expect("gauge metric was not captured by Sentry");

        let address = metric
            .attributes
            .get("address")
            .and_then(|a| a.0.as_str())
            .expect("address attribute missing");
        assert_eq!(address, "[redacted-hex]");
    }

    #[test]
    fn test_db_pool_connections_gauge() {
        // This test verifies the gauge recording function doesn't panic
        // and writes the expected metric names.
        set_db_pool_connections("idle", 3);
        set_db_pool_connections("used", 7);
    }

    // Integration-style test that verifies the metrics render output.
    // Only runs when the recorder is available (e.g., not in parallel with
    // other recorder-installing tests). Gated behind an env var so it
    // doesn't conflict with other tests that install a global recorder.
    #[test]
    fn test_new_metrics_render() {
        if std::env::var("METRICS_INTEGRATION_TEST").is_err() {
            return; // skip unless explicitly enabled
        }
        let handle = test_recorder();

        // Record DB pool metrics
        set_db_pool_connections("idle", 5);
        set_db_pool_connections("used", 10);

        // Record via the histogram directly (simulating data-service)
        histogram!("ethpayserver_db_pool_wait_duration_seconds").record(0.003);
        histogram!("ethpayserver_db_pool_wait_duration_seconds").record(0.012);

        // Record RPC metrics (simulating evm crate)
        histogram!(
            "ethpayserver_rpc_request_duration_seconds",
            "chain_id" => "11155111",
            "method" => "get_block_number"
        )
        .record(0.045);

        counter!(
            "ethpayserver_rpc_requests_total",
            "chain_id" => "11155111",
            "method" => "get_block_number",
            "status" => "ok"
        )
        .increment(1);

        counter!(
            "ethpayserver_rpc_errors_total",
            "chain_id" => "11155111",
            "method" => "get_logs",
            "error_kind" => "timeout"
        )
        .increment(1);

        let output = handle.render();
        assert!(
            output.contains("ethpayserver_db_pool_connections"),
            "missing db_pool_connections"
        );
        assert!(
            output.contains("ethpayserver_db_pool_wait_duration_seconds"),
            "missing db_pool_wait_duration"
        );
        assert!(
            output.contains("ethpayserver_rpc_request_duration_seconds"),
            "missing rpc_request_duration"
        );
        assert!(
            output.contains("ethpayserver_rpc_requests_total"),
            "missing rpc_requests_total"
        );
        assert!(
            output.contains("ethpayserver_rpc_errors_total"),
            "missing rpc_errors_total"
        );

        // Reproduces the ticket's "be seen to fail" check: a chain that is
        // caught up and healthy, then one whose monitor has stalled while
        // the chain head keeps moving, must show the lag climbing and
        // health flipping to unhealthy - not a gauge stuck at its first
        // value.
        set_chain_blocks(1, Some(100), Some(100));
        set_chain_healthy(1, true);
        let output = handle.render();
        assert!(output.contains("payserver_chain_block_lag{chain_id=\"1\"} 0"));
        assert!(output.contains("payserver_chain_healthy{chain_id=\"1\"} 1"));

        // Monitor stalls: chain head advances, last_processed_block does not.
        set_chain_blocks(1, Some(103), Some(100));
        set_chain_healthy(1, false);
        let output = handle.render();
        assert!(
            output.contains("payserver_chain_block_lag{chain_id=\"1\"} 3"),
            "lag did not climb when the monitor fell behind: {output}"
        );
        assert!(
            output.contains("payserver_chain_healthy{chain_id=\"1\"} 0"),
            "healthy gauge did not flip to 0 when the chain fell behind: {output}"
        );
    }

    #[test]
    fn test_chain_gauges_do_not_panic() {
        // Mirrors test_db_pool_connections_gauge: confirms the recording
        // functions run cleanly, including the "source not connected yet"
        // case where both block numbers are `None`.
        set_chain_blocks(1, Some(100), Some(95));
        set_chain_blocks(1, None, None);
        set_chain_healthy(1, true);
        set_chain_healthy(1, false);
    }

    // Reached through `with_local_recorder` rather than the global handle,
    // same reasoning as `fanout_recorder_forwards_real_chain_health_gauges`:
    // a thread-scoped recorder so this doesn't race other tests in this
    // file over the process-wide singleton `test_recorder()` installs.
    //
    // Breaks the exact guarantee the gauge exists for - the idempotency
    // control going unhealthy - and confirms the gauge actually moves,
    // rather than trusting it from reading `set_idempotency_healthy`'s body.
    #[test]
    fn idempotency_healthy_gauge_moves_with_redis_outcome() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            set_idempotency_healthy(true);
        });
        assert!(
            handle
                .render()
                .contains("ethpayserver_idempotency_healthy 1"),
            "gauge did not read healthy after a successful Redis op"
        );

        metrics::with_local_recorder(&recorder, || {
            set_idempotency_healthy(false);
        });
        assert!(
            handle
                .render()
                .contains("ethpayserver_idempotency_healthy 0"),
            "gauge did not flip to unhealthy when the Redis op failed"
        );
    }
}
