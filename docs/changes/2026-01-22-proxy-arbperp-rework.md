# 2026-01-22 代理程序 `arb_perp` 优化方案（仅代理程序改动）

> **目标**：优化代理程序的 `arb_perp` 指令，采用更科学的价格交叉判断逻辑，并基于 Solana 堆分配器特性优化流程结构，充分利用引用和零拷贝技术。  
> **范围**：仅修改代理程序，不改变链下程序（keep-rs）。  
> **核心原则**：以结果为导向，结合 Solana bump allocator 特性，使用引用和零拷贝，最小化堆分配，最大化堆回滚利用。

---

## 一、执行摘要

### Solana 堆分配器特性
- **Bump allocator**：从高地址向低地址分配，不支持逐块释放
- **堆回滚机制**：通过 `heap_reset_to(pos)` 回滚到之前的堆位置
- **堆大小限制**：默认 32KB，可通过 `RequestHeapFrame` 扩展
- **栈空间限制**：栈空间也很小，不能使用大数组
- **关键优化点**：使用引用 `&[T]` 和零拷贝，避免栈和堆分配

### 当前问题
- ⚠️ **价格交叉判断不科学**：仅考虑用户订单价格，未整合 VAMM 价格
- ⚠️ **堆分配浪费**：
  - `bids_scan` 和 `asks_scan` 是 `Vec<MakerLevel>`，需要堆分配
  - 每条腿都重新构造 `remaining_accounts`（`build_min_remaining_accounts_for_level`）
  - 两条腿的账户列表有大量重复，但没有复用
- ⚠️ **未充分利用引用和零拷贝**：复制数据而不是使用引用

### 优化方案
1. **科学的价格交叉判断**：整合用户订单价格和 VAMM 价格，使用 `amm_wants_to_jit_make` 判断
2. **优化流程结构**：
   - 使用引用 `&[MakerLevel]` 而不是 `Vec<MakerLevel>`
   - 直接从 `ctx.remaining_accounts` 引用账户，零拷贝
   - 预先计算公共账户索引，每条腿复用
   - 充分利用堆回滚，在关键节点回滚堆

---

## 二、Solana 堆分配器特性分析

### 2.1 Bump Allocator 工作原理

**堆分配器实现**：

```1:146:D:\RUST\proxy\jit-proxy\src\heap.rs
#![allow(dead_code)]

use core::alloc::{GlobalAlloc, Layout};
use core::mem::size_of;
use core::ptr::{null_mut, read_volatile, write_volatile};

#[cfg(target_os = "solana")]
use solana_program::entrypoint::{HEAP_LENGTH, HEAP_START_ADDRESS};

/// BPF 目标下的全局 bump 分配器
pub struct BumpAllocator;

impl BumpAllocator {
    const RESERVED: usize = size_of::<usize>(); // 堆底保留用来存游标

    #[inline]
    #[cfg(target_os = "solana")]
    unsafe fn pos_ptr() -> *mut usize {
        HEAP_START_ADDRESS as *mut usize
    }

    /// 获取当前游标（未初始化时可能为 0）
    #[inline]
    pub fn heap_pos() -> usize {
        #[cfg(target_os = "solana")]
        unsafe {
            read_volatile(Self::pos_ptr())
        }
        #[cfg(not(target_os = "solana"))]
        {
            0
        }
    }

    ///（危险）移动游标
    #[inline]
    pub unsafe fn heap_move_cursor(pos: usize) {
        #[cfg(target_os = "solana")]
        {
            write_volatile(Self::pos_ptr(), pos);
        }
    }

    /// 未初始化（=0）时返回初始游标（堆顶）
    #[inline]
    fn pos_or_initial() -> usize {
        #[cfg(target_os = "solana")]
        {
            let p = Self::heap_pos();
            if p == 0 {
                HEAP_START_ADDRESS as usize + HEAP_LENGTH
            } else {
                p
            }
        }
        #[cfg(not(target_os = "solana"))]
        {
            0
        }
    }

    /// 可用余量估计（字节）
    #[inline]
    pub fn heap_headroom() -> usize {
        #[cfg(target_os = "solana")]
        {
            let pos = Self::pos_or_initial();
            pos.saturating_sub(HEAP_START_ADDRESS as usize + Self::RESERVED)
        }
        #[cfg(not(target_os = "solana"))]
        {
            usize::MAX
        }
    }
}

unsafe impl GlobalAlloc for BumpAllocator {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        #[cfg(not(target_os = "solana")]
        {
            // 宿主环境不启用此分配器
            return null_mut();
        }

        #[cfg(target_os = "solana")]
        {
            // 处理 size==0：按 1 字节分配，避免上层出现不一致行为
            let size = core::cmp::max(layout.size(), 1);
            let align_mask = layout.align().wrapping_sub(1);

            // 读取当前游标（或初始堆顶）
            let mut pos = Self::pos_or_initial();

            // 下移 + 向下对齐
            pos = pos.saturating_sub(size);
            pos &= !align_mask;

            // 边界：不得越过保留区
            let bottom = HEAP_START_ADDRESS as usize + Self::RESERVED;
            if pos < bottom {
                return null_mut();
            }

            // 写回游标（volatile）
            Self::heap_move_cursor(pos);

            pos as *mut u8
        }
    }

    #[inline]
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // bump 分配器不逐块释放；需要时用 heap_move_cursor 回滚
    }
}

#[cfg(target_os = "solana")]
#[global_allocator]
static GLOBAL_ALLOC: BumpAllocator = BumpAllocator;

/// ===== 对外便捷函数 =====

#[inline]
pub fn heap_pos() -> usize {
    BumpAllocator::heap_pos()
}

#[inline]
pub fn heap_headroom() -> usize {
    BumpAllocator::heap_headroom()
}

/// （危险）把游标重置到 pos（可用于 CPI 前快照，失败后回滚）
#[inline]
pub unsafe fn heap_reset_to(pos: usize) {
    BumpAllocator::heap_move_cursor(pos);
}
```

