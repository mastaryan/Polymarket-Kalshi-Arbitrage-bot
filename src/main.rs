//! Prediction Market Arbitrage Trading System
//!
//! A high-performance, production-ready arbitrage trading system for cross-platform
//! prediction markets. This system monitors price discrepancies between Kalshi and
//! Polymarket
//!
//! NOTE: This file includes a KALSHI_ONLY mode that disables all Polymarket logic
//! so the app can run in Kalshi-only environments.

mod cache;
mod circuit_breaker;
mod config;
mod discovery;
mod execution;
mod kalshi;
mod polymarket;
mod polymarket_clob;
mod position_tracker;
mod types;

use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use cache::TeamCache;
use circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use config::{ARB_THRESHOLD, ENABLED_LEAGUES, WS_RECONNECT_DELAY_SECS};
use discovery::DiscoveryClient;
use execution::{create_execution_channel, run_execution_loop, ExecutionEngine};
use kalshi::{KalshiApiClient, KalshiConfig};
use polymarket_clob::{PolymarketAsyncClient, PreparedCreds, SharedAsyncClient};
use position_tracker::{create_position_channel, position_writer_loop, PositionTracker};
use types::{GlobalState, PriceCents};

/// Polymarket CLOB API host
const POLY_CLOB_HOST: &str = "https://clob.polymarket.com";
/// Polygon chain ID
const POLYGON_CHAIN_ID: u64 = 137;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("arb_bot=info".parse().unwrap()),
        )
        .init();

    info!("🚀 Prediction Market Arbitrage System v2.0");
    info!(
        "   Profit threshold: <{:.1}¢ ({:.1}% minimum profit)",
        ARB_THRESHOLD * 100.0,
        (1.0 - ARB_THRESHOLD) * 100.0
    );
    info!("   Monitored leagues: {:?}", ENABLED_LEAGUES);

    // Load .env early
    dotenvy::dotenv().ok();

    // Check for dry run mode
    let dry_run = std::env::var("DRY_RUN")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(true);

    if dry_run {
        info!("   Mode: DRY RUN (set DRY_RUN=0 to execute)");
    } else {
        warn!("   Mode: LIVE EXECUTION");
    }

    // Kalshi-only mode toggle
    let kalshi_only = std::env::var("KALSHI_ONLY")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    if kalshi_only {
        warn!("   Mode: KALSHI ONLY (Polymarket disabled)");
    }

    // Load Kalshi credentials
    let kalshi_config = KalshiConfig::from_env()?;
    info!("[KALSHI] API key loaded");

    // Load team code mapping cache
    let team_cache = TeamCache::load();
    info!("📂 Loaded {} team code mappings", team_cache.len());

    // Create Kalshi API client
    let kalshi_api = Arc::new(KalshiApiClient::new(kalshi_config));

    // Run discovery (with caching support)
    let force_discovery = std::env::var("FORCE_DISCOVERY")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    info!(
        "🔍 Market discovery{}...",
        if force_discovery { " (forced refresh)" } else { "" }
    );

    let discovery = DiscoveryClient::new(
        KalshiApiClient::new(KalshiConfig::from_env()?),
        team_cache,
    );

    let result = if kalshi_only {
        discovery.discover_kalshi_only(ENABLED_LEAGUES).await
    } else if force_discovery {
        discovery.discover_all_force(ENABLED_LEAGUES).await
    } else {
        discovery.discover_all(ENABLED_LEAGUES).await
    };

    info!("📊 Market discovery complete:");
    info!("   - Matched market pairs: {}", result.pairs.len());

    if !result.errors.is_empty() {
        for err in &result.errors {
            warn!("   ⚠️ {}", err);
        }
    }

    if result.pairs.is_empty() {
        error!("No market pairs found!");
        return Ok(());
    }

    // Display discovered market pairs
    info!("📋 Discovered market pairs:");
    for pair in &result.pairs {
        info!(
            "   ✅ {} | {} | Kalshi: {}",
            pair.description, pair.market_type, pair.kalshi_market_ticker
        );
    }

    // Build global state
    let state = Arc::new({
        let mut s = GlobalState::new();
        for pair in result.pairs {
            s.add_pair(pair);
        }
        info!(
            "📡 Global state initialized: tracking {} markets",
            s.market_count()
        );
        s
    });

    // Threshold
    let threshold_cents: PriceCents = ((ARB_THRESHOLD * 100.0).round() as u16).max(1);
    info!("   Execution threshold: {} cents", threshold_cents);

    // Create execution channel (Kalshi WS expects a sender)
    let (exec_tx, exec_rx) = create_execution_channel();

    // Prepare Kalshi WS config reused on reconnects
    let kalshi_ws_config = KalshiConfig::from_env()?;

    // ============================
    // KALSHI-ONLY MODE (RETURN EARLY)
    // ============================
    if kalshi_only {
        // Initialize execution infrastructure (Kalshi-only)
        let circuit_breaker = Arc::new(CircuitBreaker::new(CircuitBreakerConfig::from_env()));

        let position_tracker = Arc::new(RwLock::new(PositionTracker::new()));
        let (position_channel, position_rx) = create_position_channel();
        tokio::spawn(position_writer_loop(position_rx, position_tracker));

        let engine = Arc::new(ExecutionEngine::new(
            kalshi_api.clone(),
            None,
            state.clone(),
            circuit_breaker.clone(),
            position_channel,
            dry_run,
        ));

        let exec_handle = tokio::spawn(run_execution_loop(exec_rx, engine));

        // Start Kalshi WebSocket connection
        let kalshi_state = state.clone();
        let kalshi_exec_tx = exec_tx.clone();
        let kalshi_threshold = threshold_cents;

        let kalshi_handle = tokio::spawn(async move {
            loop {
                if let Err(e) = kalshi::run_ws(
                    &kalshi_ws_config,
                    kalshi_state.clone(),
                    kalshi_exec_tx.clone(),
                    kalshi_threshold,
                )
                .await
                {
                    error!("[KALSHI] WebSocket disconnected: {} - reconnecting...", e);
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(WS_RECONNECT_DELAY_SECS)).await;
            }
        });

        // Kalshi-only heartbeat + opportunity scan
        let heartbeat_state = state.clone();
        let heartbeat_handle = tokio::spawn(async move {
            use crate::types::kalshi_fee_cents;
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
            loop {
                interval.tick().await;

                let market_count = heartbeat_state.market_count();
                let mut with_kalshi_any = 0;
                let mut with_kalshi_both = 0;
                let mut best_arb: Option<(u16, u16, u16, u16, i16)> = None;

                for market in heartbeat_state.markets.iter().take(market_count) {
                    let (k_yes, k_no, _, _) = market.kalshi.load();
                    if k_yes > 0 || k_no > 0 {
                        with_kalshi_any += 1;
                    }
                    if k_yes > 0 && k_no > 0 {
                        with_kalshi_both += 1;
                        let fee = kalshi_fee_cents(k_yes) + kalshi_fee_cents(k_no);
                        let cost = k_yes + k_no + fee;
                        if best_arb.is_none() || cost < best_arb.as_ref().unwrap().0 {
                            let profit = 100i16 - cost as i16;
                            best_arb = Some((cost, market.market_id, k_yes, k_no, fee as u16, profit));
                        }
                    }
                }

                info!(
                    "💓 Kalshi-only heartbeat | Markets: {} total, {} with any Kalshi prices, {} with BOTH (yes/no)",
                    market_count, with_kalshi_any, with_kalshi_both
                );

                if let Some((cost, market_id, k_yes, k_no, fee, profit)) = best_arb {
                    let desc = heartbeat_state
                        .get_by_id(market_id)
                        .and_then(|m| m.pair.as_ref())
                        .map(|p| &*p.description)
                        .unwrap_or("Unknown");
                    let gap = cost as i16 - threshold_cents as i16;
                    let breakdown = format!("K_yes({}›) + K_no({}›) + K_fee({}›) = {}›", k_yes, k_no, fee, cost);
                    if gap <= 10 {
                        info!(
                            "   ?? Best Kalshi arb: {} | {} | gap={:+}› | profit={}›/contract",
                            desc, breakdown, gap, profit
                        );
                    } else {
                        info!(
                            "   ?? Best Kalshi arb: {} | {} | gap={:+}› (market efficient)",
                            desc, breakdown, gap
                        );
                    }
                }
            }
        });

        info!("✅ Kalshi-only mode active - running Kalshi WS + execution + heartbeat");
        let _ = tokio::join!(kalshi_handle, heartbeat_handle, exec_handle);
        return Ok(());
    }

    // ============================
    // FULL MODE (Kalshi + Polymarket)
    // ============================

    // Load Polymarket credentials
    let poly_private_key =
        std::env::var("POLY_PRIVATE_KEY").context("POLY_PRIVATE_KEY not set")?;
    let poly_funder = std::env::var("POLY_FUNDER")
        .context("POLY_FUNDER not set (your wallet address)")?;

    // Create async Polymarket client and derive API credentials
    info!("[POLYMARKET] Creating async client and deriving API credentials...");
    let poly_async_client = PolymarketAsyncClient::new(
        POLY_CLOB_HOST,
        POLYGON_CHAIN_ID,
        &poly_private_key,
        &poly_funder,
    )?;
    let api_creds = poly_async_client.derive_api_key(0).await?;
    let prepared_creds = PreparedCreds::from_api_creds(&api_creds)?;
    let poly_async =
        Arc::new(SharedAsyncClient::new(poly_async_client, prepared_creds, POLYGON_CHAIN_ID));

    // Load neg_risk cache from Python script output
    match poly_async.load_cache(".clob_market_cache.json") {
        Ok(count) => info!("[POLYMARKET] Loaded {} neg_risk entries from cache", count),
        Err(e) => warn!("[POLYMARKET] Could not load neg_risk cache: {}", e),
    }

    info!("[POLYMARKET] Client ready for {}", &poly_funder[..10]);

    // Start Kalshi WebSocket connection (full mode)
    let kalshi_state = state.clone();
    let kalshi_exec_tx = exec_tx.clone();
    let kalshi_threshold = threshold_cents;
    let kalshi_handle = tokio::spawn(async move {
        loop {
            if let Err(e) = kalshi::run_ws(
                &kalshi_ws_config,
                kalshi_state.clone(),
                kalshi_exec_tx.clone(),
                kalshi_threshold,
            )
            .await
            {
                error!("[KALSHI] WebSocket disconnected: {} - reconnecting...", e);
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(WS_RECONNECT_DELAY_SECS)).await;
        }
    });

    // Initialize execution infrastructure
    let circuit_breaker = Arc::new(CircuitBreaker::new(CircuitBreakerConfig::from_env()));

    let position_tracker = Arc::new(RwLock::new(PositionTracker::new()));
    let (position_channel, position_rx) = create_position_channel();
    tokio::spawn(position_writer_loop(position_rx, position_tracker));

    let engine = Arc::new(ExecutionEngine::new(
        kalshi_api.clone(),
        Some(poly_async),
        state.clone(),
        circuit_breaker.clone(),
        position_channel,
        dry_run,
    ));

    let exec_handle = tokio::spawn(run_execution_loop(exec_rx, engine));

    // Initialize Polymarket WebSocket connection
    let poly_state = state.clone();
    let poly_exec_tx = exec_tx.clone();
    let poly_threshold = threshold_cents;
    let poly_handle = tokio::spawn(async move {
        loop {
            if let Err(e) = polymarket::run_ws(poly_state.clone(), poly_exec_tx.clone(), poly_threshold).await {
                error!("[POLYMARKET] WebSocket disconnected: {} - reconnecting...", e);
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(WS_RECONNECT_DELAY_SECS)).await;
        }
    });

    // System health monitoring and arbitrage diagnostics
    let heartbeat_state = state.clone();
    let heartbeat_threshold = threshold_cents;
    let heartbeat_handle = tokio::spawn(async move {
        use crate::types::kalshi_fee_cents;
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            let market_count = heartbeat_state.market_count();
            let mut with_kalshi = 0;
            let mut with_poly = 0;
            let mut with_both = 0;
            let mut best_arb: Option<(u16, u16, u16, u16, u16, u16, u16, bool)> = None;

            for market in heartbeat_state.markets.iter().take(market_count) {
                let (k_yes, k_no, _, _) = market.kalshi.load();
                let (p_yes, p_no, _, _) = market.poly.load();
                let has_k = k_yes > 0 && k_no > 0;
                let has_p = p_yes > 0 && p_no > 0;
                if k_yes > 0 || k_no > 0 {
                    with_kalshi += 1;
                }
                if p_yes > 0 || p_no > 0 {
                    with_poly += 1;
                }
                if has_k && has_p {
                    with_both += 1;

                    let fee1 = kalshi_fee_cents(k_no);
                    let cost1 = p_yes + k_no + fee1;

                    let fee2 = kalshi_fee_cents(k_yes);
                    let cost2 = k_yes + fee2 + p_no;

                    let (best_cost, best_fee, is_poly_yes) = if cost1 <= cost2 {
                        (cost1, fee1, true)
                    } else {
                        (cost2, fee2, false)
                    };

                    if best_arb.is_none() || best_cost < best_arb.as_ref().unwrap().0 {
                        best_arb = Some((best_cost, market.market_id, p_yes, k_no, k_yes, p_no, best_fee, is_poly_yes));
                    }
                }
            }

            info!(
                "💓 System heartbeat | Markets: {} total, {} with Kalshi prices, {} with Polymarket prices, {} with both | threshold={}¢",
                market_count, with_kalshi, with_poly, with_both, heartbeat_threshold
            );

            if let Some((cost, market_id, p_yes, k_no, k_yes, p_no, fee, is_poly_yes)) = best_arb {
                let gap = cost as i16 - heartbeat_threshold as i16;
                let desc = heartbeat_state
                    .get_by_id(market_id)
                    .and_then(|m| m.pair.as_ref())
                    .map(|p| &*p.description)
                    .unwrap_or("Unknown");
                let leg_breakdown = if is_poly_yes {
                    format!("P_yes({}¢) + K_no({}¢) + K_fee({}¢) = {}¢", p_yes, k_no, fee, cost)
                } else {
                    format!("K_yes({}¢) + P_no({}¢) + K_fee({}¢) = {}¢", k_yes, p_no, fee, cost)
                };
                if gap <= 10 {
                    info!(
                        "   📊 Best opportunity: {} | {} | gap={:+}¢ | [Poly_yes={}¢ Kalshi_no={}¢ Kalshi_yes={}¢ Poly_no={}¢]",
                        desc, leg_breakdown, gap, p_yes, k_no, k_yes, p_no
                    );
                } else {
                    info!(
                        "   📊 Best opportunity: {} | {} | gap={:+}¢ (market efficient)",
                        desc, leg_breakdown, gap
                    );
                }
            } else if with_both == 0 {
                warn!("   ⚠️  No markets with both Kalshi and Polymarket prices - verify WebSocket connections");
            }
        }
    });

    // Main event loop - run until termination
    info!("✅ All systems operational - entering main event loop");
    let _ = tokio::join!(kalshi_handle, poly_handle, heartbeat_handle, exec_handle);

    Ok(())
}

