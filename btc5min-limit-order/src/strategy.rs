use crate::api::PolymarketApi;
use crate::config::Config;
use crate::discovery::MarketDiscovery;
use crate::models::*;
use crate::clob_client::ClobClient;
use anyhow::Result;
use chrono::Utc;
use chrono_tz::America::New_York;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use tokio::signal;
use tokio_util::sync::CancellationToken;
use log::{warn, info, error, debug};
use futures::StreamExt;
use polymarket_client_sdk::clob::ws::types::response::TradeMessageStatus;

const MARKET_DURATION_SECS: i64 = 300;
const MARKET_DURATION_SECS_U64: u64 = 300;
const BUY_PRICE: f64 = 0.49;
const SELL_PRICE: f64 = 0.50;



pub struct PreLimitStrategy {
    api: Arc<PolymarketApi>,
    config: Config,
    discovery: MarketDiscovery,
    state: Arc<Mutex<Option<PreLimitOrderState>>>,
    total_profit: Arc<Mutex<f64>>,
    trades: Arc<Mutex<HashMap<String, CycleTrade>>>,
    closure_checked: Arc<Mutex<HashMap<String, bool>>>,
    clob_client: Option<Arc<ClobClient>>,
}

#[derive(Debug, Clone)]
struct CycleTrade {
    condition_id: String,
    period_timestamp: u64,
    market_duration_secs: u64,
    up_token_id: Option<String>,
    down_token_id: Option<String>,
    up_shares: f64,
    down_shares: f64,
    up_avg_price: f64,
    down_avg_price: f64,
}

impl PreLimitStrategy {
    pub async fn new(api: Arc<PolymarketApi>, config: Config) -> Self {
        let discovery = MarketDiscovery::new(api.clone());
        
        // Initialize CLOB client if private key is available
        info!("Checking if private key is available in config...");
        let clob_client = if config.polymarket.private_key.is_some() {
            info!("Private key found in config, attempting to create CLOB client");
            match ClobClient::new(&config).await {
                Ok(client) => {
                    info!("Successfully created CLOB client");
                    Some(Arc::new(client))
                }
                Err(e) => {
                    warn!("Failed to create CLOB client: {}", e);
                    None
                }
            }
        } else {
            info!("No private key found in config, skipping CLOB client initialization");
            None
        };
        
        Self {
            api,
            config,
            discovery,
            state: Arc::new(Mutex::new(None)),
            total_profit: Arc::new(Mutex::new(0.0)),
            trades: Arc::new(Mutex::new(HashMap::new())),
            closure_checked: Arc::new(Mutex::new(HashMap::new())),
            clob_client,
        }
    }

    pub async fn get_total_profit(&self) -> f64 {
        *self.total_profit.lock().await
    }

    pub async fn run(&self) -> Result<()> {
        // Create cancellation token for graceful shutdown
        let shutdown_token = CancellationToken::new();
        let shutdown_token_clone = shutdown_token.clone();
        
        // Spawn Ctrl+C handler
        tokio::spawn(async move {
            match signal::ctrl_c().await {
                Ok(()) => {
                    info!("Received Ctrl+C, initiating graceful shutdown...");
                    shutdown_token_clone.cancel();
                }
                Err(e) => {
                    error!("Failed to listen for Ctrl+C: {}", e);
                }
            }
        });
        
        // Initialize CLOB subscription if client is available
        if let Some(clob_client) = &self.clob_client {
            info!("Starting CLOB order and trade status subscription (event-driven SELL placement)");

            let clob_client_clone = clob_client.clone();
            let state_clone = self.state.clone();
            let api_clone = self.api.clone();
            let config_clone = self.config.clone();
            let shutdown_token_clone = shutdown_token.clone();

            // Spawn task to handle CLOB subscription directly with shutdown support
            // This now handles both Order and Trade events for event-driven SELL placement
            tokio::spawn(async move {
                if let Err(e) = Self::handle_clob_subscription_direct(
                    clob_client_clone,
                    state_clone,
                    api_clone,
                    config_clone,
                    shutdown_token_clone
                ).await {
                    error!("CLOB subscription error: {}", e);
                }
            });
        } else {
            warn!("CLOB client not available - order status updates will be disabled. Please check your config.json file and ensure private_key is set.");
        }
        
        // Main strategy loop with shutdown support
        info!("Starting main strategy loop");
        loop {
            // Check for shutdown signal
            if shutdown_token.is_cancelled() {
                info!("Shutdown signal received, stopping strategy loop");
                break;
            }
            
            if let Err(e) = self.process_market().await {
                error!("Error processing market: {}", e);
            }
            
            // Sleep with shutdown support
            tokio::select! {
                _ = sleep(Duration::from_millis(self.config.strategy.check_interval_ms)) => {}
                _ = shutdown_token.cancelled() => {
                    info!("Shutdown signal received during sleep, stopping strategy loop");
                    break;
                }
            }
        }
        
        info!("Strategy shutdown complete");
        Ok(())
    }