**关键特性**：
- 从高地址向低地址分配（`pos = pos.saturating_sub(size)`）
- 不支持逐块释放（`dealloc` 为空）
- 只能通过 `heap_reset_to` 回滚到之前的堆位置

### 2.2 引用和零拷贝的使用模式

**参考 `new_jit_ra_min.rs` 的实现**：

```345:367:D:\RUST\proxy\jit-proxy\src\instructions\new_jit_ra_min.rs
fn pair_user_then_stats_fast<'info>(
    ais: &'info [AccountInfo<'info>],
    user_key: &Pubkey,
    stats_key_hint: Option<Pubkey>,
) -> Option<(&'info AccountInfo<'info>, &'info AccountInfo<'info>)> {
    // 先找 User（只按 pubkey，对 AccountInfo 不做 load）
    let idx = ais.iter().position(|ai| ai.key == user_key)?;
    let user_ai = &ais[idx];

    // 优先使用相邻账户
    if let Some(stats_ai) = ais.get(idx + 1) {
        return Some((user_ai, stats_ai));
    }

    // 备用：使用 stats pubkey hint
    if let Some(sk) = stats_key_hint {
        if let Some(stats_ai) = ais.iter().find(|ai| ai.key == &sk) {
            return Some((user_ai, stats_ai));
        }
    }

    None
}
```

**关键点**：
- 直接返回 `&'info AccountInfo<'info>` 引用，零拷贝
- 不调用 `load()`，只按 `pubkey` 查找
- `maker_pairs: Vec<(&'info AccountInfo<'info>, &'info AccountInfo<'info>)>` 存储引用

**参考 `orders_ext.rs` 的实现**：

```126:133:D:\RUST\proxy\jit-proxy\src\orders_ext.rs
                    if bid_p1.map_or(true, |p| limit_px > p) {
                        // 新的一档更高：把旧的一档整体下沉为二档（swap 复用容量，零分配）
                        bid_p2 = bid_p1;
                        core::mem::swap(&mut bids1, &mut bids2);
                        bid_p1 = Some(limit_px);
                        bids1.clear();           // 仅重置 len，不动 capacity
                        push_bid(&mut bids1);    // 在原二档缓冲区里写入新一档
                    }
```

**关键点**：
- 使用 `core::mem::swap` 复用 Vec 容量，零分配
- `clear()` 只重置 `len`，不释放 `capacity`

---

## 三、优化方案：科学的价格交叉判断 + 引用和零拷贝

### 3.1 核心设计思路

**设计原则**：
1. **使用引用而不是拷贝**：`&[MakerLevel]` 而不是 `Vec<MakerLevel>`
2. **零拷贝账户查找**：直接从 `ctx.remaining_accounts` 引用，不复制
3. **最小化堆分配**：只在必要时分配，立即使用后回滚
4. **最大化数据复用**：两条腿共享公共账户索引，只添加特有账户
5. **充分利用堆回滚**：在关键节点回滚，释放临时分配

**流程设计**：

```
1. 扫描阶段（heap_snap_scan）
   - load_user_maps（堆分配）
   - find_bids_and_asks_from_users_with_auctions（堆分配，返回 Vec<MakerLevel>）
   - 提取关键信息（价格、maker 信息）到栈上的小结构
   - 收集所有需要的市场索引（栈上的小 Vec）
   - heap_reset_to(heap_snap_scan) ← 回滚堆，释放临时 Vec

2. 规划阶段（heap_snap_plan）
   - 获取 VAMM 价格
   - 判断价格交叉（使用栈上的价格信息）
   - 判断 AMM 撮合意愿
   - 确定交易对手（使用引用，不重新分配）
   - heap_reset_to(heap_snap_plan) ← 回滚堆（如果有临时分配）

3. 预先构造公共账户列表（base_ra）
   - 调用 build_base_remaining_accounts 构造公共账户列表（两条腿共享）
   - ⚠️ **关键风险点**：base_ra 是 Vec<AccountInfo>，其 buffer 在堆上
   - ⚠️ **不能在此处 reset 堆**：如果 reset，base_ra 的 buffer 可能被后续分配覆盖，导致悬空指针
   - base_ra 在后续执行阶段仍需要使用（400、438、458、496、555 行）
   - 因此：**不在此处 reset，让 base_ra 保留到函数结束**

4. 执行阶段（每条腿）
   - 复用公共账户索引（从栈上的数组读取）
   - 添加本条腿特有的账户索引
   - 从 ctx.remaining_accounts 中按索引引用账户（零拷贝）
   - 执行 CPI
   - heap_reset_to(heap_snap_leg) ← 回滚堆
```

### 3.2 科学的价格交叉判断

**新的价格交叉判断流程**：

1. **收集所有买单价格**：
   - 扫描所有用户订单，找出买单的最高价
   - 获取 VAMM 的买单最高价（通过 `perp_market.bid_price(None)` 获取，参考 `filler.rs:223`）
   - 取两者中的最大值作为 `best_bid_price`

2. **收集所有卖单价格**：
   - 扫描所有用户订单，找出卖单的最低价
   - 获取 VAMM 的卖单价（通过 `perp_market.ask_price(None)` 获取，参考 `filler.rs:221`）
   - 取两者中的最小值作为 `best_ask_price`

3. **判断 AMM 撮合意愿**：
   - 对于买单方向：调用 `amm_wants_to_jit_make(PositionDirection::Long)` 判断 AMM 是否愿意作为 maker
   - 对于卖单方向：调用 `amm_wants_to_jit_make(PositionDirection::Short)` 判断 AMM 是否愿意作为 maker

4. **判断价格交叉**：
   - 如果 `best_bid_price >= best_ask_price`，则存在交叉
   - 如果存在交叉，且 AMM 愿意撮合，则构造两次 CPI 吃掉两边

---

## 四、具体实现方案

### 4.1 优化扫描阶段（提前判断、复用引用、高效去重）

