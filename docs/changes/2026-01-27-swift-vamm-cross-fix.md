# Swift VAMM Cross 修复说明

## 问题
- Swift 路径构造 `MakerCrosses` 时将 `has_vamm_cross` 硬编码为 `false`。
- 结果是 Swift 订单永远被视为“无 VAMM 交叉”，即使价格/数量满足 VAMM 交叉条件也会被忽略。
- 当 maker 列表为空时，`if !crosses.is_empty()` 会直接跳过，导致 VAMM-only 机会完全丢失。

## 改动目标
- Swift 路径恢复 VAMM 交叉判断，使其与旧版 DLOB 逻辑一致：
  - 方向为 Long：`taker_price > vamm_ask`
  - 方向为 Short：`taker_price < vamm_bid`
  - 同时满足 `taker_size > amm.min_order_size`
- 即使没有 maker 订单，只要存在 VAMM 交叉，也允许继续走填单流程。

## 改动内容
文件：`D:\RUST\keep-rs\src\filler.rs`

### 1) 计算 Swift 路径的 `has_vamm_cross`
- 使用当前计算出的 taker 价格 `price` 和 VAMM 价格 `vamm_price`。
- 使用 `perp_market.amm.min_order_size` 作为最小成交门槛。

新增逻辑（概念）：
```
has_vamm_cross =
    taker_size > amm.min_order_size
    && ((Long && price > vamm_ask) || (Short && price < vamm_bid))
```

### 2) 允许 VAMM-only 继续流程
- 原逻辑：`if !crosses.is_empty() { ... }`
- 新逻辑：`if !crosses.is_empty() || has_vamm_cross { ... }`

### 3) `MakerCrosses` 中写入真实的 `has_vamm_cross`
- 原逻辑：`has_vamm_cross: false`
- 新逻辑：`has_vamm_cross: has_vamm_cross`

## 影响范围
- 只影响 Swift 路径的 VAMM 交叉判断与 `MakerCrosses` 标志。
- 不改变 maker 订单筛选、reduce-only 过滤、以及 onchain 路径。

## 验证建议
1) 构造仅 VAMM 交叉、无 maker 的 Swift 场景：
   - 之前会跳过；
   - 现在应进入 fill 流程，且 `has_vamm_cross=true`。
2) 构造价格未交叉的场景：
   - `has_vamm_cross=false`，行为与之前一致（不会误报）。
3) 检查日志：
   - `maker_crosses.has_vamm_cross` 应随价格与方向变化而变化。
