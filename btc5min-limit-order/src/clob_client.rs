use std::sync::Arc;

use anyhow::Result;
use log::{warn, info, error, debug};

use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::Normal;
use polymarket_client_sdk::clob::ws::Client as WsClient;

use polymarket_client_sdk::types::Address;
use polymarket_client_sdk::auth::Credentials;


use uuid::Uuid;
use std::str::FromStr;

use crate::config::Config;

/// CLOB客户端 - 负责WebSocket连接、认证和数据订阅
#[derive(Clone)]
pub struct ClobClient {

    /// 全局配置
    config: Config,

    /// 认证客户端（用于用户事件订阅）
    pub authenticated_client: Arc<tokio::sync::RwLock<Option<WsClient<Authenticated<Normal>>>>>,


}

impl ClobClient {
    /// 创建新的CLOB客户端实例
    pub async fn new(config: &Config) -> anyhow::Result<Self> {
        info!("Initializing CLOB WebSocket client with config");
        
        // 检查配置中的私钥
        if config.polymarket.private_key.is_some() {
            info!("Private key found in config, will attempt authentication");
        } else {
            info!("No private key found in config, authentication will be skipped");
        }
                
        // 尝试创建认证客户端（用于用户事件）
        info!("Calling create_authenticated_client...");
        let authenticated_client = match Self::create_authenticated_client(config).await {
            Ok(client) => {
                info!("Successfully created authenticated CLOB client");
                Arc::new(tokio::sync::RwLock::new(Some(client)))
            }
            Err(e) => {
                warn!("Failed to create authenticated client: {}. User events will not be available.", e);
                Arc::new(tokio::sync::RwLock::new(None))
            }
        };
        
        info!("ClobClient initialization completed");
        Ok(Self { 
            config: config.clone(),
            authenticated_client,
        })
    }
    
    /// 创建认证客户端
    async fn create_authenticated_client(config: &Config) -> Result<WsClient<Authenticated<Normal>>> {
        // 从配置中读取私钥
        let private_key = config.polymarket.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("private_key not found in config.polymarket"))?;
        
        // 派生API凭证和地址
        let (api_key, api_secret, api_passphrase) = crate::config::derive_api_credentials(&private_key).await?;
        let address = crate::config::derive_polymarket_address(&private_key)?;
        
        info!("API Key: {}", api_key);
        info!("API Secret: {}", api_secret);
        info!("API Passphrase: {}", api_passphrase);
        info!("Address: {}", address);
        
        let api_key = Uuid::parse_str(&api_key)?;
        let address = Address::from_str(&address)?;
        let credentials = Credentials::new(api_key, api_secret, api_passphrase);
        
        // 使用authenticate方法进行认证
        let client = WsClient::default()
            .authenticate(credentials, address)?;
            
        info!("connected to authenticated WebSocket");
        
        Ok(client)
    }
}