**优化要点**：

1. **提前判断价格交叉**：在扫描阶段内就判断，如果不存在交叉，提前返回，避免后续计算
2. **复用 perp_market 引用**：在扫描阶段获取后，在扫描阶段内就判断 AMM 撮合意愿，避免重复获取
3. **使用 BTreeSet 优化去重**：替代 `Vec::contains`（O(n)），使用 `BTreeSet`（O(log n)）
4. **同步收集 maker 市场**：在扫描阶段就收集所有需要的市场索引

**优化后的逻辑**（提取关键信息，使用引用，提前判断）：

```rust
#[cfg(target_os = "solana")]
let heap_snap_scan = crate::heap::heap_pos();

// 扫描阶段：收集订单和市场索引
let (user_bids_ref, user_asks_ref, required_markets) = {
    let (makers_map, _) = load_user_maps(&mut rem_iter, true)?;
    
    // 扫描用户订单（堆分配）
    let (bids_tmp, asks_tmp) = {
        let pm_ref = perp_market_map.get_ref(&market_index)?;
        let pm: &drift::state::perp_market::PerpMarket = &*pm_ref;
        find_bids_and_asks_from_users_with_auctions(
            pm,
            &oracle_px_val,
            &makers_map,
            slot,
            now,
        )?
    };
    
    // 提取关键信息到栈上的小结构（避免保留整个 Vec）
    // 只保留价格、maker_user、taker_order_id 等必要信息
    struct OrderInfo {
        price: u64,
        base_asset_amount: u64,
        maker_user: Pubkey,
        taker_order_id: u32,
        is_auctioning: bool,
    }
    
    // 提取 bid 信息（栈上的小数组）
    let mut bid_infos: [OrderInfo; 16] = [/* 初始化 */];
    let mut bid_len = 0;
    for (i, m) in bids_tmp.iter().enumerate().take(16) {
        bid_infos[i] = OrderInfo {
            price: m.price,
            base_asset_amount: m.base_asset_amount,
            maker_user: m.maker_user,
            taker_order_id: m.taker_order_id,
            is_auctioning: m.is_auctioning,
        };
        bid_len += 1;
    }
    
    // 提取 ask 信息（栈上的小数组）
    let mut ask_infos: [OrderInfo; 16] = [/* 初始化 */];
    let mut ask_len = 0;
    for (i, m) in asks_tmp.iter().enumerate().take(16) {
        ask_infos[i] = OrderInfo {
            price: m.price,
            base_asset_amount: m.base_asset_amount,
            maker_user: m.maker_user,
            taker_order_id: m.taker_order_id,
            is_auctioning: m.is_auctioning,
        };
        ask_len += 1;
    }
    
    // 收集所有需要的市场索引（栈上的小 Vec，在作用域结束后自动释放）
    let mut perp_idxs = Vec::<u16>::with_capacity(8);
    let mut spot_idxs = Vec::<u16>::with_capacity(8);
    
    // 目标 perp 市场（必需，writable）
    push_unique_u16(&mut perp_idxs, market_index);
    
    // Quote spot 市场（必需，readable）
    let quote_spot_index = spot_market_map.get_quote_spot_market()?.market_index;
    push_unique_u16(&mut spot_idxs, quote_spot_index);
    
    // 我方活跃市场
    let taker = ctx.accounts.user.load()?;
    for p in taker.perp_positions.iter().filter(|p| !p.is_available()) {
        push_unique_u16(&mut perp_idxs, p.market_index);
    }
    for p in taker.spot_positions.iter().filter(|p| !p.is_available()) {
        push_unique_u16(&mut spot_idxs, p.market_index);
    }
    
    // 所有 maker 的活跃市场（从 bids_tmp 和 asks_tmp 中获取 maker_user，然后查找）
    let mut seen_makers = Vec::<Pubkey>::with_capacity(16);
    for m in bids_tmp.iter().chain(asks_tmp.iter()) {
        if seen_makers.contains(&m.maker_user) {
            continue;
        }
        seen_makers.push(m.maker_user);
        
        // 从 ctx.remaining_accounts 中查找 maker User 账户（零拷贝）
        if let Some(maker_user_ai) = find_account_by_pubkey(&ctx.remaining_accounts, &m.maker_user) {
            if let Ok(loader) = AccountLoader::<User>::try_from(maker_user_ai) {
                if let Ok(maker_user) = loader.load() {
                    for p in maker_user.perp_positions.iter().filter(|p| !p.is_available()) {
                        push_unique_u16(&mut perp_idxs, p.market_index);
                    }
                    for p in maker_user.spot_positions.iter().filter(|p| !p.is_available()) {
                        push_unique_u16(&mut spot_idxs, p.market_index);
                    }
                }
            }
        }
    }
    
    // 将 Vec 转换为固定大小数组（栈上）
    let mut perp_idxs_arr: [u16; 32] = [0; 32];
    let mut spot_idxs_arr: [u16; 32] = [0; 32];
    let perp_len = perp_idxs.len().min(32);
    let spot_len = spot_idxs.len().min(32);
    perp_idxs_arr[..perp_len].copy_from_slice(&perp_idxs[..perp_len]);
    spot_idxs_arr[..spot_len].copy_from_slice(&spot_idxs[..spot_len]);
    
    // 返回引用和数组（栈上的数据）
    (
        &bid_infos[..bid_len],
        &ask_infos[..ask_len],
        (perp_idxs_arr, spot_idxs_arr, perp_len, spot_len),
    )
};

#[cfg(target_os = "solana")]
unsafe {
    crate::heap::heap_reset_to(heap_snap_scan);
}

// 注意：user_bids_ref 和 user_asks_ref 现在指向栈上的数据，可以安全使用
```

**关键优化**：
- 提取关键信息到栈上的小结构（`OrderInfo`），避免保留整个 `Vec<MakerLevel>`
- 使用栈上的小数组（`[OrderInfo; 16]`），避免堆分配
- 扫描完成后立即回滚堆，释放临时 `Vec`
- 使用引用 `&[OrderInfo]` 而不是 `Vec<OrderInfo>`

