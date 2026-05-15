mod api;
mod clob_client;
mod config;
mod discovery;
mod models;
mod strategy;

use anyhow::Result;
use api::PolymarketApi;
use clap::Parser;
use config::{Args, Config};
use log::{warn, LevelFilter};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use strategy::PreLimitStrategy;

fn setup_logging(log_file: Option<&Path>) -> Result<()> {
    if let Some(path) = log_file {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        env_logger::Builder::from_default_env()
            .filter_level(LevelFilter::Info)
            .format(|buf, record| {
                let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
                writeln!(
                    buf,
                    "[{}] {} - {}",
                    timestamp,
                    record.level(),
                    record.args()
                )
            })
            .target(env_logger::Target::Pipe(Box::new(file)))
            .init();
    } else {
        env_logger::Builder::from_default_env()
            .filter_level(LevelFilter::Info)
            .format(|buf, record| {
                let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
                writeln!(
                    buf,
                    "[{}] {} - {}",
                    timestamp,
                    record.level(),
                    record.args()
                )
            })
            .init();
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls CryptoProvider");

    let log_file = std::env::var("LOG_FILE").ok();
    let log_path = log_file.as_ref().map(|s| Path::new(s.as_str()));
    setup_logging(log_path)?;

    let config = Config::load(&args.config)?;
    log::info!("Config loaded from: {:?}", args.config);
    log::info!(
        "Private key present in config: {}",
        config.polymarket.private_key.is_some()
    );

    let shares = config.strategy.shares;
    let buy_price = config.strategy.buy_price;
    let sell_price = config.strategy.sell_price;
    let cost_per_trade = shares * buy_price * 2.0;

    eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    eprintln!("BTC 5分钟周期限价单策略");
    eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    eprintln!("  买单价格: ${:.2}", buy_price);
    eprintln!("  卖单价格: ${:.2}", sell_price);
    eprintln!("  每边份额: {:.0}", shares);
    eprintln!("  每笔交易成本: ${:.2}", cost_per_trade);
    eprintln!(
        "  每笔交易预期利润: ${:.2}",
        shares * sell_price * 2.0 - cost_per_trade
    );
    eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    if config.strategy.simulation_mode {
        eprintln!("运行模式: 模拟（不实际下单）");
    } else {
        eprintln!("运行模式: 实盘");
    }
    eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    let api = Arc::new(PolymarketApi::new(
        config.polymarket.gamma_api_url.clone(),
        config.polymarket.clob_api_url.clone(),
        config.polymarket.api_key.clone(),
        config.polymarket.api_secret.clone(),
        config.polymarket.api_passphrase.clone(),
        config.polymarket.private_key.clone(),
        config.polymarket.proxy_wallet_address.clone(),
        config.polymarket.signature_type,
        config.rpc.clone(),
        config.rpc_backup.clone(),
        config.polymarket.use_relayer,
        config.polymarket.relayer_api_key.clone(),
        config.polymarket.relayer_api_key_address.clone(),
    ));

    // Authenticate first if private key is available (required for --redeem and --merge-only)
    if config.polymarket.private_key.is_some() {
        if let Err(e) = api.authenticate().await {
            log::error!("认证失败: {}", e);
            anyhow::bail!("认证失败，请检查凭证。");
        }
    }

    if args.redeem {
        run_redeem_only(api.as_ref(), &config, args.condition_id.as_deref()).await?;
        return Ok(());
    }

    if args.merge_only {
        run_merge_only(api.as_ref(), &config).await?;
        return Ok(());
    }

    if config.polymarket.private_key.is_none() {
        log::warn!("未提供私钥，机器人将仅监控市场。");
    }

    let market_closure_interval = config.strategy.market_closure_check_interval_seconds;
    let strategy = Arc::new(PreLimitStrategy::new(api, config).await);
    let strategy_for_closure = Arc::clone(&strategy);

    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(tokio::time::Duration::from_secs(market_closure_interval));
        loop {
            interval.tick().await;
            if let Err(e) = strategy_for_closure.check_market_closure().await {
                warn!("检查市场关闭时出错: {}", e);
            }
        }
    });

    strategy.run().await
}

