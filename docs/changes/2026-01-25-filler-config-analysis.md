# Filler 机器人配置文件分析

> 分析改版后的 filler 机器人是否需要更新配置文件模板

---

## 1. 当前 `.env.example` 内容

```env
# Bot Configuration
# Required environment variables for running the keeper bots

# Bot wallet private key (base58 encoded)
BOT_PRIVATE_KEY="your_base58_private_key_here"

# RPC endpoint for Solana
RPC_URL="https://api.mainnet-beta.solana.com"

# gRPC configuration for Drift updates
GRPC_ENDPOINT="https://api.rpcpool.com"
GRPC_X_TOKEN="your_grpc_token_here"

# Pyth price feed access token
PYTH_LAZER_TOKEN="your_pyth_token_here"

# Optional configuration
# Metrics server port (default: 9898)
# METRICS_PORT=9898

# Market IDs to operate on (comma-separated, default: "0,1,2")
# MARKET_IDS="0,1,2"

# Network selection (default: mainnet)
# MAINNET=true

# Dry run mode (do not send transactions)
# DRY_RUN=false
```

---

## 2. Filler 机器人使用的配置项

### 2.1 从 `Config` 结构体（命令行参数）

**基础配置：**
- `dry` (对应环境变量 `DRY_RUN`)
- `all_markets` (无环境变量，命令行参数)
- `market_ids` (对应环境变量 `MARKET_IDS`)
- `mainnet` (对应环境变量 `MAINNET`)
- `sub_account_id` (无环境变量，默认值 `0`)

**Swift/On-chain Fill 配置：**
- `swift_cu_limit` (无环境变量，默认值 `364000`)
- `fill_cu_limit` (无环境变量，默认值 `256000`)

**JIT 策略配置（新增）：**
- `jit_enabled` (无环境变量，默认值 `true`)
- `jit_sub_account_id` (无环境变量，默认值 `1`)
- `jit_edge_ppm` (无环境变量，默认值 `5000`)
- `jit_max_makers_per_side` (无环境变量，默认值 `3`)
- `jit_cooldown_ms` (无环境变量，默认值 `5`)
- `jit_price_stale_ms` (无环境变量，默认值 `1500`)
- `jit_cu_limit` (无环境变量，默认值 `256000`)
- `jit_proxy_program_id` (无环境变量，默认值 `""`)

### 2.2 直接使用的环境变量

- `PYTH_LAZER_TOKEN` ✅ 已在 `.env.example` 中

---

## 3. 配置项使用位置

### 3.1 `filler.rs` 中的使用

```rust
// 基础配置
let filler_subaccount = drift.wallet.sub_account(config.sub_account_id);
let jit_subaccount = drift.wallet.sub_account(config.jit_sub_account_id);

// Swift Fill
try_swift_fill(
    drift,
    pf,
    config.swift_cu_limit,  // ✅ 使用配置
    filler_subaccount,
    signed_order,
    maker_crosses,
    trigger_price,
    &user_cache,
    tx_worker_ref.clone(),
).await;

// On-chain Cross
try_onchain_cross(
    drift,
    priority_fee,
    config.fill_cu_limit,  // ✅ 使用配置
    market_index,
    filler_subaccount,
    crosses,
    &user_cache,
    tx_worker_ref.clone(),
    pyth_update,
    trigger_price,
)
.await;

// JIT 策略初始化
let mut jit_strategy = Some(JitStrategy::new(
    config.jit_edge_ppm,              // ✅ 使用配置
    config.jit_cooldown_ms,            // ✅ 使用配置
    config.jit_price_stale_ms,         // ✅ 使用配置
    config.jit_max_makers_per_side,    // ✅ 使用配置
));

// JIT 执行
try_jit(
    drift,
    pf,
    config.jit_cu_limit,                // ✅ 使用配置
    jit_subaccount,
    &intent,
    &user_cache,
    tx_worker_ref.clone(),
    jit_proxy_program_id,               // ✅ 使用配置
)
.await;

// 环境变量
let pyth_access_token = std::env::var("PYTH_LAZER_TOKEN").expect("pyth access token");
```

---

## 4. 是否需要更新 `.env.example`？

### 4.1 当前状态

**已包含：**
- ✅ `PYTH_LAZER_TOKEN` (必需)
- ✅ `MARKET_IDS` (可选，有注释)
- ✅ `MAINNET` (可选，有注释)
- ✅ `DRY_RUN` (可选，有注释)

**缺失：**
- ❌ JIT 相关配置项（全部缺失）
- ❌ `sub_account_id` (可选，但建议添加)
- ❌ `jit_sub_account_id` (可选，但建议添加)
- ❌ CU limits (可选，但建议添加)

### 4.2 建议更新

**原因：**
1. **JIT 功能是新增的**：改版后新增了 JIT 策略，相关配置项应该添加到配置模板中
2. **提高可配置性**：虽然这些配置项有默认值，但用户可能需要根据实际情况调整
3. **文档完整性**：配置文件模板应该反映所有可配置项，特别是新增的功能

---

## 5. 建议的 `.env.example` 更新

### 5.1 新增配置项（建议添加）