### 4.2 优化规划阶段（复用扫描结果）

**注意**：价格交叉判断和 AMM 撮合意愿判断已在扫描阶段完成，这里只需要确定交易对手：

```rust
// 路由判定：确定交易对手（使用栈上的数据）
// 注意：best_bid_price、best_ask_price、amm_wants_to_make_on_bid、amm_wants_to_make_on_ask
// 已在扫描阶段计算完成，直接使用
let user_bids_ref = &best_bid_orders[..best_bid_count];
let user_asks_ref = &best_ask_orders[..best_ask_count];

let has_non_auc_bid = user_bids_ref.iter().any(|m| !m.is_auctioning);
let has_non_auc_ask = user_asks_ref.iter().any(|m| !m.is_auctioning);

let sel_bid = if !has_non_auc_bid {
    // 只有拍卖订单，选择最大的
    let best_auc = user_bids_ref
        .iter()
        .filter(|m| m.is_auctioning)
        .max_by_key(|m| m.base_asset_amount)
        .copied()
        .ok_or(ErrorCode::NoBestBid)?;
    SelectedSide::Make(best_auc)
} else if best_bid_price == vamm_bid_price && amm_wants_to_make_on_bid {
    // VAMM 价格最高且愿意撮合
    SelectedSide::Vamm { price: vamm_bid_price }
} else {
    // 使用用户订单
    SelectedSide::Take { order_infos: user_bids_ref }
};

let sel_ask = if !has_non_auc_ask {
    // 只有拍卖订单，选择最大的
    let best_auc = user_asks_ref
        .iter()
        .filter(|m| m.is_auctioning)
        .max_by_key(|m| m.base_asset_amount)
        .copied()
        .ok_or(ErrorCode::NoBestAsk)?;
    SelectedSide::Make(best_auc)
} else if best_ask_price == vamm_ask_price && amm_wants_to_make_on_ask {
    // VAMM 价格最低且愿意撮合
    SelectedSide::Vamm { price: vamm_ask_price }
} else {
    // 使用用户订单
    SelectedSide::Take { order_infos: user_asks_ref }
};
```

**关键优化**：
- ✅ **复用扫描结果**：价格交叉判断和 AMM 撮合意愿已在扫描阶段完成，避免重复计算
- ✅ **使用栈上数据**：所有数据都在栈上，无需堆分配

### 4.3 预先构造公共账户列表（符合 SDK 逻辑）

**关键点**：
1. CPI 会消耗 AccountInfo 的所有权，必须 clone，不能只使用引用
2. 账户顺序必须符合 SDK：oracles → spot → perp
3. 优先从 loader/map 获取账户，更高效
4. 复用 SDK 的辅助函数：`get_market_set_for_user_positions`、`get_market_set_for_spot_positions`

**参考 SDK 的实现**：

```4242:4311:D:\RUST\drift-rs\crates\src\lib.rs
pub fn build_accounts<'a>(
    program_data: &ProgramData,
    base_accounts: impl ToAccountMetas,
    users: impl Iterator<Item = &'a User>,
    markets_readable: impl Iterator<Item = &'a MarketId>,
    markets_writable: impl Iterator<Item = &'a MarketId>,
) -> Vec<AccountMeta> {
    // the order of accounts returned must be instruction, oracles, spot, perps
    let mut accounts = BTreeSet::<RemainingAccount>::new();
    // ... 使用 BTreeSet 去重和排序
}
```

**优化后的逻辑**：预先构造公共账户列表，两条腿复用，符合 SDK 逻辑：

```rust
// 优化：按照 SDK 和 new_jit_ra_min.rs 的逻辑构造 base_ra
// 顺序：oracles → spot → perp（与 SDK 一致）
let base_ra = {
    let mut ra = Vec::<AccountInfo>::new();
    let mut oracle_keys = Vec::<Pubkey>::with_capacity(16);
    
    // 1. 从 perp / spot 索引集合推导 oracle pubkey（复用 new_jit_ra_min.rs 的逻辑）
    for &idx in plan.required_markets.0[..plan.required_markets.2].iter() {
        if let Ok(pm) = perp_market_map.get_ref(&idx) {
            push_unique_pubkey(&mut oracle_keys, pm.amm.oracle);
        }
    }
    for &idx in plan.required_markets.1[..plan.required_markets.3].iter() {
        if let Ok(sm) = spot_market_map.get_ref(&idx) {
            push_unique_pubkey(&mut oracle_keys, sm.oracle);
        }
    }
    
    // 2. oracle accounts（优先从 oracle_map 获取，与 new_jit_ra_min.rs 一致）
    for ok in oracle_keys.iter() {
        // 优先从 oracle_map 获取（更高效）
        if let Ok(o_ai) = oracle_map.get_account_info(ok) {
            push_ai_if_absent(&mut ra, &o_ai);
        } else if let Some(ai) = ctx.remaining_accounts.iter().find(|ai| ai.key == *ok) {
            // 如果 oracle_map 中没有，再从 remaining_accounts 查找
            push_ai_if_absent(&mut ra, ai);
        }
    }
    
    // 3. spot markets（优先从 loader 获取，与 new_jit_ra_min.rs 一致）
    for &idx in plan.required_markets.1[..plan.required_markets.3].iter() {
        if let Some(loader) = spot_market_map.0.get(&idx) {
            push_ai_if_absent(&mut ra, &loader.to_account_info());
        } else if let Ok(sm) = spot_market_map.get_ref(&idx) {
            // 如果 loader 中没有，再从 remaining_accounts 查找
            if let Some(ai) = ctx.remaining_accounts.iter().find(|ai| ai.key == sm.pubkey) {
                push_ai_if_absent(&mut ra, ai);
            }
        }
    }
    
    // 4. perp markets（优先从 loader 获取，与 new_jit_ra_min.rs 一致）
    for &idx in plan.required_markets.0[..plan.required_markets.2].iter() {
        if let Some(loader) = perp_market_map.0.get(&idx) {
            push_ai_if_absent(&mut ra, &loader.to_account_info());
        } else if let Ok(pm) = perp_market_map.get_ref(&idx) {
            // 如果 loader 中没有，再从 remaining_accounts 查找
            if let Some(ai) = ctx.remaining_accounts.iter().find(|ai| ai.key == pm.pubkey) {
                push_ai_if_absent(&mut ra, ai);
            }
        }
    }
    
    ra
};
```

