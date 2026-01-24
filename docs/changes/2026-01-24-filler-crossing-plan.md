# Filler 交叉检测改造方案（详细）

## 目标
在 keeper/filler 中**统一检测所有交叉机会**（maker‑maker、taker‑maker、taker‑taker），并以可维护方式接入代理程序。

---

## 1) 现状：三类交叉路径

当前 filler 主要有三条“交叉相关”路径（并非只有两条）：

1. **拍卖/触发撮合交叉**
   - `dlob.find_crosses_for_auctions(...)`
   - 找到 taker ↔ maker 的 auction/trigger 撮合机会

2. **限价单交叉（uncross）**
   - `dlob.find_crossing_region(...)`
   - 寻找限价单穿价并产生撮合机会

3. **Swift order 路径**
   - Swift 订单流自身就是“taker 订单”，由 `try_swift_fill` 处理
   - 属于“事件驱动的 taker ↔ maker”

**结论**：你说的“三种交叉判断”成立，而不是仅 `find_crosses_for_auctions` 与 `find_crossing_region` 两条。

---

## 2) 为什么还漏掉“所有交叉”

上述三条路径本质仍以 **taker ↔ maker 撮合** 为前提：
- taker‑maker 是主要目标
- maker‑maker 或 taker‑taker 不会被 DLOB 视为有效撮合

---

## 3) 全交叉检测的策略选择

你提出的问题：  
> “如果我想判断所有的交叉，应该新增交叉判断的方法，还是整合为一个大的方法合适？”

### 方案对比

**方案 A：新增独立的“全交叉检测”方法（推荐）**
- 新增：`find_crosses_any(...)`
- 输入：L3 或 L2 的 top‑of‑book + makers list
- 输出：跨类型（maker‑maker / taker‑maker / taker‑taker）统一机会
- 优点：  
  - 与 DLOB 的撮合逻辑解耦  
  - 不破坏现有流程  
  - 更易调参（阈值、取 N 档、maker 数量）
- 缺点：  
  - 需要额外维护一套“交叉判断”

**方案 B：合并到现有 DLOB 交叉函数**
- 在 `find_crosses_for_auctions / find_crossing_region` 内扩展逻辑
- 优点：  
  - 单一入口  
  - 逻辑集中  
- 缺点：  
  - 破坏 DLOB 原有“撮合语义”  
  - 可能影响已有策略与测试  

### 推荐结论
选择 **方案 A**：新增独立的“全交叉检测”方法。  
理由：保持 DLOB 的撮合语义稳定，不影响当前 `try_auction_fill` / `try_uncross` / `try_swift_fill`。

---

## 4) 推荐实现结构（方案 A）

### 4.1 新增函数：`find_crosses_any`
**职责**：  
给定 L3/L2 top‑of‑book，判断是否存在任意交叉（不限 maker/taker 类型）。

**输入**（建议）：
- `best_bid_px / best_ask_px`
- `top_bid_users / top_ask_users`（最多 N 个）
- `oracle_price`（可选，仅用于过滤）

**输出**：
- `CrossIntent { bid_px, ask_px, bid_users, ask_users }`

### 4.2 接入点
- 放在 `filler.rs` 的 slot 循环中  
- 与现有撮合路径并行，不互斥  
- 若交叉成立 → 调用代理程序 `arb_perp`

### 4.3 与现有逻辑的关系
- **拍卖/uncross 路径**仍保留  
- **全交叉路径**只新增，不替换

---

## 5) 缺失 metadata 日志的详细原因与流程

### 5.1 触发链路
1. WS 更新 → `WsAccountCache::apply_user_update`
2. `DLOBNotifier::user_update`
3. DLOB 构造/更新 orderbook
4. L3 snapshot 构建时发现：  
   `orderbook` 有订单，但 `metadata` 没有对应条目
5. 触发日志：
   ```
   L3Book: Found X orders without metadata
   missing order id: ...
   ```

### 5.2 为什么会“无限重复”
- `L3Book` 每次 snapshot 构建都会扫到同一个“孤儿订单”
- 该订单一直存在于 orderbook，但 metadata 永久缺失
- 因此每次都重复打印

### 5.3 常见成因
- **事件丢失 / 顺序错乱**：`Remove` / `Insert` 事件未完整进入 metadata
- **触发单/拍卖单迁移**：订单从一个簿转移到另一个簿时 metadata 没同步
- **WS 初始同步不完整**：缓存先有订单，再补 metadata 失败

### 5.4 解决方向（建议）
1) **短期降噪**
   - 对 missing 日志限频或只打印一次  
2) **中期修复**
   - 对缺失 metadata 的 order 直接在 L3 构建时过滤掉  
   - 或者补一层“缺失 metadata 自动剔除”逻辑  
3) **长期修复**
   - 追踪该 `order_id` 的事件序列（开启 `dlob_dbg`）  
   - 找到 metadata 断裂点并修正 DLOB 更新逻辑  

---

## 6) 方案落地清单
- [ ] 新增 `find_crosses_any`（独立方法）
- [ ] 在 slot 循环中调用
- [ ] maker 列表上限与阈值参数化
- [ ] 日志缺失元数据限频/过滤