    async fn process_market(&self) -> Result<()> {
        let current_period_et = Self::get_current_5m_period_et();
        let current_time_et = Self::get_current_time_et();
        let next_period_start = current_period_et + MARKET_DURATION_SECS;
        
        let mut state_guard = self.state.lock().await;
        let state = state_guard.clone();
        
        match state {
            Some(mut s) => {
                if current_time_et > s.expiry {
                    // Only register for redemption if trade was actually mined (tokens received)
                    if s.up_mined || s.down_mined {
                        let trade = Self::cycle_trade_from_state(&s, self.config.strategy.shares);
                        let mut t = self.trades.lock().await;
                        t.insert(s.condition_id.clone(), trade);
                        info!("Market expired. Registered position for redemption (condition: {}, up_mined: {}, down_mined: {})",
                            &s.condition_id[..s.condition_id.len().min(20)], s.up_mined, s.down_mined);
                    }
                    *state_guard = None;
                    return Ok(());
                }
                
                self.check_buy_order_matches(&mut s).await?;
                
                // Event-driven SELL placement is now handled in handle_clob_subscription_direct()
                // when TradeMessageStatus::Mined is received. This block serves as a fallback
                // in case the WebSocket event was missed.
                if (s.up_mined || s.down_mined) && s.up_sell_order_id.is_none() && s.down_sell_order_id.is_none() {
                    if s.up_mined && s.up_sell_order_id.is_none() {
                        info!("Fallback: Up trade mined but SELL not placed. Placing now...");
                        let sell_order = self.place_limit_order(&s.up_token_id, "SELL", SELL_PRICE).await?;
                        s.up_sell_order_id = Some(sell_order.order_id.unwrap_or_default());
                        info!("Up buy mined. Placed SELL order at ${:.2}", SELL_PRICE);
                    }
                    if s.down_mined && s.down_sell_order_id.is_none() {
                        info!("Fallback: Down trade mined but SELL not placed. Placing now...");
                        let sell_order = self.place_limit_order(&s.down_token_id, "SELL", SELL_PRICE).await?;
                        s.down_sell_order_id = Some(sell_order.order_id.unwrap_or_default());
                        info!("Down buy mined. Placed SELL order at ${:.2}", SELL_PRICE);
                    }
                }
                
                *state_guard = Some(s);
            }
            None => {
                if let Some(market) = self.discover_market("BTC", current_period_et).await? {
                    if current_time_et >= current_period_et && current_time_et < next_period_start {
                        info!("5min period started. Placing BUY orders at ${:.2} for BTC", BUY_PRICE);
                        let (up_token_id, down_token_id) = self.discovery.get_market_tokens(&market.condition_id).await?;
                        
                        let up_order = self.place_limit_order(&up_token_id, "BUY", BUY_PRICE).await?;
                        let down_order = self.place_limit_order(&down_token_id, "BUY", BUY_PRICE).await?;
                        
                        let new_state = PreLimitOrderState {
                            asset: "BTC".to_string(),
                            condition_id: market.condition_id,
                            up_token_id: up_token_id.clone(),
                            down_token_id: down_token_id.clone(),
                            up_order_id: up_order.order_id,
                            down_order_id: down_order.order_id,
                            up_buy_price: BUY_PRICE,
                            down_buy_price: BUY_PRICE,
                            up_matched: false,
                            down_matched: false,
                            up_mined: false,
                            down_mined: false,
                            up_shares_received: 0.0,
                            down_shares_received: 0.0,
                            up_sell_order_id: None,
                            down_sell_order_id: None,
                            expiry: next_period_start,
                            order_placed_at: current_time_et,
                            market_period_start: current_period_et,
                        };
                        *state_guard = Some(new_state);
                    }
                }
            }
        }
        
        Ok(())
    }

