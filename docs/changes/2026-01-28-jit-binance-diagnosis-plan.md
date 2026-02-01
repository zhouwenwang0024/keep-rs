# JIT Binance 数据缺失排查与修复计划

## 背景与现象
- 日志只有 `jit feed init`，没有 `jit price update / jit check / jit trigger`
- 说明 Binance WS 没有产出更新，JIT 逻辑无法进入触发分支

## 初步结论（基于现有代码）
1. `feed_binance.rs` 无连接/断线日志，可能一直失败但不可见
2. 更新条件苛刻：必须同时收到 spot + futures，且 1 秒内同时 fresh
3. 交易对强制拼 `USDC`，Binance 可能只有 `USDT` 导致无数据

## 计划修改范围（仅计划，不改代码）
### 1) 日志增强（诊断优先）
**文件：`keep-rs/src/jit/feed_binance.rs`**
- 连接成功日志：输出 market_index、symbol、spot/fut url
- 连接失败日志：输出错误与重连退避时间
- WS 断开/重连日志
- 消息接收统计（节流打印，如每 30s 输出 spot/fut 最后更新时间）
- 发送更新日志（节流）：当产生 BinancePriceUpdate 时打印价格与 ts

**文件：`keep-rs/src/filler.rs`**
- `binance_feed` 为空时，定期提示“binance feed 未产出更新”
- 若 `binance_enabled=true` 但 `jit_price_cache` 长期为空，输出 warn
- 可选：启动时打印 `jit_symbols` 列表，明确实际订阅币对

### 2) Bug 处理方案（候选，择优实施）
**方案 A：币对兼容（优先）**
- 若 `XXXUSDC` 无数据，允许 fallback 为 `XXXUSDT`
- 或允许同时订阅 `USDC + USDT` 并择优使用（优先有效流）

**方案 B：放宽更新条件**
- 允许仅 spot 或仅 futures 推动更新
- 将 `BINANCE_STALE_MS` 从 1000ms 放宽到 2000ms 或 3000ms

**方案 C：订阅失败可见化**
- 连接失败后记录连续失败次数，超过阈值输出 error

## 预期修改的日志清单（明确）
- `[BINANCE_FEED] connect ok / connect fail`
- `[BINANCE_FEED] reconnect backoff`
- `[BINANCE_FEED] last_spot_ts / last_fut_ts`
- `[BINANCE_FEED] emit update market=... mid=... ts=...`
- `[JIT] binance feed idle`（若 60s 无更新）
- `[JIT] jit_symbols=...`（启动时打印）

## 验证步骤（上线前）
1. 启动后 1 分钟内能看到 `[BINANCE_FEED] connect ok`
2. 60 秒内看到至少一次 `emit update`
3. `jit check` 每分钟至少出现一次
4. 若有跨价，出现 `jit trigger`

## 退出条件
- Binance 更新可见且 JIT 触发日志出现
- 若仍无更新，进入下一阶段：检查网络/防火墙/DNS 与交易对映射