```env
# JIT Strategy Configuration
# JIT subaccount ID (default: 1)
# JIT_SUB_ACCOUNT_ID=1

# JIT edge threshold in ppm (1e6 = 100%, default: 5000)
# JIT_EDGE_PPM=5000

# Max makers per side for JIT (default: 3)
# JIT_MAX_MAKERS_PER_SIDE=3

# JIT cooldown in milliseconds (default: 5)
# JIT_COOLDOWN_MS=5

# JIT price staleness limit in milliseconds (default: 1500)
# JIT_PRICE_STALE_MS=1500

# JIT compute unit limit (default: 256000)
# JIT_CU_LIMIT=256000

# JIT proxy program ID (optional, empty to disable)
# JIT_PROXY_PROGRAM_ID=""

# Filler subaccount ID (default: 0)
# SUB_ACCOUNT_ID=0

# Swift order compute unit limit (default: 364000)
# SWIFT_CU_LIMIT=364000

# On-chain fill compute unit limit (default: 256000)
# FILL_CU_LIMIT=256000
```

### 5.2 完整更新后的 `.env.example`

```env
# Bot Configuration
# Required environment variables for running the keeper bots

# Bot wallet private key (base58 encoded)
BOT_PRIVATE_KEY="your_base58_private_key_here"

# RPC endpoint for Solana
RPC_URL="https://api.mainnet-beta.solana.com"

# gRPC configuration for Drift updates
GRPC_ENDPOINT="https://api.rpcpool.com"
GRPC_X_TOKEN="your_grpc_token_here"

# Pyth price feed access token (required for filler bot)
PYTH_LAZER_TOKEN="your_pyth_token_here"

# Optional configuration
# Metrics server port (default: 9898)
# METRICS_PORT=9898

# Market IDs to operate on (comma-separated, default: "0,1,2")
# MARKET_IDS="0,1,2"

# Network selection (default: mainnet)
# MAINNET=true

# Dry run mode (do not send transactions)
# DRY_RUN=false

# Filler subaccount ID (default: 0)
# SUB_ACCOUNT_ID=0

# Swift order compute unit limit (default: 364000)
# SWIFT_CU_LIMIT=364000

# On-chain fill compute unit limit (default: 256000)
# FILL_CU_LIMIT=256000

# JIT Strategy Configuration
# JIT subaccount ID (default: 1)
# JIT_SUB_ACCOUNT_ID=1

# JIT edge threshold in ppm (1e6 = 100%, default: 5000)
# JIT_EDGE_PPM=5000

# Max makers per side for JIT (default: 3)
# JIT_MAX_MAKERS_PER_SIDE=3

# JIT cooldown in milliseconds (default: 5)
# JIT_COOLDOWN_MS=5

# JIT price staleness limit in milliseconds (default: 1500)
# JIT_PRICE_STALE_MS=1500

# JIT compute unit limit (default: 256000)
# JIT_CU_LIMIT=256000

# JIT proxy program ID (optional, empty to disable)
# JIT_PROXY_PROGRAM_ID=""
```

---

## 6. 注意事项

### 6.1 环境变量 vs 命令行参数

**当前实现：**
- 大部分配置项通过命令行参数传递（`clap`）
- 只有少数配置项支持环境变量（`MARKET_IDS`, `MAINNET`, `DRY_RUN`）

**建议：**
- 如果希望这些配置项支持环境变量，需要在 `Config` 结构体中添加 `env` 属性
- 例如：`#[clap(long, env = "JIT_SUB_ACCOUNT_ID", default_value = "1")]`

### 6.2 必需 vs 可选

**必需的环境变量：**
- `BOT_PRIVATE_KEY`
- `RPC_URL`
- `GRPC_ENDPOINT`
- `GRPC_X_TOKEN`
- `PYTH_LAZER_TOKEN` (filler 机器人必需)

**可选的环境变量：**
- 其他所有配置项都有默认值，可以通过命令行参数或环境变量覆盖

---

## 7. 总结

### 7.1 是否需要更新？

**答案：建议更新**

**原因：**
1. ✅ JIT 功能是新增的，配置模板应该反映这些新功能
2. ✅ 提高可配置性和文档完整性
3. ✅ 虽然大部分配置项有默认值，但用户可能需要根据实际情况调整

### 7.2 更新优先级

**高优先级（建议立即添加）：**
- `PYTH_LAZER_TOKEN` ✅ 已存在
- JIT 相关配置项（新增功能）

**中优先级（可选）：**
- CU limits (`SWIFT_CU_LIMIT`, `FILL_CU_LIMIT`, `JIT_CU_LIMIT`)
- Subaccount IDs (`SUB_ACCOUNT_ID`, `JIT_SUB_ACCOUNT_ID`)

**低优先级（已有默认值，可选）：**
- 其他 JIT 策略参数（有合理的默认值）

### 7.3 实施建议

1. **更新 `.env.example`**：添加上述建议的配置项（注释形式）
2. **可选：支持环境变量**：在 `Config` 结构体中为这些配置项添加 `env` 属性
3. **更新 README.md**：在 README 中说明这些新配置项的用途

---

**文档生成时间**：2026-01-25  
**分析文件**：`src/filler.rs`, `src/main.rs`, `.env.example`