    /// Handle CLOB subscription and send order updates to the strategy
    /// Now also handles Trade events for event-driven SELL order placement
    pub async fn handle_clob_subscription_direct(
        clob_client: Arc<ClobClient>,
        state: Arc<Mutex<Option<PreLimitOrderState>>>,
        api: Arc<PolymarketApi>,
        config: Config,
        shutdown_token: CancellationToken,
    ) -> Result<()> {
        // Subscribe to user events using empty market list for all events
        let markets = Vec::new();

        // Get the authenticated client
        let client = {
            let auth_client: tokio::sync::RwLockReadGuard<Option<polymarket_client_sdk::clob::ws::Client<polymarket_client_sdk::auth::state::Authenticated<polymarket_client_sdk::auth::Normal>>>> = clob_client.authenticated_client.read().await;
            auth_client.clone().ok_or_else(|| anyhow::anyhow!("No authenticated client available"))?
        };

        // Subscribe to user events
        let stream = client.subscribe_user_events(markets)?;
        let mut stream = std::pin::pin!(stream);

        info!("CLOB subscription started, listening for order and trade status updates");

        loop {
            tokio::select! {
                Some(event) = stream.next() => {
                    match event {
                        Ok(user_event) => {
                            match user_event {
                                polymarket_client_sdk::clob::ws::WsMessage::Order(order) => {
                                    info!("Received order update: {:?}", order);

                                    if let Some(order_status) = &order.status {
                                        match order_status {
                                            polymarket_client_sdk::clob::types::OrderStatusType::Matched => {
                                                if let (Some(size_matched), Some(outcome)) = (&order.size_matched, &order.outcome) {
                                                    let size: f64 = size_matched.to_string().parse::<f64>().unwrap_or(0.0);

                                                    // Update matched state (but NOT mined yet - don't place SELL)
                                                    let mut state_guard = state.lock().await;
                                                    if let Some(mut s) = state_guard.clone() {
                                                        if outcome.to_lowercase() == "up" {
                                                            s.up_matched = true;
                                                            info!("Order MATCHED: up_matched = true (size: {}). Waiting for MINED status before placing SELL.", size);
                                                        } else if outcome.to_lowercase() == "down" {
                                                            s.down_matched = true;
                                                            info!("Order MATCHED: down_matched = true (size: {}). Waiting for MINED status before placing SELL.", size);
                                                        }
                                                        *state_guard = Some(s);
                                                    }
                                                }
                                            }
                                            _ => {
                                                debug!("Order status: {:?}", order_status);
                                            }
                                        }
                                    }
                                }
                                polymarket_client_sdk::clob::ws::WsMessage::Trade(trade) => {
                                    info!("Received trade update: id={}, status={:?}, side={:?}, size={}, price={}",
                                        trade.id, trade.status, trade.side, trade.size, trade.price);

                                    // Handle trade status updates for event-driven SELL placement
                                    match trade.status {
                                        TradeMessageStatus::Mined | TradeMessageStatus::Confirmed => {
                                            // Trade is now on-chain - tokens are in our account!
                                            // This is the critical moment to place the SELL order

                                            let trade_size: f64 = trade.size.to_string().parse::<f64>().unwrap_or(0.0);
                                            let outcome = trade.outcome.as_deref().unwrap_or("");
                                            let is_buy = matches!(trade.side, polymarket_client_sdk::clob::types::Side::Buy);

                                            info!("Trade {} - Status: {:?}, Side: {:?}, Outcome: {}, Size: {}",
                                                trade.id, trade.status, trade.side, outcome, trade_size);

                                            // Only process BUY trades that were mined (we bought tokens)
                                            if is_buy && trade_size > 0.0 {
                                                let mut state_guard = state.lock().await;
                                                if let Some(mut s) = state_guard.clone() {
                                                    let should_place_sell = if outcome.to_lowercase() == "up" {
                                                        if !s.up_mined {
                                                            s.up_mined = true;
                                                            s.up_shares_received += trade_size;
                                                            info!("Trade MINED: up_mined = true, shares_received = {}", s.up_shares_received);
                                                            s.up_sell_order_id.is_none() // Place SELL if not already placed
                                                        } else {
                                                            // Additional fill for same side
                                                            s.up_shares_received += trade_size;
                                                            info!("Additional trade MINED for UP: total shares = {}", s.up_shares_received);
                                                            false
                                                        }
                                                    } else if outcome.to_lowercase() == "down" {
                                                        if !s.down_mined {
                                                            s.down_mined = true;
                                                            s.down_shares_received += trade_size;
                                                            info!("Trade MINED: down_mined = true, shares_received = {}", s.down_shares_received);
                                                            s.down_sell_order_id.is_none()
                                                        } else {
                                                            s.down_shares_received += trade_size;
                                                            info!("Additional trade MINED for DOWN: total shares = {}", s.down_shares_received);
                                                            false
                                                        }
                                                    } else {
                                                        false
                                                    };

                                                    // Immediately place SELL order - event-driven, no polling!
                                                    if should_place_sell && !config.strategy.simulation_mode {
                                                        let (token_id, side_name, shares) = if outcome.to_lowercase() == "up" {
                                                            (s.up_token_id.clone(), "UP", s.up_shares_received)
                                                        } else {
                                                            (s.down_token_id.clone(), "DOWN", s.down_shares_received)
                                                        };

                                                        info!("Placing SELL order immediately after MINED event for {} ({} shares)", side_name, shares);

                                                        // Place SELL order at $0.50
                                                        let order = OrderRequest {
                                                            token_id: token_id.clone(),
                                                            side: "SELL".to_string(),
                                                            size: shares.to_string(),
                                                            price: format!("{:.2}", SELL_PRICE),
                                                            order_type: "LIMIT".to_string(),
                                                        };

                                                        match api.place_order(&order).await {
                                                            Ok(sell_order) => {
                                                                let order_id = sell_order.order_id.unwrap_or_default();
                                                                if outcome.to_lowercase() == "up" {
                                                                    s.up_sell_order_id = Some(order_id.clone());
                                                                } else {
                                                                    s.down_sell_order_id = Some(order_id.clone());
                                                                }
                                                                info!("SELL order placed successfully for {}: order_id={}", side_name, order_id);
                                                            }
                                                            Err(e) => {
                                                                error!("Failed to place SELL order for {}: {}", side_name, e);
                                                            }
                                                        }
                                                    } else if should_place_sell && config.strategy.simulation_mode {
                                                        let side_name = if outcome.to_lowercase() == "up" { "UP" } else { "DOWN" };
                                                        let shares = if outcome.to_lowercase() == "up" { s.up_shares_received } else { s.down_shares_received };
                                                        info!("SIMULATION: Would place SELL order for {} ({} shares) at ${:.2}", side_name, shares, SELL_PRICE);
                                                        let fake_order_id = format!("SIM-SELL-{}-{}", side_name, chrono::Utc::now().timestamp());
                                                        if outcome.to_lowercase() == "up" {
                                                            s.up_sell_order_id = Some(fake_order_id);
                                                        } else {
                                                            s.down_sell_order_id = Some(fake_order_id);
                                                        }
                                                    }

                                                    *state_guard = Some(s);
                                                }
                                            }
                                        }
                                        TradeMessageStatus::Matched => {
                                            // Trade matched but not yet on-chain - log but don't act
                                            debug!("Trade {} matched, waiting for MINED status", trade.id);
                                        }
                                        _ => {
                                            debug!("Trade {} has unknown status: {:?}", trade.id, trade.status);
                                        }
                                    }
                                }
                                _ => {
                                    debug!("Received other user event");
                                }
                            }
                        }
                        Err(e) => {
                            error!("Error receiving user event: {}", e);
                        }
                    }
                }
                _ = shutdown_token.cancelled() => {
                    info!("Shutdown signal received, stopping CLOB subscription");
                    break;
                }
            }
        }

        info!("CLOB subscription ended");
        Ok(())
    }

