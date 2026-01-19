# 拍卖/触发单填充策略

## 目标
在每个 slot 周期扫描订单簿中的拍卖单与触发单，尽快完成撮合成交或触发后成交。

## 触发时机
- 每次 slot 更新时，在 `FillerBot::run` 的 slot 分支执行。
- 通过 `DLOB.find_crosses_for_auctions(...)` 找到可成交的 taker 订单。

## 关键输入
- 当前 slot、oracle 价格、触发价格。
- 可选 Pyth Lazer 价格更新（当 Pyth 价格与 oracle 不一致时）。
- `CrossesAndTopMakers`（taker 订单 + maker 列表 + 顶部 maker）。
- `priority_fee` 与 `fill_cu_limit`。
- keeper/filler 子账户。

## 详细流程
1) 计算当前 oracle 价格与触发价格：
   - 通过 `perp_market.get_trigger_price(...)` 计算触发价格。
   - 如 Pyth 更新存在且价格不同，则以 Pyth 价格为准，并准备 `post_pyth_lazer_oracle_update`。
2) 调用 `DLOB.find_crosses_for_auctions(...)` 获得可成交订单集合。
3) 使用 `OrderSlotLimiter` 过滤同 slot 重复的订单（按 order_id）。
4) 对每个 taker 订单执行：
   - 拉取 taker 用户与 `UserStats`（缺失则跳过）。
   - 创建交易并设置 `priority_fee` 与 `fill_cu_limit`。
   - 如果存在 Pyth 更新且本次尚未发送，则添加 `post_pyth_lazer_oracle_update`（只发送一次）。
   - 若 taker 是触发单：
     - 从 taker 的 on-chain 订单里读取 `trigger_price/trigger_condition`。
     - 触发条件不满足则直接退出本次处理。
     - 满足条件时先 `trigger_order(...)` 再尝试撮合。
   - 从 cross 列表收集 maker：
     - 若 vAMM cross 且市场 drawdown 且 JIT maker 触发，则跳过。
     - 若无 vAMM 且 maker 为空，跳过。
     - 若 maker 数 < 3，用 `top_maker_asks/bids` 补齐深度。
   - 调用 `fill_perp_order(...)` 填充拍卖单。
   - 若指令账户 >= 20，则 CU 上限翻倍。
5) 通过 `TxWorker` 发送 `TxIntent::AuctionFill`。

## 跳过/保护规则
- 重复订单（同 slot）被 `OrderSlotLimiter` 丢弃。
- 触发条件不满足直接跳过。
- maker 列表为空且无 vAMM cross 时跳过。
- vAMM cross 在 drawdown + JIT 条件下会跳过。

## 性能与费用
- 每 slot 扫描所有配置市场，费用与耗时随市场数增加。
- 指令账户多时自动提高 CU 以降低失败率。

## 可观测性
- 日志包含 cross 发现、触发尝试、跳过原因。
- `TxWorker` 统计成交与失败类型。

## 相关代码
- `src/filler.rs`：`FillerBot::run` (slot 分支)、`try_auction_fill`、`amm_wants_to_jit_make`