**关键优化**：
- ✅ **符合 SDK 顺序**：oracles → spot → perp（与 SDK 的 `build_accounts` 一致）
- ✅ **优先从 loader/map 获取**：与 `new_jit_ra_min.rs` 一致，更高效
- ✅ **复用 SDK 辅助函数**：使用 `get_market_set_for_user_positions` 和 `get_market_set_for_spot_positions`
- ✅ **账户去重**：使用 `push_ai_if_absent` 确保不重复添加

**关键点**：
- **必须 clone AccountInfo**：因为 CPI 会消耗 AccountInfo 的所有权，不能只使用引用
- **AccountInfo::clone() 是轻量级的**：只复制元数据（指针、标志等），不复制实际账户数据
- **预先构造公共账户列表**：两条腿可以复用，避免重复查找和 clone
- **使用 `push_ai_if_absent` 去重**：避免重复 clone 同一个账户

### 4.4 优化执行阶段（复用公共账户列表）

**每条腿复用公共账户列表，只添加特有账户**：

```rust
// 执行第一腿
#[cfg(target_os = "solana")]
let heap_snap_leg1 = crate::heap::heap_pos();

let leg1_ra = {
    let mut ra = Vec::<AccountInfo>::new();
    
    // 复用公共账户列表（从预先构造的 base_ra 中 clone）
    // 注意：虽然 base_ra 已经在堆上，但我们可以通过 extend 来复用
    // 或者更优：直接复用 base_ra，只添加特有账户
    ra.extend_from_slice(&base_ra);
    
    // 添加第一腿特有的账户
    match &ask_side {
        SelectedSide::Take { order_infos } => {
            // 添加 maker 账户（去重）
            let mut seen_makers = Vec::<Pubkey>::with_capacity(8);
            for order_info in order_infos.iter() {
                if seen_makers.contains(&order_info.maker_user) {
                    continue;
                }
                seen_makers.push(order_info.maker_user);
                
                // 从 ctx.remaining_accounts 中查找并 clone（必须 clone）
                if let Some((maker_user_ai, maker_stats_ai)) = 
                    pair_user_then_stats_fast(&ctx.remaining_accounts, &order_info.maker_user, None)
                {
                    push_ai_if_absent(&mut ra, maker_user_ai);
                    push_ai_if_absent(&mut ra, maker_stats_ai);
                }
            }
        }
        SelectedSide::Vamm { .. } => {
            // 与 AMM 成交，不需要额外的 maker 账户
        }
        _ => {}
    }
    
    ra
};

// 执行 CPI
exec_leg_with_ra(&leg1_ra, &ask_side, ...)?;

#[cfg(target_os = "solana")]
unsafe {
    crate::heap::heap_reset_to(heap_snap_leg1);
}

// 执行第二腿（复用公共账户列表）
#[cfg(target_os = "solana")]
let heap_snap_leg2 = crate::heap::heap_pos();

let leg2_ra = {
    let mut ra = Vec::<AccountInfo>::new();
    
    // 复用公共账户列表（从预先构造的 base_ra 中 clone）
    ra.extend_from_slice(&base_ra);
    
    // 添加第二腿特有的账户
    match &bid_side {
        SelectedSide::Take { order_infos } => {
            // 添加 maker 账户（去重）
            let mut seen_makers = Vec::<Pubkey>::with_capacity(8);
            for order_info in order_infos.iter() {
                if seen_makers.contains(&order_info.maker_user) {
                    continue;
                }
                seen_makers.push(order_info.maker_user);
                
                // 从 ctx.remaining_accounts 中查找并 clone（必须 clone）
                if let Some((maker_user_ai, maker_stats_ai)) = 
                    pair_user_then_stats_fast(&ctx.remaining_accounts, &order_info.maker_user, None)
                {
                    push_ai_if_absent(&mut ra, maker_user_ai);
                    push_ai_if_absent(&mut ra, maker_stats_ai);
                }
            }
        }
        SelectedSide::Vamm { .. } => {
            // 与 AMM 成交，不需要额外的 maker 账户
        }
        _ => {}
    }
    
    ra
};

// 执行 CPI
exec_leg_with_ra(&leg2_ra, &bid_side, ...)?;

#[cfg(target_os = "solana")]
unsafe {
    crate::heap::heap_reset_to(heap_snap_leg2);
}
```

**关键优化**：
- **复用公共账户列表**：两条腿共享 `base_ra`，通过 `extend_from_slice` 复用
- **必须 clone AccountInfo**：CPI 会消耗 AccountInfo 的所有权，不能只使用引用
- **AccountInfo::clone() 是轻量级的**：只复制元数据（指针、标志等），不复制实际账户数据
- **每条腿只添加特有账户**：避免重复构造公共账户列表

**进一步优化**（如果堆空间允许）：
- 可以预先构造完整的 `base_ra`，然后在每条腿执行时直接复用，只添加特有账户
- 或者：使用 `Vec::with_capacity` 预分配容量，减少重新分配

### 4.5 修改枚举定义（使用引用）

**当前枚举**：

```36:39:D:\RUST\proxy\jit-proxy\src\instructions\arb_perp.rs
enum SelectedSide {
    Take { makers: Vec<MakerLevel> }, // 同价可多 maker
    Make(MakerLevel),                 // 单对手（拍卖）
}
```

