# Filler 双分支改造方案（Swift + 全链上交叉）

> 本文目标：把现有 filler 从“Swift + 拍卖/触发 + uncross”三路，收敛为两路：
> 1) Swift 订单（保持现有路径）；
> 2) 链上全类型订单交叉机会发现（不区分拍卖/触发/Resting），并把机会交给原有 try_* 流程构造交易。
>
> 重要澄清（按你的新要求）：
> - “全链上交叉”是 **发现** 价格交叉，不负责本地撮合决策。
> - 发现阶段 **不过滤** post-only / 同账户 / taker-taker / maker-maker。
> - 发现结果必须进入原有 try_* 交易构造流程（不是单独上报）。
> - trigger 单必须 **同 tx 触发 + 成交**。
> - depth 固定为 3。
> - 拍卖价格计算默认 `slot + 1`。
> - 交易发送时默认基础 CU 预算 `* 2`。

---

## 0) 需求与边界（确认版）

### 0.1 必须满足
- 发现 **任何价格交叉** 的链上订单组合（全类型、全来源）。
- 发现阶段不做成交限制，不排除 post-only，也不排除同账户。
- 发现结果 **进入原有 try_* 交易构造流程**。
- trigger 单必须 **同 tx 触发 + 成交**。
- depth 固定为 **3**。
- 拍卖价格计算默认 **slot + 1**。
- 交易发送时基础 CU 预算 **乘以 2**。

### 0.2 非目标
- 不在发现阶段做利润/金额门槛过滤（`cross_meets_thresholds` 可选）。
- 不新增独立“上报/观察”分支（避免冗余分支）。

---

## 1) 现状问题（为何必须改）
当前 `FillerBot::run` 的三路：
1) Swift：`subscribe_swift_orders` + `find_crosses_for_taker_order` + `try_swift_fill`
2) 拍卖/触发：`find_crosses_for_auctions` + `try_auction_fill`
3) Resting 交叉：`find_crossing_region` + `try_uncross`

问题：
- `find_crossing_region` 内部固定 `trigger_price=None`，触发单不进入交叉判断。
- `find_crosses_for_auctions` 只做 taker->maker，不覆盖 taker-taker / maker-maker。
- 交叉发现分散且重复，无法覆盖“全类型交叉”。

---

## 2) 全类型交叉定义（基于 SDK L3 合流）
使用 SDK 的合流规则（`L3Book::bids/asks`）：
- fixed: resting limit + market orders
- floating: floating limit + oracle orders
- trigger: 仅当 `trigger_price=Some` 时，触发单以 post-trigger price 参与排序
- vAMM: 需要 `perp_market` 计算 fallback 价格
- 交叉判断必须用“有效价”：trigger 单用 `post_trigger_price`，vAMM 用 `fallback_price`，不能直接使用 L3Order.price

因此“全类型交叉”必须基于：
```
book.bids(Some(oracle_price), perp_market, Some(trigger_price))
book.asks(Some(oracle_price), perp_market, Some(trigger_price))
```

这能发现：
- limit <-> limit（fixed / floating）
- auction <-> limit
- auction <-> auction（两边都是拍卖单）
- trigger <-> 其他（满足触发条件后）
- vAMM <-> 订单簿

补充：拍卖价格计算默认 **slot + 1**。

---

## 3) “CrossingRegionAll + find_crossing_region_all_types”是什么意思
- `CrossingRegionAll`：新的 **数据结构**，用于保存“全类型交叉区”的 best_bid/best_ask + crossing_bids/asks。
- `find_crossing_region_all_types`：新的 **单一检测方法**，只负责“全类型交叉发现”。
- 这不是“两套检测方法”，而是 **一个结构 + 一个方法**。

建议签名：
```
fn find_crossing_region_all_types(
    dlob: &DLOB,
    market_index: u16,
    market_type: MarketType,
    oracle_price: u64,
    perp_market: Option<&PerpMarket>,
    trigger_price: u64,
    depth: usize, // 固定为 3
) -> Option<CrossingRegionAll>
```

算法要点：
1) 用合流 bids/asks（带 trigger_price）。
2) best_bid < best_ask => 无交叉。
3) 取 depth=3 的 crossing_bids/asks。
4) 只检测，不做成交过滤。

---

