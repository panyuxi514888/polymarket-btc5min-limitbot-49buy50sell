use clap::Parser;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::str::FromStr;

use polymarket_client_sdk::auth::ExposeSecret;
use polymarket_client_sdk::POLYGON;
use polymarket_client_sdk::derive_safe_wallet;
use polymarket_client_sdk::auth::LocalSigner;
use polymarket_client_sdk::auth::Signer;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    #[arg(short, long, default_value = "config.json")]
    pub config: PathBuf,

    #[arg(long)]
    pub redeem: bool,

    #[arg(long, requires = "redeem")]
    pub condition_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub polymarket: PolymarketConfig,
    pub strategy: StrategyConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyConfig {
    pub shares: f64,
    pub check_interval_ms: u64,
    #[serde(default)]
    pub simulation_mode: bool,
    #[serde(default = "default_market_closure_check_interval_seconds")]
    pub market_closure_check_interval_seconds: u64,
}

fn default_market_closure_check_interval_seconds() -> u64 {
    120
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolymarketConfig {
    pub gamma_api_url: String,
    pub clob_api_url: String,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    pub api_passphrase: Option<String>,
    pub private_key: Option<String>,
    pub proxy_wallet_address: Option<String>,
    pub signature_type: Option<u8>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            polymarket: PolymarketConfig {
                gamma_api_url: "https://gamma-api.polymarket.com".to_string(),
                clob_api_url: "https://clob.polymarket.com".to_string(),
                api_key: None,
                api_secret: None,
                api_passphrase: None,
                private_key: None,
                proxy_wallet_address: None,
                signature_type: None,
            },
            strategy: StrategyConfig {
                shares: 5.0,
                check_interval_ms: 2000,
                simulation_mode: true,
                market_closure_check_interval_seconds: 120,
            },
        }
    }
}

impl Config {
    pub fn load(path: &PathBuf) -> anyhow::Result<Self> {
        if path.exists() {
            let content = std::fs::read_to_string(path)?;
            Ok(serde_json::from_str(&content)?)
        } else {
            let config = Config::default();
            let content = serde_json::to_string_pretty(&config)?;
            std::fs::write(path, content)?;
            Ok(config)
        }
    }
}

/// Derive API credentials from private key using the same method as create_or_derive_api_key
pub async fn derive_api_credentials(private_key: &str) -> anyhow::Result<(String, String, String)> {
    use polymarket_client_sdk::clob;
    
    let signer = LocalSigner::from_str(private_key)
        .map_err(|_| anyhow::anyhow!("Invalid private key"))?
        .with_chain_id(Some(POLYGON));
    
    let client = clob::Client::default();
    let credentials = client.create_or_derive_api_key(&signer, None).await?;
    
    Ok((
        credentials.key().to_string(),
        credentials.secret().expose_secret().to_string(), 
        credentials.passphrase().expose_secret().to_string()
    ))
}

/// Derive POLYMARKET_ADDRESS (safe wallet address) from private key
pub fn derive_polymarket_address(private_key: &str) -> anyhow::Result<String> {
    let signer = LocalSigner::from_str(private_key)
        .map_err(|_| anyhow::anyhow!("Invalid private key"))?;
    
    let safe_addr = derive_safe_wallet(signer.address(), POLYGON)
        .ok_or_else(|| anyhow::anyhow!("Failed to derive safe wallet address"))?;
    
    Ok(safe_addr.to_string())
}