**优化后的枚举**（使用引用和轻量级结构）：

```rust
// 轻量级订单信息（栈上）
#[derive(Clone, Copy)]
struct OrderInfo {
    price: u64,
    base_asset_amount: u64,
    maker_user: Pubkey,
    taker_order_id: u32,
    is_auctioning: bool,
}

enum SelectedSide<'a> {
    Take { order_infos: &'a [OrderInfo] }, // 使用引用，避免堆分配
    Make(OrderInfo),                       // 单对手（拍卖），栈上拷贝
    Vamm { price: i64 },                   // 与 AMM 成交（新增）
}
```

**关键优化**：
- `SelectedSide::Take` 使用 `&'a [OrderInfo]` 引用，不分配堆
- `OrderInfo` 是 `Copy` 类型，栈上拷贝成本低
- `SelectedSide::Make` 直接存储 `OrderInfo`（栈上）

---

## 五、关键实现细节

### 5.1 零拷贝账户查找

**参考 `new_jit_ra_min.rs` 的实现**：

```345:367:D:\RUST\proxy\jit-proxy\src\instructions\new_jit_ra_min.rs
fn pair_user_then_stats_fast<'info>(
    ais: &'info [AccountInfo<'info>],
    user_key: &Pubkey,
    stats_key_hint: Option<Pubkey>,
) -> Option<(&'info AccountInfo<'info>, &'info AccountInfo<'info>)> {
    // 先找 User（只按 pubkey，对 AccountInfo 不做 load）
    let idx = ais.iter().position(|ai| ai.key == user_key)?;
    let user_ai = &ais[idx];

    // 优先使用相邻账户
    if let Some(stats_ai) = ais.get(idx + 1) {
        return Some((user_ai, stats_ai));
    }

    // 备用：使用 stats pubkey hint
    if let Some(sk) = stats_key_hint {
        if let Some(stats_ai) = ais.iter().find(|ai| ai.key == &sk) {
            return Some((user_ai, stats_ai));
        }
    }

    None
}
```

**关键点**：
- 直接返回 `&'info AccountInfo<'info>` 引用，零拷贝
- 不调用 `load()`，只按 `pubkey` 查找
- 优先使用相邻账户（User → UserStats 紧邻）

### 5.2 线性去重函数（避免 BTreeSet 堆分配）

**参考 `new_jit_ra_min.rs` 的实现**：

```294:315:D:\RUST\proxy\jit-proxy\src\instructions\new_jit_ra_min.rs
/// 线性去重（避免 BTreeSet 堆分配）
#[inline(always)]
fn push_unique_u16(v: &mut Vec<u16>, x: u16) {
    if !v.iter().any(|&y| y == x) {
        v.push(x);
    }
}

#[inline(always)]
fn push_unique_pubkey(v: &mut Vec<Pubkey>, x: Pubkey) {
    if !v.iter().any(|&y| y == x) {
        v.push(x);
    }
}

/// AccountInfo 去重（按 pubkey）
#[inline(always)]
fn push_ai_if_absent<'info>(ra: &mut Vec<AccountInfo<'info>>, ai: &AccountInfo<'info>) {
    if !ra.iter().any(|x| x.key == ai.key) {
        ra.push(ai.clone());
    }
}
```

**关键点**：
- 使用线性搜索去重，避免 BTreeSet 的堆分配
- 对于小集合（市场数量、账户数量），线性搜索性能足够
- `AccountInfo::clone()` 是轻量级的（只复制指针）

### 5.3 堆回滚策略

**堆回滚时机**：

1. **扫描阶段后**：回滚 `load_user_maps` 和 `find_bids_and_asks_from_users_with_auctions` 的临时分配
2. **规划阶段后**：回滚规划阶段的临时分配（如果有）
3. **公共账户索引计算后**：回滚索引计算的临时分配
4. **每条腿执行后**：回滚本条腿的临时分配

**堆回滚模式**：

```rust
#[cfg(target_os = "solana")]
let heap_snap = crate::heap::heap_pos();

// 临时分配（在堆上）
let temp_data = /* 堆分配 */;

// 提取关键信息到栈上（零拷贝或小拷贝）
let stack_data = /* 提取到栈上 */;

#[cfg(target_os = "solana")]
unsafe {
    crate::heap::heap_reset_to(heap_snap);
}

// 后续使用栈上的数据
```

### 5.4 VAMM 价格和撮合意愿判断

**参考 keep-rs 的实现**：

```220:224:D:\RUST\keep-rs\src\filler.rs
                            let vamm_price = if order_params.direction == PositionDirection::Long {
                                perp_market.ask_price(None)
                            } else {
                                perp_market.bid_price(None)
                            };
```

**关键点**：
- VAMM 价格不是直接通过 `amm.bid_price()` 和 `amm.ask_price()` 获取的
- 而是通过 `perp_market.bid_price(None)` 和 `perp_market.ask_price(None)` 获取
- `bid_price(None)` 和 `ask_price(None)` 内部会使用 `reserve_price()` 作为默认值
- 参考 `drift-rs/crates/src/ffi.rs:770-790` 的实现

**VAMM 价格计算**（参考 `drift-rs/crates/src/ffi.rs`）：

```770:790:D:\RUST\drift-rs\crates\src\ffi.rs
    pub fn bid_price(&self, reserve_price: Option<u64>) -> u64 {
        let adjusted_spread = (-(self.amm.short_spread as i32)) + self.amm.reference_price_offset;
        let multiplier = BID_ASK_SPREAD_PRECISION_I64 + adjusted_spread as i64;
        let reserve_price = reserve_price.unwrap_or(self.reserve_price());

        (reserve_price * multiplier.unsigned_abs()) / BID_ASK_SPREAD_PRECISION_I64 as u64
    }

    /// Return AMM's ask price
    ///
    /// ## Params
    ///
    /// * `reserve_price` - optional reserve price, default: AMM current reserve price
    ///
    pub fn ask_price(&self, reserve_price: Option<u64>) -> u64 {
        let adjusted_spread = self.amm.long_spread as i32 + self.amm.reference_price_offset;
        let multiplier = BID_ASK_SPREAD_PRECISION_I64 + adjusted_spread as i64;
        let reserve_price = reserve_price.unwrap_or(self.reserve_price());

        (reserve_price * multiplier.unsigned_abs()) / BID_ASK_SPREAD_PRECISION_I64 as u64
    }
```