    fn get_current_5m_period_et() -> i64 {
        MarketDiscovery::current_5m_period_start_et()
    }
    
    fn get_current_time_et() -> i64 {
        let now_utc = Utc::now();
        let now_et = now_utc.with_timezone(&New_York);
        now_et.timestamp()
    }

    async fn discover_market(&self, asset_name: &str, period_start: i64) -> Result<Option<Market>> {
        let slug = MarketDiscovery::build_5m_slug(asset_name, period_start);
        match self.api.get_market_by_slug(&slug).await {
            Ok(m) => {
                if m.active && !m.closed {
                    Ok(Some(m))
                } else {
                    Ok(None)
                }
            }
            Err(e) => {
                debug!("Failed to find market with slug {}: {}", slug, e);
                Ok(None)
            }
        }
    }

    pub async fn check_market_closure(&self) -> Result<()> {
        let trades: Vec<(String, CycleTrade)> = {
            let t = self.trades.lock().await;
            t.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        };
        if trades.is_empty() {
            return Ok(());
        }
        
        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        for (market_key, trade) in trades {
            let market_end = trade.period_timestamp + trade.market_duration_secs;
            if current_time < market_end {
                continue;
            }

            let checked = self.closure_checked.lock().await;
            if checked.get(&trade.condition_id).copied().unwrap_or(false) {
                drop(checked);
                continue;
            }
            drop(checked);

            let market = match self.api.get_market(&trade.condition_id).await {
                Ok(m) => m,
                Err(e) => {
                    warn!("Failed to fetch market {}: {}", &trade.condition_id[..16], e);
                    continue;
                }
            };
            if !market.closed {
                continue;
            }

            let up_wins = trade.up_token_id.as_ref()
                .map(|id| market.tokens.iter().any(|t| t.token_id == *id && t.winner))
                .unwrap_or(false);
            let down_wins = trade.down_token_id.as_ref()
                .map(|id| market.tokens.iter().any(|t| t.token_id == *id && t.winner))
                .unwrap_or(false);

            let total_cost = (trade.up_shares * trade.up_avg_price) + (trade.down_shares * trade.down_avg_price);
            let payout = if up_wins {
                trade.up_shares * 1.0
            } else if down_wins {
                trade.down_shares * 1.0
            } else {
                0.0
            };
            let pnl = payout - total_cost;

            let winner = if up_wins { "Up" } else if down_wins { "Down" } else { "Unknown" };
            let sim_prefix = if self.config.strategy.simulation_mode { "SIMULATION: " } else { "" };
            eprintln!("=== Market resolved {}===", sim_prefix);
            eprintln!(
                "{}Market closed | condition {} | Winner: {} | Up {:.2} @ {:.4} | Down {:.2} @ {:.4} | Cost ${:.2} | Payout ${:.2} | PnL ${:.2}",
                sim_prefix,
                &trade.condition_id[..16],
                winner,
                trade.up_shares,
                trade.up_avg_price,
                trade.down_shares,
                trade.down_avg_price,
                total_cost,
                payout,
                pnl
            );

            if !self.config.strategy.simulation_mode && (up_wins || down_wins) {
                let (token_id, outcome) = if up_wins && trade.up_shares > 0.001 {
                    (trade.up_token_id.as_deref().unwrap_or(""), "Up")
                } else {
                    (trade.down_token_id.as_deref().unwrap_or(""), "Down")
                };
                if let Err(e) = self.api.redeem_tokens(&trade.condition_id, token_id, outcome).await {
                    warn!("Redeem failed: {}", e);
                }
            }

            {
                let mut total = self.total_profit.lock().await;
                *total += pnl;
            }
            let total_actual_pnl = *self.total_profit.lock().await;
            eprintln!(
                "  -> {}PnL this market: ${:.2} | Total PnL: ${:.2}",
                sim_prefix,
                pnl,
                total_actual_pnl
            );
            {
                let mut c = self.closure_checked.lock().await;
                c.insert(trade.condition_id.clone(), true);
            }
            let mut t = self.trades.lock().await;
            t.remove(&market_key);
        }
        Ok(())
    }

