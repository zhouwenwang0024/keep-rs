# 编译警告记录（2026-01-25）

来源：`/home/ubuntu/rust/keep-rs` 执行 `cargo build --release` 的编译输出

## drift-rs
- `CARGO_DRIFT_FFI_PATH not set`：未设置 FFI 预编译库路径，`drift-ffi-sys` 走源码编译（构建变慢但不影响功能）。
- `libdrift_ffi_sys: building from source...` / `searching for lib...`：同上。
- `Keypair::from_bytes` deprecated：`/home/ubuntu/rust/drift-rs/crates/src/utils.rs:30,38,43`。
- `ErrorNotification` 字段未读：`/home/ubuntu/rust/drift-rs/crates/src/swift_order_subscriber.rs:136`。
- `TriggerOrder::will_trigger_at` 未使用：`/home/ubuntu/rust/drift-rs/crates/src/dlob/types.rs:356`。
- `LimitOrder::is_expired` 未使用：`/home/ubuntu/rust/drift-rs/crates/src/dlob/types.rs:558`。
- `FloatingLimitOrder::is_expired` 未使用：`/home/ubuntu/rust/drift-rs/crates/src/dlob/types.rs:579`。
- `suspicious_double_ref_op`：`/home/ubuntu/rust/drift-rs/crates/src/dlob/mod.rs:667-668`，`bids.peek()?.clone()` / `asks.peek()?.clone()` 只克隆引用。

## keep-rs
- `unreachable_code`：`/home/ubuntu/rust/keep-rs/src/filler.rs:423`，`loop { tokio::select! { ... } }` 后的清理逻辑不可达。
- `unused variable`：`/home/ubuntu/rust/keep-rs/src/liquidator.rs`（`age_slots`、`market`、`high_risk_count`、`t0`、`newly_high_risk` 等）。
- `unused_mut`：`/home/ubuntu/rust/keep-rs/src/liquidator.rs:1162`。
- `private_interfaces`：`/home/ubuntu/rust/keep-rs/src/filler.rs:569`，`setup_ws` 返回私有类型 `WsSubscriptions`。
- `dead_code`：
  - `filler.rs` 中 `WsSubscriptions` 字段未读。
  - `liquidator.rs` 中 `WsSubscriptions` 字段未读。
  - `tx_worker.rs`：`TxWork::Send.ts` 未读、`Confirm` 未构造、`confirm_tx` 未使用。
  - `util.rs`：`OrderSlotLimiter::check_event` 未使用，`TxIntent` 多个字段未读/变体未构造，`PendingTxMeta.ts` 未读。

> 以上为警告，不影响本次编译通过。