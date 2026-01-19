# 变更说明：src/main.rs

## 目的
- 引入 WS 缓存模块，并在退出时清理 WS 订阅。

## 具体改动
- 新增 `mod ws_cache;`，注册新模块。
- Ctrl+C 处理中新增 `drift.unsubscribe().await`，优先释放 WS 订阅，再调用 `grpc_unsubscribe` 做兼容清理。

