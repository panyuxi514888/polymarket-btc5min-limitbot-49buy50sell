# Polymarket BTC 5分钟周期限价单策略

**Polymarket BTC 5分钟周期双边限价单低买高卖机器人。** 当5分钟周期开启时，自动设置0.49美元限价单低价买入（Up和Down各5 shares），成交后自动设置0.50美元高价卖出。

---

## 功能特性

- **仅支持BTC交易**：专注单一资产，减少复杂度
- **5分钟周期**：与Polymarket 5分钟市场同步
- **双边限价单**：
  - 周期开启时：`$0.49` 限价买单（Up和Down各5 shares）
  - 买单成交后：`$0.50` 限价卖单（Up和Down各5 shares）
- **每笔交易预期利润**：$0.05（5 shares × $0.50 - 5 shares × $0.49 = $0.05）
- **模拟模式**：可在不实际下单的情况下测试策略
- **自动赎回**：市场结算后自动赎回获胜头寸

---

## 策略逻辑

```
5分钟周期开始
    │
    ▼
 设置 $0.49 买单 (Up: 5 shares, Down: 5 shares)
    │
    ▼
买单成交？
    │
     ├── 是 → 设置 $0.50 卖单
     │           │
     │           ▼
     │       卖单成交 → 盈利 $0.05/边
    │
    └── 否 → 等待周期结束
```

---

## 安装

```bash
git clone <your-repo-url>
cd polymarket-btc5min-limit-order
cargo build --release
```

---

## 配置

复制配置文件：

```bash
cp config.json.example config.json
```

编辑 `config.json`：

```json
{
  "polymarket": {
    "gamma_api_url": "https://gamma-api.polymarket.com",
    "clob_api_url": "https://clob.polymarket.com",
    "api_key": "YOUR_API_KEY",
    "api_secret": "YOUR_API_SECRET",
    "api_passphrase": "YOUR_PASSPHRASE",
    "private_key": "YOUR_PRIVATE_KEY_HEX",
    "proxy_wallet_address": "0xYourProxyWallet",
    "signature_type": 2
  },
  "strategy": {
    "shares": 5,
    "check_interval_ms": 500,
    "simulation_mode": true,
    "market_closure_check_interval_seconds": 60
  }
}
```

### 配置说明

| 配置项 | 说明 | 默认值 |
|--------|------|--------|
| `shares` | 每边交易份额 | 5 |
| `check_interval_ms` | 检查间隔（毫秒） | 500 |
| `simulation_mode` | 模拟模式（不实际下单） | true |
| `market_closure_check_interval_seconds` | 市场结算检查间隔 | 60 |

---

## 使用

### 运行机器人

```bash
# 默认配置
./target/release/btc5min-limit-order

# 自定义配置路径
./target/release/btc5min-limit-order --config /path/to/config.json
```
#### 日志添加
"""
# Git Bash/MINGW64 的语法
export LOG_FILE="btc5min.log"
./target/release/btc5min-limit-order

Windows PowerShell
$env:LOG_FILE="btc5min.log"; ./target/release/btc5min-limit-order

# Windows CMD
set LOG_FILE=btc5min.log
./target/release/btc5min-limit-order

# Linux/Mac
LOG_FILE=btc5min.log ./target/release/btc5min-limit-order
"""

### 赎回头寸

```bash
# 赎回所有可赎回头寸
./target/release/btc5min-limit-order --redeem

# 赎回指定条件
./target/release/btc5min-limit-order --redeem --condition-id 0x...
```

### 日志

```bash
# 输出到文件
LOG_FILE=btc5min.log ./target/release/btc5min-limit-order

# 控制台输出（默认）
./target/release/btc5min-limit-order
```

---
