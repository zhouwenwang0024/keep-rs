# Filler 两分支改造实现摘要（2026-01-25）

目的：保留 2 个分支（Swift + 链上全类型交叉），发现后仍走原 try -> 交易构造流程。

## 1) 需求确认
- Swift 分支：判断任意交叉（不限 maker 可撮合）。
- 链上分支：所有链上订单类型统一交叉判断。
- 发现深度 `depth=3`；拍卖价格使用 `slot + 1`。
- 触发单必须同一 tx 里 trigger + 成交。
- 基础 CU `* 2`；账户数过多时额外加倍。
- 不记录 metrics；可移除不必要分支/冗余代码。
- 代理程序无需改动。

## 2) 实现要点
### 2.1 Swift 分支（任意交叉）
- Swift stream 收到订单后，按 `slot + 1` 计算拍卖/限价价格。
- 计算 `trigger_price` 后，调用 `collect_swift_crosses(...)` 在 L3 asks/bids 中找任意交叉（传入 `trigger_price`，`depth=3`）。
- 交叉判断使用“有效价”：trigger 单用 `post_trigger_price`，VAMM 单用 `fallback_price`，避免把触发价/0 当成交价。
- 命中交叉后构造 `MakerCrosses`，进入 `try_swift_fill`。
- `try_swift_fill` 先对交叉内的 trigger 单执行 `trigger_order`，再 `place_swift_order + proxy_spread_capture`。
- Maker 账户从交叉集合中收集（去重），再追加 taker 账户。

### 2.2 链上分支（全类型交叉）
- SDK 新增 `CrossingRegionAll` 与 `find_crossing_region_all_types(...)`，基于 L3（含 trigger_price）做全类型交叉探测。
- 交叉判断使用“有效价”：trigger 单用 `post_trigger_price`，VAMM 单用 `fallback_price`。
- `filler.rs` 里改用新接口，并用 `OrderSlotLimiter` 以 `best_bid/best_ask` 的 order id 节流。
- 新增 `try_onchain_cross`：可带 Pyth 更新；在同一 tx 内处理 trigger + `proxy_spread_capture`。
- 旧 `try_auction_fill` / `try_uncross` 分支移除。

### 2.3 交易与资源
- `base_cu = cu_limit * 2` 作为默认基准；账号数多时额外提高 CU 上限。
- 仍沿用原流程：发现 -> try -> 交易构造与发送。

## 3) 代码位置
- drift-rs
  - `crates/src/dlob/types.rs`：新增 `CrossingRegionAll`。
  - `crates/src/dlob/mod.rs`：新增 `find_crossing_region_all_types(...)`，并在交叉判断中使用有效价。
- keep-rs
  - `src/filler.rs`：`CROSS_DEPTH=3`；Swift 任意交叉检测；交叉判断使用有效价；链上改走全类型交叉 + `try_onchain_cross`；拍卖/限价使用 `slot + 1`。
  - `src/filler_trades.rs`：`try_swift_fill` 增加 `trigger_price`；新增 `try_onchain_cross`；移除旧 try。
  - `src/util.rs`：新增 `TxIntent::OnchainCross`。
- proxy：无改动（已撤回）。