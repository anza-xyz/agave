# Solana (Agave) Priority Fee 源码分析 - 完整链路追踪

> **分析版本**: Agave commit `bc45720`
> **分析日期**: 2025-11-20
> **文档**: 第1部分 - 交易生命周期与完整链路

---

## 目录

1. [交易生命周期完整视图](#交易生命周期完整视图)
2. [阶段一: 交易接收与验证](#阶段一-交易接收与验证)
3. [阶段二: Priority Fee 计算](#阶段二-priority-fee-计算)
4. [阶段三: 交易调度与排序](#阶段三-交易调度与排序)
5. [阶段四: 区块打包与执行](#阶段四-区块打包与执行)
6. [阶段五: Fee 缓存与查询](#阶段五-fee-缓存与查询)

---

## 交易生命周期完整视图

```
┌─────────────────────────────────────────────────────────────────────┐
│                     交易从提交到上链的完整流程                          │
└─────────────────────────────────────────────────────────────────────┘

[Client]
   │ 提交交易 (含 compute_unit_price)
   ↓
┌──────────────────────────────────────────────────────────────────┐
│ 阶段 1: 接收与验证 (Receive & Buffer)                             │
│ 文件: core/src/banking_stage/transaction_scheduler/              │
│       receive_and_buffer.rs                                       │
├──────────────────────────────────────────────────────────────────┤
│ ✓ 签名验证 (前置 sig-verify stage 完成)                           │
│ ✓ 解析和 Sanitize (line 453-457)                                 │
│ ✓ 账户锁验证 (line 419-426) - validate_account_locks()           │
│ ✓ Compute Budget 提取 (line 428-433)                             │
│    └─> compute_unit_price, compute_unit_limit                    │
│ ✓ Blockhash 年龄检查 (line 276, MAX_PROCESSING_AGE)              │
│ ✓ Fee Payer 余额检查 (line 302-311)                              │
└──────────────────────────────────────────────────────────────────┘
   │
   ↓ calculate_priority_and_cost() - line 437
   │
┌──────────────────────────────────────────────────────────────────┐
│ 阶段 2: Priority & Cost 计算                                      │
│ 文件: receive_and_buffer.rs:524-544                              │
├──────────────────────────────────────────────────────────────────┤
│ [Reward 计算] - runtime/src/bank/fee_distribution.rs:64-83       │
│   reward = priority_fee + (tx_fee - burn)                        │
│   其中 priority_fee = compute_unit_limit * compute_unit_price    │
│                                                                   │
│ [Cost 计算] - cost-model/src/cost_model.rs:34-54                 │
│   cost = signature_cost + write_lock_cost + data_bytes_cost      │
│          + programs_execution_cost + loaded_accounts_cost        │
│                                                                   │
│ [Priority 计算] - line 537-541                                   │
│   priority = (reward * 1_000_000) / (cost + 1)                   │
│   MULTIPLIER = 1_000_000 避免整数除法截断                         │
└──────────────────────────────────────────────────────────────────┘
   │
   ↓ TransactionState::new() - line 439
   │
┌──────────────────────────────────────────────────────────────────┐
│ 阶段 3: 状态存储与优先级队列                                       │
│ 文件: transaction_state.rs, transaction_priority_id.rs           │
├──────────────────────────────────────────────────────────────────┤
│ TransactionState {                                                │
│   transaction: RuntimeTransaction,                                │
│   max_age: MaxAge,                                                │
│   priority: u64,  ← 用于排序                                     │
│   cost: u64       ← 用于成本限制检查                              │
│ }                                                                 │
│                                                                   │
│ TransactionPriorityId {                                           │
│   priority: u64,  ← 主要排序字段 (降序)                           │
│   id: TransactionId  ← 次要排序字段 (升序, FIFO)                  │
│ }                                                                 │
│                                                                   │
│ 排序规则: 先按 priority 降序, 同 priority 按 id 升序              │
│ 文件: transaction_priority_id.rs:11-33                           │
└──────────────────────────────────────────────────────────────────┘
   │
   ↓ 进入 Container Queue
   │
┌──────────────────────────────────────────────────────────────────┐
│ 阶段 4: 交易调度 (Scheduler)                                      │
│ 文件: prio_graph_scheduler.rs                                    │
├──────────────────────────────────────────────────────────────────┤
│ [PrioGraphScheduler 配置] - line 49-64                           │
│   max_scheduled_cus: MAX_BLOCK_UNITS (60M)                       │
│   max_scanned_per_pass: 1000                                     │
│   look_ahead_window_size: 256                                    │
│   target_transactions_per_batch: 64                              │
│                                                                   │
│ [调度流程] - schedule() line 110-356                             │
│ 1. 预算检查: budget -= in_flight_cus                             │
│ 2. 线程可用性: 每线程 max_cu = 60M / num_threads                  │
│ 3. 前瞻窗口填充: 弹出 256 个交易建图                              │
│ 4. 依赖图构建: PrioGraph 跟踪账户读写冲突                         │
│ 5. 优先级调度: 循环弹出最高优先级交易                             │
│    ├─> 检查账户锁冲突 (line 394-427)                             │
│    ├─> 选择执行线程 (基于负载均衡)                                │
│    ├─> 累加成本到线程 budget                                     │
│    └─> 发送到 worker 线程                                        │
│ 6. 不可调度交易: 推回队列等待下次                                 │
│                                                                   │
│ [关键决策点]                                                      │
│ • 账户锁冲突 → UnschedulableConflicts (line 259-261)             │
│ • 线程满载 → UnschedulableThread (line 263-266)                  │
│ • Budget 耗尽 → 停止调度 (line 225)                              │
└──────────────────────────────────────────────────────────────────┘
   │
   ↓ 发送到 Consumer Workers
   │
┌──────────────────────────────────────────────────────────────────┐
│ 阶段 5: QoS 服务与成本追踪                                        │
│ 文件: qos_service.rs, cost_tracker.rs                            │
├──────────────────────────────────────────────────────────────────┤
│ [QosService::select_and_accumulate] - qos_service.rs:49-72       │
│   compute_transaction_costs() → 计算每笔交易成本                  │
│   select_transactions_per_cost() → 基于成本选择                   │
│                                                                   │
│ [CostTracker 限制检查] - cost_tracker.rs:169-179                 │
│   try_add(tx_cost):                                              │
│     ✓ block_cost < MAX_BLOCK_UNITS (60M)                         │
│     ✓ vote_cost < MAX_VOTE_UNITS (36M)                           │
│     ✓ account_cost < MAX_WRITABLE_ACCOUNT_UNITS (12M)            │
│     ✓ allocated_data_size < MAX_BLOCK_ACCOUNTS_DATA_SIZE (100M)  │
│                                                                   │
│   失败原因:                                                       │
│   • WouldExceedBlockMaxLimit                                     │
│   • WouldExceedAccountMaxLimit - 单账户写锁冲突过多               │
│   • WouldExceedAccountDataBlockLimit                             │
│                                                                   │
│ [常量定义] - block_cost_limits.rs:1-49                           │
│   MAX_BLOCK_UNITS = 60_000_000 CU                                │
│   MAX_WRITABLE_ACCOUNT_UNITS = 12_000_000 CU                     │
│   MAX_VOTE_UNITS = 36_000_000 CU                                 │
│   COMPUTE_UNIT_TO_US_RATIO = 30 (1 CU ≈ 30 微秒)                 │
└──────────────────────────────────────────────────────────────────┘
   │
   ↓ 执行交易
   │
┌──────────────────────────────────────────────────────────────────┐
│ 阶段 6: 区块执行与 Fee 记录                                       │
│ 文件: prioritization_fee.rs, prioritization_fee_cache.rs         │
├──────────────────────────────────────────────────────────────────┤
│ [执行完成后更新 Fee Cache]                                        │
│   prioritization_fee_cache.update() - cache.rs:209-275           │
│                                                                   │
│ [PrioritizationFee 追踪] - prioritization_fee.rs:149-251         │
│   min_compute_unit_price: 区块最低 CU price                       │
│   min_writable_account_fees: HashMap<Pubkey, u64>                │
│      ↑ 每个可写账户的最低费用                                     │
│                                                                   │
│   update(compute_unit_price, fee, accounts):                     │
│     - 更新区块最低价格                                            │
│     - 为每个可写账户更新最低费用                                  │
│                                                                   │
│   mark_block_completed():                                        │
│     - 清理冗余账户 (fee <= block_min_fee)                         │
│     - 标记为 finalized 可查询                                    │
│                                                                   │
│ [异步缓存更新] - cache.rs:137-151                                │
│   后台线程接收:                                                   │
│   • TransactionUpdate - 交易完成时                               │
│   • BankFinalized - 区块冻结时                                   │
│   维护最近 150 个区块的 fee 数据                                  │
└──────────────────────────────────────────────────────────────────┘
   │
   ↓ 区块完成
   │
┌──────────────────────────────────────────────────────────────────┐
│ 阶段 7: RPC 查询接口                                              │
│ 文件: rpc/src/rpc.rs:2386-2399                                   │
├──────────────────────────────────────────────────────────────────┤
│ get_recent_prioritization_fees(pubkeys):                         │
│   返回最近 150 个区块的优先级费用                                 │
│   • 如果指定账户, 返回该账户的历史费用                            │
│   • 如果为空, 返回区块级别最低费用                                │
│                                                                   │
│   Response: Vec<RpcPrioritizationFee> {                          │
│     slot: Slot,                                                  │
│     prioritization_fee: u64                                      │
│   }                                                              │
└──────────────────────────────────────────────────────────────────┘
```

---

## 阶段一: 交易接收与验证

### 1.1 入口点与数据流

**主要文件**: `core/src/banking_stage/transaction_scheduler/receive_and_buffer.rs`

**入口函数**: `TransactionViewReceiveAndBuffer::receive_and_buffer_packets()`
- **位置**: receive_and_buffer.rs:114-222
- **作用**: 从 `BankingPacketReceiver` 接收交易包, 进行初步验证

**关键流程**:

```rust
// 1. 接收交易批次 (line 119-124)
let (root_bank, working_bank) = bank_forks.read().unwrap();

// 2. 超时机制 (line 127-128)
const TIMEOUT: Duration = Duration::from_millis(10);
const PACKET_BURST_LIMIT: usize = 1000;

// 3. 决策: 是否解析交易 (line 244)
let should_parse = !matches!(decision, BufferedPacketsDecision::Forward);
// Forward 模式下不解析, 直接转发
```

### 1.2 验证步骤详解

**函数**: `try_handle_packet()`
**位置**: receive_and_buffer.rs:406-440

#### 步骤 1: 解析与 Sanitize (line 413-418, 446-487)

```rust
// translate_to_runtime_view() - line 446
pub(crate) fn translate_to_runtime_view<D: TransactionData>(
    data: D,
    bank: &Bank,
    enable_static_instruction_limit: bool,
    transaction_account_lock_limit: usize,
) -> Result<(RuntimeTransaction<ResolvedTransactionView<D>>, u64), PacketHandlingError>
```

**检查项**:
- ✅ 交易格式解析 (line 453-457)
- ✅ Vote-only bank 过滤 (line 468-470): 如果 bank 处于 vote-only 模式, 拒绝非投票交易
- ✅ 账户总数限制 (line 472-474): 总账户数不超过 `transaction_account_lock_limit`

**失败统计**: `num_dropped_on_parsing_and_sanitization`

#### 步骤 2: 账户锁验证 (line 419-426)

```rust
if validate_account_locks(
    view.account_keys(),
    root_bank.get_transaction_account_lock_limit(),
).is_err() {
    return Err(PacketHandlingError::LockValidation);
}
```

**作用**: 检查交易请求的账户锁数量是否超限
**限制**: 由 `Bank::get_transaction_account_lock_limit()` 动态返回
**失败统计**: `num_dropped_on_lock_validation` (line 362-363)

#### 步骤 3: Compute Budget 提取 (line 428-433)

```rust
let Ok(compute_budget_limits) = view
    .compute_budget_instruction_details()
    .sanitize_and_convert_to_compute_budget_limits(&working_bank.feature_set)
else {
    return Err(PacketHandlingError::ComputeBudget);
};
```

**提取字段**:
- `compute_unit_price`: 单位 Compute Unit 价格 (lamports/CU)
- `compute_unit_limit`: 交易可使用的最大 CU
- `loaded_accounts_bytes`: 加载账户数据大小限制

**特殊过滤**: `compute_unit_limit == 0` 的交易会被拒绝 (prioritization_fee_cache.rs:238-240)

**失败统计**: `num_dropped_on_compute_budget` (line 365-367)

#### 步骤 4: Blockhash 年龄检查 (line 273-278)

```rust
working_bank.check_transactions::<RuntimeTransaction<_>>(
    &transactions,
    &lock_results[..transactions.len()],
    MAX_PROCESSING_AGE,  // ← 关键常量
    &mut error_counters,
)
```

**常量**: `MAX_PROCESSING_AGE` 来自 `solana_clock` (receive_and_buffer.rs:25)
**失败原因**: `TransactionError::BlockhashNotFound` (line 288-290)
**失败统计**: `num_dropped_on_age`

#### 步骤 5: Fee Payer 余额检查 (line 302-311)

```rust
if let Err(err) = Consumer::check_fee_payer_unlocked(
    working_bank,
    transaction,
    &mut error_counters,
) {
    *result = Err(err);
    num_dropped_on_fee_payer += 1;
    container.remove_by_id(priority_id.id);
    continue;
}
```

**检查内容**: Fee payer 账户是否有足够余额支付交易费用
**失败统计**: `num_dropped_on_fee_payer`

---

## 阶段二: Priority Fee 计算

### 2.1 计算函数入口

**函数**: `calculate_priority_and_cost()`
**位置**: receive_and_buffer.rs:524-544
**调用点**: line 437

### 2.2 核心算法

```rust
pub(crate) fn calculate_priority_and_cost(
    transaction: &impl TransactionWithMeta,
    fee_budget_limits: &FeeBudgetLimits,
    bank: &Bank,
) -> (u64, u64) {
    // 1. 计算成本 (Cost Model)
    let cost = CostModel::calculate_cost(transaction, &bank.feature_set).sum();

    // 2. 计算奖励 (Reward)
    let reward = bank.calculate_reward_for_transaction(transaction, fee_budget_limits);

    // 3. 计算优先级
    const MULTIPLIER: u64 = 1_000_000;
    let priority = reward
        .saturating_mul(MULTIPLIER)
        .saturating_div(cost.saturating_add(1));

    (priority, cost)
}
```

### 2.3 Reward 计算详解

**函数**: `Bank::calculate_reward_for_transaction()`
**位置**: runtime/src/bank/fee_distribution.rs:64-83

```rust
pub fn calculate_reward_for_transaction(
    &self,
    transaction: &impl TransactionWithMeta,
    fee_budget_limits: &FeeBudgetLimits,
) -> u64 {
    // 1. 获取 lamports_per_signature
    let (_last_hash, last_lamports_per_signature) =
        self.last_blockhash_and_lamports_per_signature();

    // 2. 计算 fee_details
    let fee_details = solana_fee::calculate_fee_details(
        transaction,
        last_lamports_per_signature == 0,
        self.fee_structure().lamports_per_signature,
        fee_budget_limits.prioritization_fee,  // ← Priority fee 直接传入
        FeeFeatures::from(self.feature_set.as_ref()),
    );

    // 3. 计算 reward 和 burn
    let FeeDistribution { deposit: reward, burn: _ } =
        self.calculate_reward_and_burn_fee_details(&CollectorFeeDetails::from(fee_details));

    reward
}
```

**Reward 组成**:
```
reward = priority_fee + (transaction_fee - burn)

其中:
  priority_fee = compute_unit_limit * compute_unit_price
  transaction_fee = base_fee (签名费等)
  burn = transaction_fee * 50%
```

**实际公式** (fee_distribution.rs:93-97):
```rust
let burn = fee_details.transaction_fee * 50 / 100;
let deposit = fee_details.priority_fee
    .saturating_add(fee_details.transaction_fee.saturating_sub(burn));
```

### 2.4 Cost 计算详解

**函数**: `CostModel::calculate_cost()`
**位置**: cost-model/src/cost_model.rs:34-54

**Cost 组成**:
```rust
struct TransactionCost {
    signature_cost: u64,              // 签名验证成本
    write_lock_cost: u64,             // 写锁成本
    data_bytes_cost: u16,             // 指令数据大小成本
    programs_execution_cost: u64,     // 程序执行成本 (CU limit)
    loaded_accounts_data_size_cost: u64,  // 加载账户数据成本
}

total_cost = signature_cost + write_lock_cost + data_bytes_cost
           + programs_execution_cost + loaded_accounts_data_size_cost
```

**成本计算公式** (block_cost_limits.rs):

| 项目 | 公式 | 常量值 |
|------|------|--------|
| 签名成本 | `num_signatures * SIGNATURE_COST` | 720 CU/签名 |
| 写锁成本 | `num_write_locks * WRITE_LOCK_UNITS` | 300 CU/锁 |
| 数据字节成本 | `data_bytes / INSTRUCTION_DATA_BYTES_COST` | ~4.67 bytes/CU |
| 执行成本 | `compute_unit_limit` | 用户指定 |
| 账户加载成本 | `loaded_accounts_bytes / 32` | (近似) |

**示例计算**:
```
交易: 1 签名, 2 写锁, 100 bytes 数据, 200K CU limit
cost = 720 + (2 * 300) + (100 / 4.67) + 200_000 + 账户加载成本
     ≈ 720 + 600 + 21 + 200_000 + ~1000
     ≈ 202,341 CU
```

### 2.5 Priority 计算公式深度解析

**公式**: `priority = (reward * 1_000_000) / (cost + 1)`

**为什么需要 MULTIPLIER (1,000,000)?**

```rust
// 示例 1: 不使用 MULTIPLIER
reward = 5000 lamports
cost = 200_000 CU
priority = 5000 / 200_000 = 0  ← 整数除法截断!

// 示例 2: 使用 MULTIPLIER
priority = (5000 * 1_000_000) / 200_000 = 25_000
```

**意义**: MULTIPLIER 提供了足够的精度, 让不同 reward/cost 比率的交易能有显著区分度。

**实际优先级对比**:

| compute_unit_price | compute_unit_limit | reward | cost | priority |
|-------------------|-------------------|--------|------|----------|
| 1 microlamport/CU | 200K | 200 | 202K | 990 |
| 10 microlamport/CU | 200K | 2000 | 202K | 9_900 |
| 100 microlamport/CU | 200K | 20_000 | 202K | 99_000 |
| 1000 microlamport/CU | 200K | 200_000 | 202K | 990_099 |

**观察**: Priority 与 compute_unit_price 几乎线性相关, 但会被 cost 稀释。

---

## 关键代码位置速查表

| 功能 | 文件路径 | 关键行号 |
|------|---------|---------|
| **接收与验证** |
| 交易接收入口 | receive_and_buffer.rs | 114-222 |
| 解析与 Sanitize | receive_and_buffer.rs | 446-487 |
| 账户锁验证 | receive_and_buffer.rs | 419-426 |
| Compute Budget 提取 | receive_and_buffer.rs | 428-433 |
| 年龄检查 | receive_and_buffer.rs | 273-278 |
| Fee Payer 检查 | receive_and_buffer.rs | 302-311 |
| **Priority 计算** |
| Priority 计算入口 | receive_and_buffer.rs | 524-544 |
| Reward 计算 | bank/fee_distribution.rs | 64-83 |
| Cost 计算 | cost_model.rs | 34-54 |
| **状态存储** |
| TransactionState | transaction_state.rs | 完整文件 |
| TransactionPriorityId | transaction_priority_id.rs | 11-33 |
| **调度器** |
| PrioGraphScheduler | prio_graph_scheduler.rs | 68-361 |
| 调度主逻辑 | prio_graph_scheduler.rs | 110-356 |
| **QoS 与成本追踪** |
| QoS 选择 | qos_service.rs | 49-158 |
| CostTracker | cost_tracker.rs | 71-236 |
| 成本限制常量 | block_cost_limits.rs | 1-49 |
| **Fee 缓存** |
| Fee 记录 | prioritization_fee.rs | 149-251 |
| Fee 缓存 | prioritization_fee_cache.rs | 157-404 |
| RPC 查询 | rpc/src/rpc.rs | 2386-2399 |

---

## 下一部分预告

**第2部分**: 核心算法深度解析
- 调度算法的内部实现
- 依赖图构建与冲突检测
- 线程选择与负载均衡
- 账户锁机制详解

**第3部分**: 关键决策点识别
- 交易被拒绝的所有原因及代码位置
- Priority Fee 无效的边界条件
- 账户锁冲突的处理机制
- CU 限制的影响分析

**第4部分**: 时间窗口量化分析
- Slot 时间与交易延迟
- 调度延迟的理论分析
- 网络负载对上链时间的影响
- 从代码推导最快上链时间

**第5部分**: 实战策略框架
- Fee 设置建议算法
- 动态 Fee 调整策略
- 失败重试最佳实践
- 成本效益模型