## 4) 发现阶段 vs 执行阶段（仍然分离，但执行走 try_*）

### 4.1 发现阶段（本地）
- 只做“是否交叉 + 输出 crossing 区域”。
- 不做：post-only / 同账户 / 利润过滤 / maker-taker 判断。

### 4.2 执行阶段（走原有 try_*）
- **必须走原有 try_* 交易构造**，不新增独立上报流程。
- trigger 订单必须同 tx 触发 + 成交。
- CU 预算默认 `* 2`。

---

## 5) 两个分支的具体执行路径（明确）

### 5.1 Swift 分支（改为“任意交叉”判断）
- Swift 仍是独立分支。
- 交叉判断改为 **任意交叉**：
  - 用 `find_crossing_region_all_types` 或等价逻辑判断 Swift 订单是否与任意订单交叉。
  - 不再仅依赖 `find_crosses_for_taker_order` 的 maker 交叉。
- 进入执行：
  - 仍走 `try_swift_fill`，但需要 **扩展 maker/account 列表来源**，允许包含任意交叉订单的用户账户（不局限 maker）。
  - 这样 proxy arb 指令能在链上扫描剩余账户，决定真实成交路径。

### 5.2 全链上交叉分支（统一入口）
- detection：`find_crossing_region_all_types`（depth=3）。
- execution：需要一个 **统一 try 入口** 来消费 `CrossingRegionAll`。

#### 是否需要新 try？结论：**需要**
原因：
- `try_auction_fill` 依赖 `CrossesAndTopMakers`（taker->maker）结构，不适配全类型 crossing。
- `try_uncross` 不处理 trigger，也不覆盖 auction/trigger 的触发逻辑。
- 全类型 crossing 包含 taker-taker / maker-maker / trigger 混合，必须统一处理。

建议新增：
```
try_onchain_cross(crosses: CrossingRegionAll, ...)
```
职责：
1) 选取 best_bid/best_ask 作为主要成交对手。
2) 收集 depth=3 的 crossing_bids/asks 用户账户，构建 proxy_spread_capture 账户列表。
3) 若包含 trigger 且未触发，插入 `trigger_order` 指令（可最多插入 1~2 条）。
4) 在同 tx 内继续调用 proxy_spread_capture（满足“触发 + 成交”）。
5) CU 预算默认 `* 2`。

> 旧的 `try_auction_fill` / `try_uncross` 可被 try_onchain_cross 替代，从而满足“移除冗余分支”的要求。

---

## 6) 代理程序（arb_perp）与方案匹配性分析
基于 `D:\RUST\proxy\jit-proxy` 的 `arb_perp`：
- arb_perp 会从 remaining_accounts 中扫描 **用户订单 + vAMM**，寻找最佳 bid/ask。
- 它不会处理 **未触发的 trigger**（代码里 `must_be_triggered && !triggered` 直接跳过）。
- 结论：
  - 发现阶段可以包含 trigger，但 **必须在 keep-rs 侧先 trigger**，再调用 proxy。
  - depth=3 与 arb_perp 的 top-3 扫描结构一致。

因此方案正确性：
- 检测只要能提供 crossing 的用户账户列表，arb_perp 就能自主选择成交路径。
- 但 trigger 必须在 keep-rs 的 try_* 中先触发，保持同 tx 触发 + 成交。

---

## 7) SDK / Proxy 修改点（允许时可做）
- SDK（drift-rs）可新增 `find_crossing_region_all_types`，或给 `find_crossing_region` 增加 `trigger_price` 参数。
- Proxy 可选修改：价格扫描时使用 `slot + 1`（与 off-chain 规则一致）。
  - 例如 `find_best_prices_with_vamm` 内部的 `force_get_limit_price` 使用 `slot + 1`。

---

## 8) 最小落地路径（按你的要求）
1) SDK 或 keep-rs 新增 `CrossingRegionAll + find_crossing_region_all_types`（只检测）。
2) Swift 分支：改为“任意交叉”判断 + 扩展 try_swift_fill 的账户选择。
3) 链上分支：新增 `try_onchain_cross`，替代 try_auction_fill / try_uncross。
4) 默认 `depth=3`，拍卖价格用 `slot + 1`，CU 基准 `* 2`。
5) 直接移除不必要分支与冗余代码。