async fn run_merge_only(api: &PolymarketApi, config: &Config) -> Result<()> {
    let wallet = if let Some(ref addr) = config.polymarket.proxy_wallet_address {
        addr.clone()
    } else if let Some(ref pk) = config.polymarket.private_key {
        crate::config::derive_polymarket_address(pk)?
    } else {
        anyhow::bail!("--merge-only 需要 proxy_wallet_address 或 private_key")
    };

    eprintln!("合并模式 (wallet: {})", wallet);
    eprintln!("查询可合并头寸...");

    let positions = api.get_mergeable_positions(&wallet).await?;
    if positions.is_empty() {
        eprintln!("没有找到可合并的头寸。");
        return Ok(());
    }

    eprintln!("找到 {} 个可合并头寸。", positions.len());
    for pos in &positions {
        eprintln!(
            "  - {} | size: {:.2} | {}",
            &pos.condition_id[..pos.condition_id.len().min(18)],
            pos.size,
            pos.title
        );
    }

    const MAX_RETRIES: u32 = 3;
    const INITIAL_DELAY_MS: u64 = 2000;

    let mut ok = 0u32;
    let mut fail = 0u32;
    for pos in &positions {
        eprintln!(
            "\n--- 合并 {} (size: {:.2}) ---",
            &pos.condition_id[..pos.condition_id.len().min(18)],
            pos.size
        );
        match api
            .merge_shares_with_retry(&pos.condition_id, pos.size, MAX_RETRIES, INITIAL_DELAY_MS)
            .await
        {
            Ok(tx_hash) => {
                eprintln!("成功: {} (tx: {})", pos.condition_id, tx_hash);
                ok += 1;
            }
            Err(e) => {
                eprintln!("失败: {} (跳过) - {}", &pos.condition_id[..pos.condition_id.len().min(18)], e);
                fail += 1;
            }
        }
    }

    eprintln!("\n合并完成。成功: {}, 失败: {}", ok, fail);
    Ok(())
}

async fn run_redeem_only(
    api: &PolymarketApi,
    config: &Config,
    condition_id: Option<&str>,
) -> Result<()> {
    let proxy = config
        .polymarket
        .proxy_wallet_address
        .as_deref()
        .ok_or_else(|| {
            anyhow::anyhow!("--redeem 需要在 config.json 中设置 proxy_wallet_address")
        })?;

    eprintln!("赎回模式 (proxy: {})", proxy);
    let cids: Vec<String> = if let Some(cid) = condition_id {
        let cid = if cid.starts_with("0x") {
            cid.to_string()
        } else {
            format!("0x{}", cid)
        };
        eprintln!("赎回条件: {}", cid);
        vec![cid]
    } else {
        eprintln!("获取可赎回头寸...");
        let list = api.get_redeemable_positions(proxy).await?;
        if list.is_empty() {
            eprintln!("没有找到可赎回的头寸。");
            return Ok(());
        }
        eprintln!("找到 {} 个条件需要赎回。", list.len());
        list
    };

    let mut ok_count = 0u32;
    let mut fail_count = 0u32;
    for cid in &cids {
        eprintln!("\n--- 赎回条件 {} ---", &cid[..cid.len().min(18)]);

        // Check which outcome won before redeeming
        let outcome = match api.get_market(cid).await {
            Ok(market) => {
                let up_wins = market.tokens.iter().any(|t| {
                    let out = t.outcome.to_lowercase();
                    t.winner && (out.contains("up") || out == "yes")
                });
                let down_wins = market.tokens.iter().any(|t| {
                    let out = t.outcome.to_lowercase();
                    t.winner && (out.contains("down") || out == "no")
                });
                if up_wins {
                    "Up"
                } else if down_wins {
                    "Down"
                } else {
                    eprintln!("跳过 {}: 市场未结算，无法确定获胜方", &cid[..cid.len().min(18)]);
                    fail_count += 1;
                    continue;
                }
            }
            Err(e) => {
                eprintln!("跳过 {}: 获取市场信息失败: {}", &cid[..cid.len().min(18)], e);
                fail_count += 1;
                continue;
            }
        };
        eprintln!("  获胜方: {}", outcome);

        match api.redeem_tokens(cid, "", outcome).await {
            Ok(_) => {
                eprintln!("成功: {}", cid);
                ok_count += 1;
            }
            Err(e) => {
                eprintln!("失败 {}: {} (跳过)", cid, e);
                fail_count += 1;
            }
        }

        // Small delay to avoid rate limiting
        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
    }
    eprintln!("\n赎回完成。成功: {}, 失败: {}", ok_count, fail_count);
    Ok(())
}