**AMM 撮合意愿判断**（参考 Drift 源码）：

```1476:1485:C:\Users\Administrator\Desktop\protocol-v2-master\protocol-v2-master\programs\drift\src\state\perp_market.rs
    pub fn amm_wants_to_jit_make(&self, taker_direction: PositionDirection) -> DriftResult<bool> {
        let amm_wants_to_jit_make = match taker_direction {
            PositionDirection::Long => {
                // AMM 愿意作为卖单 maker（taker 买入）
                self.amm.base_asset_amount < 0
            }
            PositionDirection::Short => {
                // AMM 愿意作为买单 maker（taker 卖出）
                self.amm.base_asset_amount > 0
            }
        };
        Ok(amm_wants_to_jit_make && self.amm_jit_is_active())
    }
```

---

## 六、完整流程设计

### 6.1 流程概览

```
arb_perp 入口
├── 1. 初始化阶段（无堆分配）
│   ├── 获取 clock、slot、now
│   ├── 加载 perp_market_map、spot_market_map、oracle_map
│   └── 获取 taker 初始状态（base_init、quote_init）
│
├── 2. 扫描阶段（heap_snap_scan）
│   ├── load_user_maps（堆分配）
│   ├── find_bids_and_asks_from_users_with_auctions（堆分配）
│   ├── 提取关键信息到栈上的 OrderInfo 数组（零拷贝）
│   ├── 收集所有需要的市场索引（栈上的小 Vec）
│   └── heap_reset_to(heap_snap_scan) ← 回滚堆
│
├── 3. 规划阶段（heap_snap_plan）
│   ├── 获取 VAMM 价格
│   ├── 判断价格交叉（使用栈上的 OrderInfo）
│   ├── 判断 AMM 撮合意愿
│   ├── 确定交易对手（使用引用 &[OrderInfo]）
│   └── heap_reset_to(heap_snap_plan) ← 回滚堆（如果有临时分配）
│
├── 4. 预先构造公共账户列表（base_ra）
│   ├── 调用 build_base_remaining_accounts 构造公共账户列表（两条腿共享）
│   ├── ⚠️ **关键风险点**：base_ra 是 Vec<AccountInfo>，其 buffer 在堆上
│   ├── ⚠️ **不能在此处 reset 堆**：如果 reset，base_ra 的 buffer 可能被后续分配覆盖，导致悬空指针
│   └── base_ra 在后续执行阶段仍需要使用（400、438、458、496、555 行），因此不在此处 reset
│
└── 5. 执行阶段
    ├── 第一腿（heap_snap_leg1）
    │   ├── 复用公共账户索引（从栈上数组读取）
    │   ├── 从 ctx.remaining_accounts 中按索引引用账户（零拷贝）
    │   ├── 添加第一腿特有的账户（零拷贝查找）
    │   ├── 执行 CPI
    │   └── heap_reset_to(heap_snap_leg1) ← 回滚堆
    │
    └── 第二腿（heap_snap_leg2）
        ├── 复用公共账户索引（从栈上数组读取）
        ├── 从 ctx.remaining_accounts 中按索引引用账户（零拷贝）
        ├── 添加第二腿特有的账户（零拷贝查找）
        ├── 执行 CPI
        └── heap_reset_to(heap_snap_leg2) ← 回滚堆
```

### 6.2 堆分配优化对比

**当前实现**：
- 扫描阶段：`load_user_maps` + `find_bids_and_asks` + `bids_scan` + `asks_scan`（堆分配）
- 每条腿：`build_min_remaining_accounts_for_level` 内部多个 Vec（堆分配）
- 总堆分配：~3-4 次大分配，峰值高

**优化后实现**：
- 扫描阶段：`load_user_maps` + `find_bids_and_asks`（堆分配，立即回滚）
- 规划阶段：使用栈上的 `OrderInfo` 数组（无堆分配）
- 公共账户列表：`base_ra`（堆分配，**不立即回滚**，见下方风险点说明）
- 每条腿：只添加特有账户（小堆分配，立即回滚）
- 总堆分配：~3-4 次，但每次分配后立即回滚，堆使用峰值大幅降低

#### 6.2.1 关键风险点：`base_ra` 的堆回滚时机

**问题描述**：

在构造 `base_ra` 后立即调用 `heap_reset_to(heap_snap_base)` 会导致严重的悬空指针问题。

**原因分析**：

1. **`heap_reset_to` 的工作原理**：
   ```rust
   pub unsafe fn heap_reset_to(pos: usize) {
       BumpAllocator::heap_move_cursor(pos);  // 只移动游标，不释放内存
   }
   ```
   - `heap_reset_to` 只是移动游标，不会真正释放内存
   - 但后续分配会从新游标位置开始，**可能覆盖之前的内存**

2. **`base_ra` 的内存布局**：
   - `base_ra` 是 `Vec<AccountInfo>`，其 buffer 在堆上
   - 如果 `base_ra` 的 buffer 位于被回退的堆区域，会被后续分配覆盖

3. **使用场景**：
   - `base_ra` 在后续执行阶段仍需要使用：
     - 400 行：`&base_ra`（第一腿 Long）
     - 438 行：`&base_ra`（第一腿 Short）
     - 458 行：`&base_ra`（第二腿 Long）
     - 496 行：`&base_ra`（第二腿 Short）
     - 555 行：`base_ra.to_vec()`（VAMM 分支）

**风险场景**：