    async fn place_limit_order(&self, token_id: &str, side: &str, price: f64) -> Result<OrderResponse> {
        let price = Self::round_price(price);
        if self.config.strategy.simulation_mode {
            info!("SIMULATION: Would place {} order for token {}: {} shares @ ${:.2}", 
                side, token_id, self.config.strategy.shares, price);
            
            let fake_order_id = format!("SIM-{}-{}", side, chrono::Utc::now().timestamp());
            
            Ok(OrderResponse {
                order_id: Some(fake_order_id),
                status: "SIMULATED".to_string(),
                message: Some("Order simulated".to_string()),
            })
        } else {
            let order = OrderRequest {
                token_id: token_id.to_string(),
                side: side.to_string(),
                size: self.config.strategy.shares.to_string(),
                price: price.to_string(),
                order_type: "LIMIT".to_string(),
            };
            self.api.place_order(&order).await
        }
    }

    async fn check_buy_order_matches(&self, state: &mut PreLimitOrderState) -> Result<()> {
        if state.up_matched && state.down_matched {
            return Ok(());
        }

        if !self.config.strategy.simulation_mode {
            if let (Some(up_id), Some(down_id)) = (&state.up_order_id, &state.down_order_id) {
                if !up_id.starts_with("SIM-") && !down_id.starts_with("SIM-") {
                    match self.api.are_both_orders_filled(up_id, down_id).await {
                        Ok((up_filled, down_filled)) => {
                            if up_filled && !state.up_matched {
                                info!("Up buy order filled for BTC (verified via API)");
                                state.up_matched = true;
                            }
                            if down_filled && !state.down_matched {
                                info!("Down buy order filled for BTC (verified via API)");
                                state.down_matched = true;
                            }
                        }
                        Err(e) => {
                            debug!("API fill check failed, falling back to price inference: {}", e);
                        }
                    }
                }
            }
        }

        if !state.up_matched {
            if let Ok(up_price) = self.api.get_price(&state.up_token_id, "SELL").await {
                let up_price_f64: f64 = up_price.to_string().parse().unwrap_or(0.0);
                if up_price_f64 <= BUY_PRICE || (up_price_f64 - BUY_PRICE).abs() < 0.001 {
                    info!("Up buy order matched for BTC (price ${:.4} <= ${:.2})", up_price_f64, BUY_PRICE);
                    state.up_matched = true;
                }
            }
        }
        
        if !state.down_matched {
            if let Ok(down_price) = self.api.get_price(&state.down_token_id, "SELL").await {
                let down_price_f64: f64 = down_price.to_string().parse().unwrap_or(0.0);
                if down_price_f64 <= BUY_PRICE || (down_price_f64 - BUY_PRICE).abs() < 0.001 {
                    info!("Down buy order matched for BTC (price ${:.4} <= ${:.2})", down_price_f64, BUY_PRICE);
                    state.down_matched = true;
                }
            }
        }
        
        Ok(())
    }

    fn round_price(price: f64) -> f64 {
        let rounded = (price * 100.0).round() / 100.0;
        rounded.clamp(0.01, 0.99)
    }

    fn cycle_trade_from_state(s: &PreLimitOrderState, shares: f64) -> CycleTrade {
        // Use actual shares received if available, otherwise fall back to config shares
        let up_shares = if s.up_shares_received > 0.0 { s.up_shares_received } else if s.up_mined { shares } else { 0.0 };
        let down_shares = if s.down_shares_received > 0.0 { s.down_shares_received } else if s.down_mined { shares } else { 0.0 };

        CycleTrade {
            condition_id: s.condition_id.clone(),
            period_timestamp: s.market_period_start as u64,
            market_duration_secs: MARKET_DURATION_SECS_U64,
            up_token_id: Some(s.up_token_id.clone()),
            down_token_id: Some(s.down_token_id.clone()),
            up_shares,
            down_shares,
            up_avg_price: s.up_buy_price,
            down_avg_price: s.down_buy_price,
        }
    }
}
