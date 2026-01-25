# Filler 策略侧问题分析与计划（reduce-only / update AMM / auction slot）

## 问题与目标
当前发现三类潜在偏差：
1) 交叉判定时是否把“无效 reduce-only 订单”当成有效报价参与排序与交叉判断。
2) filler 发送交易时是否缺少 `update_amm(s)` 指令，导致 AMM 状态滞后。
3) 链上订单交叉分支对拍卖订单计价的 `slot` 是否应当使用 `slot + 1`，避免机会发现滞后。

目标：不改入口签名的前提下，使策略端的“可成交性判断”更贴合链上逻辑，减少误报/错失，并降低失败交易概率。

---

## 已确认的代码现状（定位）

### 1) reduce-only 是否参与排序/交叉判定
- DLOB 在 L3 构建时仅设置 `reduce_only` 标记，不做有效性过滤：
  - `D:\RUST\drift-rs\crates\src\dlob\mod.rs`（`load_orderbook`）
  - `D:\RUST\drift-rs\crates\src\dlob\types.rs`（`L3Order::is_reduce_only`）
- 交叉判定与排序路径中没有基于 reduce-only 的过滤：
  - `D:\RUST\drift-rs\crates\src\dlob\mod.rs`（`L3Book::bids/asks`、`find_crossing_region_all_types`）

结论：当前 reduce-only 订单仍会参与 L3 排序与交叉判定，无效 reduce-only（持仓方向同向或无仓位）可能造成伪报价与误报。

### 2) filler 交易是否包含 update_amm(s)
- keep-rs 构建交易路径：
  - `D:\RUST\keep-rs\src\filler_trades.rs` 中 `try_swift_fill` / `try_onchain_cross` 仅拼 `post_pyth_lazer_oracle_update` + `place_*` / `proxy_*`，未插入 `update_amm(s)`。
- drift-rs SDK 中没有现成 `update_amm(s)` 构造方法：
  - `D:\RUST\drift-rs\crates\src\lib.rs` 未提供对应 builder。
  - `D:\RUST\drift-rs\crates\src\drift_idl.rs` 存在 `UpdateAmms` 指令定义（data + accounts）。

结论：当前 filler 交易未更新 AMM，如需该指令必须扩展 SDK 或在 keep-rs 手工构造并插入。

### 3) onchain 交叉分支的拍卖计价 slot
- DLOB 快照价格计算使用 `book.slot` 固化：
  - `D:\RUST\drift-rs\crates\src\dlob\mod.rs`（`load_orderbook`）
  - `D:\RUST\drift-rs\crates\src\dlob\types.rs`（`MarketOrder/OracleOrder::get_price`）
- keep-rs onchain 分支直接调用 DLOB：
  - `D:\RUST\keep-rs\src\filler.rs`（`dlob.find_crossing_region_all_types(...)`）
- swift 分支已使用 `slot + 1` 计算拍卖价格：
  - `D:\RUST\keep-rs\src\filler.rs`

结论：onchain 分支当前没有 `slot + 1`，仅在 keep-rs 调整 slot 还不足以修正 DLOB 侧拍卖计价，需要 DLOB 支持 slot override 或动态计价。

---

## 方案与落点（已定）

### A) reduce-only 过滤（keep-rs 策略侧）
- 采用 keep-rs 侧过滤：在 DLOB 返回 crossing 结果后，基于 `WsAccountCache` 的持仓信息过滤无效 reduce-only，再重新计算 best_bid/best_ask 与交叉判定。
- 过滤规则：
  - reduce-only 且该市场持仓 = 0 → 剔除。
  - reduce-only 且持仓方向与订单方向同向 → 剔除。
  - 其他 reduce-only 保留。
- 原因：DLOB 无持仓上下文，改 DLOB 牵涉结构性变动；策略端过滤成本更低、落地更快。

### B) update_amm(s) 指令插入（新增 SDK 构造）
- SDK 新增：`TransactionBuilder::update_amms(market_indexes: Vec<u16>) -> Self`。
- 指令 data：`drift_idl::instructions::UpdateAmms { market_indexes }`。
- 固定 accounts：`state` + `authority`。
- remaining accounts 顺序（对每个 market_index）：
  1) `oracle`（只读）
  2) `perp_market.pubkey`（可写）
- 来源：`program_data.perp_market_config_by_index(market_index)` 中的 `amm.oracle` 与 `pubkey`。
- keep-rs 落地：在 `try_swift_fill` / `try_onchain_cross` 的业务指令前插入 `update_amms`。

### C) onchain 分支 slot + 1（DLOB 底层统一）
- 目标：只改“拍卖价格计算”的 slot，不改“拍卖生命周期/过期判断”的 slot。
- DLOB 改动点（两条底层路径都要改）：
  1) **L3 快照路径**：`load_orderbook` 中 `MarketOrder::get_price` / `OracleOrder::get_price` 传入 `auction_slot = self.slot + 1`。
  2) **触发单路径**：`L3Order::post_trigger_price` 调用处统一传入 `auction_slot = book.slot + 1`。
- 预期效果：onchain 分支拍卖计价与 swift 分支一致，机会发现提前 1 slot，减少滞后。

---

## 后续验证建议
1) reduce-only 用例回放：同向持仓 + reduce-only 订单，确认不再进入 best bid/ask。
2) update_amm(s) 插入前后对比：统计 `taker does not cross amm` 失败率。
3) onchain slot + 1：回放拍卖单边界，验证机会发现是否提前且与链上一致。