```
时间线：
1. heap_snap_base = 0x1000 (假设)
2. build_base_remaining_accounts() 分配 Vec buffer 在 0x0F00-0x0FFF
3. base_ra 持有指向 0x0F00 的指针
4. heap_reset_to(0x1000) ← 游标回到 0x1000
5. 后续分配（如 exec_leg_min_heap 内部）从 0x1000 开始
6. 如果分配超过 0x0F00，会覆盖 base_ra 的 buffer！
7. base_ra.to_vec() 读取已被覆盖的内存 → 悬空指针/崩溃
```

**解决方案**：

**删除 `base_ra` 构造后的 `heap_reset_to`**：

```343:354:D:\RUST\proxy\jit-proxy\src\instructions\arb_perp.rs
// 优化：复用 build_base_remaining_accounts 构造公共账户列表（两条腿共享）
// 顺序：oracles → spot → perp（与 SDK 一致）
// ⚠️ 注意：base_ra 在后续执行阶段仍需要使用，不能在此处 reset 堆
// 如果 reset，base_ra 的 buffer 可能被后续分配覆盖，导致悬空指针
let base_ra = build_base_remaining_accounts(
    &ctx.remaining_accounts,
    &perp_market_map,
    &spot_market_map,
    &mut oracle_map,
    &plan.required_markets.0[..plan.required_markets.2],
    &plan.required_markets.1[..plan.required_markets.3],
)?;
```

**理由**：
1. `base_ra` 是必需的长期数据，不应被回滚
2. 函数结束时 `base_ra` 会被 drop，内存自然回收
3. 堆回滚的目的是释放临时分配，而不是长期数据

**关键原则**：

> **只有在确定不再使用某个堆分配的数据后，才能对该数据之前的堆位置进行回滚。**

### 6.3 引用和零拷贝的优势

**使用引用 `&[T]`**：
- 不分配堆，不占用栈（只存储指针和长度）
- 可以直接引用 `ctx.remaining_accounts` 中的数据
- 生命周期由 Rust 编译器保证

**账户查找和 clone**：
- `pair_user_then_stats_fast` 直接返回 `&AccountInfo` 引用（用于查找）
- 不调用 `load()`，只按 `pubkey` 查找
- **必须 clone AccountInfo**：因为 CPI 会消耗 AccountInfo 的所有权
- `AccountInfo::clone()` 是轻量级的（只复制元数据：指针、标志等，不复制实际账户数据）

**公共账户列表复用**：
- 预先构造公共账户列表（`base_ra`），两条腿共享
- 通过 `extend_from_slice` 复用，避免重复查找和 clone
- 每条腿只添加特有账户，最小化堆分配

---

## 七、风险与验证清单

1. **生命周期管理**
   - ⚠️ 需要确保引用的生命周期正确
   - ⚠️ `OrderInfo` 数组的生命周期需要覆盖整个执行过程

2. **账户列表顺序**
   - ⚠️ 需要确保账户列表的顺序与 Drift 的 `optional_accounts` 解析顺序一致
   - ⚠️ 参考 SDK 的顺序：oracles → spot → perp → users

3. **AccountInfo clone 的必要性**
   - ⚠️ CPI 会消耗 AccountInfo 的所有权，必须 clone，不能只使用引用
   - ⚠️ `AccountInfo::clone()` 是轻量级的，但仍有开销
   - ⚠️ 需要确保每条腿都有独立的 AccountInfo 副本

4. **索引查找性能**
   - ⚠️ `ctx.remaining_accounts.iter().find()` 是 O(N) 查找
   - ⚠️ 如果 `remaining_accounts` 很大，可能需要优化（但通常不会太大）
   - ⚠️ 可以通过预先构造索引映射来优化（但会增加复杂度）

4. **栈空间限制**
   - ⚠️ 固定大小数组（`[OrderInfo; 16]`、`[usize; 64]`）需要确保不会溢出栈
   - ⚠️ 如果数据量超过上限，需要处理

---

## 八、输出物清单

- `proxy/jit-proxy/src/instructions/arb_perp.rs`：优化流程结构，使用引用和零拷贝
- `proxy/jit-proxy/src/instructions/arb_perp.rs`：修改价格交叉判断逻辑，整合 VAMM 价格
- `proxy/jit-proxy/src/instructions/arb_perp.rs`：新增 `SelectedSide::Vamm` 枚举
- `proxy/jit-proxy/src/instructions/arb_perp.rs`：优化执行阶段，使用索引数组和零拷贝引用

---

## 九、参考代码位置

### 9.1 Solana 堆分配器

- `heap.rs`：Bump allocator 实现
- `heap_pos()`：获取当前堆位置
- `heap_reset_to()`：回滚堆到指定位置

### 9.2 引用和零拷贝参考

- `new_jit_ra_min.rs::pair_user_then_stats_fast`：零拷贝账户查找
- `new_jit_ra_min.rs::maker_pairs`：存储引用 `Vec<(&AccountInfo, &AccountInfo)>`
- `orders_ext.rs::core::mem::swap`：复用 Vec 容量，零分配

### 9.3 SDK 设计参考

- `build_remaining_accounts_for_proxy`：链下预先计算账户列表
- `build_accounts`：使用 `BTreeSet` 去重（链下，可以使用）

### 9.4 keep-rs 参考

- `filler.rs:220-224`：VAMM 价格的获取方式（`perp_market.bid_price(None)` 和 `perp_market.ask_price(None)`）
- `filler.rs:251`：`find_crosses_for_taker_order` 的使用方式
- `filler.rs:258-259`：`has_vamm_cross` 标志的使用

### 9.5 Drift 源码

- `amm_wants_to_jit_make`：判断 AMM 是否愿意撮合
- `perp_market.bid_price(None)`：获取 VAMM 买单最高价（参考 `drift-rs/crates/src/ffi.rs:770`）
- `perp_market.ask_price(None)`：获取 VAMM 卖单最低价（参考 `drift-rs/crates/src/ffi.rs:784`）
