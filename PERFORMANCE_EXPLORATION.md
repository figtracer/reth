# Reth Performance Architecture: A Deep Exploration

This document provides a comprehensive analysis of the performance-critical design decisions,
data structures, and architectural patterns that make Reth one of the fastest Ethereum execution
clients. It is organized by subsystem and includes concrete code references.

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Engine API & Block Processing Pipeline](#2-engine-api--block-processing-pipeline)
3. [State Root Computation](#3-state-root-computation)
4. [Execution Prewarming](#4-execution-prewarming)
5. [Caching Architecture](#5-caching-architecture)
6. [Database & Storage Layer](#6-database--storage-layer)
7. [Deferred & Lazy Computation](#7-deferred--lazy-computation)
8. [Memory Management & Allocation](#8-memory-management--allocation)
9. [Cross-Cutting Performance Patterns](#9-cross-cutting-performance-patterns)
10. [Performance Metrics & Observability](#10-performance-metrics--observability)

---

## 1. Executive Summary

Reth achieves high performance through a layered strategy:

- **Pipelined execution**: Block execution, state root computation, and persistence run as
  concurrent tasks connected by channels, minimizing idle time.
- **Parallel computation**: Storage roots, state hashing, and proof generation are parallelized
  with rayon and dedicated worker pools.
- **Multi-tier caching**: Fixed-size execution caches, LRU RPC caches, precompile caches, and
  read caches form a hierarchy that minimizes database I/O.
- **Deferred work**: Hash computation, trie updates, encoding, and persistence are deferred until
  actually needed, keeping the critical path short.
- **Optimized storage**: A dual-backend architecture (MDBX + RocksDB) with static files
  (NippyJar) for immutable data provides the right engine for each access pattern.
- **Allocation discipline**: Jemalloc, stack-allocated keys, buffer reuse, and zero-copy patterns
  minimize GC-like overhead in hot paths.

---

## 2. Engine API & Block Processing Pipeline

### 2.1 Architecture Overview

The engine tree (`crates/engine/tree/`) is the heart of live sync. It processes `newPayload` and
`forkchoiceUpdated` messages from the consensus layer (CL).

**Key files:**
- `crates/engine/tree/src/tree/mod.rs` — Main handler, FCU logic
- `crates/engine/tree/src/engine.rs` — `EngineHandler` event loop
- `crates/engine/tree/src/persistence.rs` — Async persistence service
- `crates/engine/tree/src/tree/payload_processor/mod.rs` — Pipelined execution

### 2.2 The Four-Task Pipeline

When a new payload arrives, Reth spawns a **four-task pipeline** that overlaps I/O with
computation (`payload_processor/mod.rs:216-325`):

```
Transaction Iterator (parallel tx conversion)
    ↓ decoded transactions
Prewarm Task (parallel speculative execution)
    ↓ proof targets + warm cache
MultiProof Task (parallel proof workers)
    ↓ SparseTrieUpdate
Sparse Trie Task (incremental state root)
    ↓ final state root
```

Meanwhile, **sequential block execution** proceeds on the main thread, reading from the
pre-warmed cache. The state root computation runs concurrently alongside execution rather than
after it.

### 2.3 Asynchronous Persistence

Persistence runs in a **separate thread** (`persistence.rs:29-56`):

```rust
pub struct PersistenceService<N> {
    provider: ProviderFactory<N>,
    incoming: Receiver<PersistenceAction<N::Primitives>>,
    pruner: PrunerWithFactory<ProviderFactory<N>>,
}
```

- Blocks are written to static files and database without blocking execution.
- A **biased channel select** prioritizes persistence completion notifications.
- Persistence is triggered when the in-memory chain exceeds a configurable threshold
  (`mod.rs:1791-1800`), keeping memory bounded while avoiding write amplification.

### 2.4 In-Memory Tree State

The block tree (`tree/state.rs:24-41`) uses:
- `HashMap<B256, Arc<BlockState>>` for O(1) block lookup by hash
- `BTreeMap<u64, B256>` for ordered block number lookups
- Parent-child tracking for efficient reorg handling

Executed blocks are held in memory until persisted, enabling instant access for RPC queries and
fork choice decisions without database reads.

### 2.5 Backfill vs Live Sync

- **Pipeline sync** (historical): Staged sync through `crates/stages/` with batch execution,
  ETL-optimized database writes, and threshold-based commits.
- **Live sync** (tip-following): The engine tree with the four-task pipeline above.
- Transition: Once pipeline catches up to CL, the engine switches to live mode. During backfill,
  new payloads are **buffered** rather than executed (`mod.rs:569-640`).

---

## 3. State Root Computation

State root is often the bottleneck in Ethereum clients. Reth addresses this with three
complementary strategies.

### 3.1 Sparse Trie

**Key file:** `crates/trie/sparse/src/trie.rs`

Instead of loading the entire Merkle Patricia Trie, Reth maintains a **sparse trie** that only
contains nodes relevant to recent state changes:

- Nodes are **revealed** on-demand from multiproofs (only touched nodes loaded into memory).
- Updates apply only to revealed portions of the trie.
- The `SparseStateTrie` is **reused across blocks** via `ClearedSparseStateTrie`
  (`payload_processor/mod.rs:131`), avoiding rebuild costs.

### 3.2 Prefix Sets for Minimal Traversal

**Key file:** `crates/trie/common/src/prefix_set.rs`

Prefix sets track which keys changed, enabling the trie walker to skip unchanged subtrees:

```rust
pub struct PrefixSet {
    keys: Arc<Vec<Nibbles>>,  // Sorted changed keys
    index: usize,             // Sequential scan index
}
```

The critical optimization is the **sequential index** (`prefix_set.rs:202-226`): since trie
traversal visits nodes in order, the prefix set exploits this with an advancing index that makes
the total lookup cost O(n) across all queries rather than O(n log n).

### 3.3 Parallel State Root

**Key file:** `crates/trie/parallel/src/root.rs`

`ParallelStateRoot` pre-computes storage roots for changed accounts in parallel:

1. **Identify targets**: Collect accounts with storage changes into `StorageRootTargets`.
2. **Parallel computation** (lines 95-128): Each storage root computed via `tokio::spawn_blocking`
   with results sent through `sync_channel(1)` for backpressure.
3. **Sequential walk**: The account trie is walked sequentially, consuming pre-computed storage
   roots from the channel.
4. **Missed leaf fallback**: If an account wasn't pre-computed, its storage root is computed
   on-demand (line 164).

Thread pool reuse: A static `OnceLock<Runtime>` with 15-second keep-alive avoids thread creation
overhead per block (`root.rs:269-286`).

### 3.4 Concurrent State Root via MultiProof

**Key file:** `crates/engine/tree/src/tree/payload_processor/multiproof.rs`

During live sync, state root computation runs **concurrently with block execution**:

1. State updates from execution are sent as `MultiProofMessage::StateUpdate`.
2. A **ProofSequencer** (`multiproof.rs:131-179`) maintains ordering with fast-path for
   in-order delivery.
3. **Pre-spawned worker pools** (`proof_task.rs:103-200`) handle storage and account proofs
   with dedicated database transactions (no contention).
4. The **sparse trie task** (`sparse_trie.rs:54-130`) batch-drains pending updates and applies
   them, computing the final root only when all updates are consumed.

---

## 4. Execution Prewarming

### 4.1 Speculative Transaction Execution

**Key file:** `crates/engine/tree/src/tree/payload_processor/prewarm.rs`

Before actual block execution, Reth **speculatively executes transactions in parallel** to
populate the state cache:

```
Prewarm Workers (N concurrent)
    ↓ each executes a transaction against parent state
    ↓ reads populate shared ExecutionCache
Sequential Execution
    ↓ reads from warm cache (cache hits instead of DB reads)
```

- Workers use independent EVM instances but share the same `ExecutionCache` via `Arc`.
- **Prewarm mode** (`cached_state.rs:108-126`) inserts every state read into the cache,
  while normal execution only reads.
- Cache updates are atomic under lock to prevent pollution from failed transactions.

### 4.2 Block Access List (BAL) Optimization

**Key file:** `crates/engine/tree/src/tree/payload_processor/bal.rs`

When the consensus layer provides a **Block Access List** (EIP-7928), Reth skips transaction
replay entirely:

- Known accessed storage slots are **directly prefetched** across worker threads.
- Slots are divided across workers for parallel loading.
- This is dramatically faster than re-executing transactions for cache warming.

### 4.3 Cache Inheritance

Each block's prewarming **inherits the parent block's cache** if the parent hash matches
(`mod.rs:753-866`). The `PayloadExecutionCache` uses `Arc` reference counting to safely share
caches across async boundaries and detect when a cache is available for reuse.

---

## 5. Caching Architecture

### 5.1 ExecutionCache (Fixed-Size Pools)

**Key file:** `crates/engine/tree/src/tree/payload_processor/cached_state.rs`

The execution cache uses **fixed-size hash tables** optimized for the EVM workload:

```rust
storage_cache:   FixedHashPool<(Address, B256), U256>  // 88.88% of capacity
account_cache:   FixedHashPool<Address, Account>        // 5.56%
code_cache:      FixedHashPool<B256, Bytecode>          // 5.56%
```

Design decisions:
- **Fixed size** prevents unbounded memory growth.
- **88.88% allocation to storage** reflects that storage is the hottest EVM data path.
- **Cache-line aligned buckets** (128 bytes) for CPU cache efficiency.
- **Epoch-based invalidation** — O(1) cache clear using epoch counters rather than iterating.
- **Collision tracking** via metrics for tuning cache sizes.

### 5.2 CachedReads (Payload Building)

**Key file:** `crates/revm/src/cached.rs`

For payload building (block production), `CachedReads` provides a HashMap-based cache:

```rust
pub struct CachedReads {
    accounts: HashMap<Address, CachedAccount>,
    contracts: HashMap<B256, Bytecode>,
    block_hashes: HashMap<u64, B256>,
}
```

Multiple payload build attempts reuse the same cache, avoiding redundant DB reads.

### 5.3 RPC State Cache

**Key file:** `crates/rpc/rpc-eth-types/src/cache/mod.rs`

The RPC layer maintains separate LRU caches:
- **Block cache**: 5,000 blocks (default)
- **Receipt cache**: 2,000 entries
- **Header cache**: 1,000 entries
- **Transaction hash index**: Maps tx hashes to (block_hash, index)

A **multi-consumer pattern** (`MultiConsumerLruCache`) queues concurrent requests for the same
block, fetching from the database only once.

Live cache updates subscribe to canonical chain notifications, automatically populating the cache
on new blocks and evicting reorged blocks.

### 5.4 Precompile Cache

**Key file:** `crates/engine/tree/src/tree/precompile_cache.rs`

Per-address LRU caches (10,000 entries each) store precompile input→output mappings:

```rust
pub struct PrecompileCacheMap<S>(Arc<DashMap<Address, PrecompileCache<S>>>)
```

- Only caches **pure** (deterministic) precompiles.
- Spec-aware: different hardforks can produce different outputs.
- Shared across prewarm workers via `Arc<DashMap>`.

### 5.5 Fee History Cache

**Key file:** `crates/rpc/rpc-eth-types/src/fee_history.rs`

Precomputed fee history with gas price percentiles, stored in a BTreeMap for ordered block range
queries. Automatically populated for missing blocks.

---

## 6. Database & Storage Layer

### 6.1 Dual-Backend Architecture (MDBX + RocksDB)

**Key files:**
- `crates/storage/db/src/implementation/mdbx/mod.rs` — MDBX configuration
- `crates/storage/provider/src/rocksdb/provider.rs` — RocksDB integration
- `crates/storage/provider/src/either_writer.rs` — Backend routing

Reth uses **different databases for different access patterns**:

| Data | Backend | Rationale |
|------|---------|-----------|
| Current state (accounts, storage) | MDBX | Fast random reads via mmap |
| Account/storage history | RocksDB (Edge mode) | LSM tree efficient for append-heavy writes |
| Transaction hash → number | RocksDB (Edge mode) | Write-heavy, rarely read |
| Immutable historical data | Static files (NippyJar) | Columnar, compressed, mmap'd |

Configuration via `StorageSettings` (`db-api/models/metadata.rs:13-137`):
- **Legacy mode**: All MDBX (backward compatible)
- **Edge mode**: History in RocksDB, changesets in static files

The `EitherReader`/`EitherWriter` abstraction routes reads/writes to the correct backend at
runtime without code changes.

### 6.2 MDBX Tuning

Key configuration (`mdbx/mod.rs`):
- **Max size**: 8 TB with 4 GB growth steps
- **Max readers**: 32,000 concurrent read transactions
- **WriteMap mode**: Memory-mapped writes in read-write mode
- **Sync modes**: Durable (full fsync) vs SafeNoSync (faster, crash-risk trade-off)
- **Page size**: OS-aligned for optimal mmap performance

### 6.3 Static Files (NippyJar)

**Key file:** `crates/storage/nippy-jar/src/lib.rs`

NippyJar provides **columnar, compressed, memory-mapped** storage for immutable data:

- **Column-based**: Headers, transactions, receipts stored in separate columns for efficient
  access to individual fields.
- **Offset-based access**: O(1) random access via precomputed offsets (no tree traversal).
- **Compression**: Zstd dictionary compression with configurable dictionaries per column.
- **Memory-mapped reads**: Data accessed directly through OS page cache.
- **Consistency checks**: CRC-based integrity verification, automatic healing on startup.
- **Buffer reuse**: Internal `Vec<u8>` buffers cleared and reused across operations
  (`writer.rs:28-488`).

### 6.4 ETL (Extract-Transform-Load)

**Key file:** `crates/etl/src/lib.rs`

For bulk data loading during initial sync:

1. **Buffer in memory** until threshold exceeded (`lib.rs:103-114`).
2. **Parallel sort** with rayon on flush (`lib.rs:133-142`).
3. **Write sorted chunks** to temporary files.
4. **Merge-K iteration** via binary heap across sorted files (`lib.rs:185-217`).
5. **Append-mode insert** into MDBX using `WriteFlags::APPEND` — pre-sorted data avoids B-tree
   rebalancing (critical for hash-based keys).

### 6.5 DupSort Tables

Tables like `AccountChangeSets` and `StorageChangeSets` use MDBX's **DupSort** feature:
multiple sorted values per key. This provides efficient historical lookups without separate
indices. Cursor operations like `next_dup()`, `walk_dup()`, and `append_dup()` enable fast
iteration within a key's value set.

### 6.6 Write Batching

- **Execution stage** (`stages/stages/src/stages/execution.rs`): Batches block executions and
  commits based on block count, state size, cumulative gas, and elapsed time thresholds.
- **RocksDB batches**: Writes accumulated in `WriteBatchWithTransaction` and committed alongside
  MDBX for eventual consistency.
- **Commit ordering**: Normal = static files → RocksDB → MDBX; Unwind = reverse for safe
  recovery.

---

## 7. Deferred & Lazy Computation

### 7.1 Sealed Headers and Blocks

**Key file:** `crates/primitives-traits/src/header/sealed.rs`

`SealedHeader` caches the block hash, computed once:

```rust
pub struct SealedHeader<H = Header> {
    hash: BlockHash,   // Computed once, cached forever
    header: H,
}
```

During RLP decoding, the hash is computed directly from raw bytes (`sealed.rs:166-182`),
avoiding a re-encode step.

### 7.2 LazyTrieData

**Key file:** `crates/trie/common/src/lazy.rs`

`LazyTrieData` wraps a `OnceLock<T>` with a compute function:

```rust
pub struct LazyTrieData<T> {
    data: OnceLock<T>,
    compute: Option<Arc<dyn Fn() -> T + Send + Sync>>,
}
```

The expensive trie data computation runs only on first access. Subsequent calls return the
cached value via `Arc` clone.

### 7.3 Deferred Trie Updates

**Key file:** `crates/chain-state/src/deferred_trie.rs`

Uses `Arc::make_mut()` for **copy-on-write** semantics:

```rust
// Cheap clone — just increments Arc reference count
let mut overlay = TrieInputSorted::new(
    Arc::clone(&trie_input.nodes),
    Arc::clone(&trie_input.state),
    Default::default(),
);

// Copy only triggered when mutation actually occurs
if !sorted_hashed_state.is_empty() {
    Arc::make_mut(&mut overlay.state)
        .extend_ref_and_sort(&sorted_hashed_state);
}
```

Read-heavy case (parent overlay reused) is O(1); copy happens only on write.

### 7.4 Compact Encoding

**Key file:** `crates/storage/codecs/src/lib.rs`

Custom `Compact` codec for database values:
- **Bitfield flags** encode presence/absence of optional fields.
- **Variable-length integers** reduce storage for small values.
- **50-70% storage reduction** vs standard RLP for account data.
- Derive macro generates implementations automatically.

### 7.5 WithEncoded Pattern

Transactions keep both decoded and encoded forms:

```rust
// From alloy_eips::eip2718::WithEncoded
WithEncoded::new(encoded, tx.with_signer(signer))
```

This avoids re-encoding for network transmission or hashing.

### 7.6 RecoveredBlock — Lazy Sealing

**Key file:** `crates/primitives-traits/src/block/recovered.rs`

```rust
pub struct RecoveredBlock<B: Block> {
    block: SealedBlock<B>,    // Hash computed on demand
    senders: Vec<Address>,    // Expensive recovery cached
}
```

`new_unhashed()` defers hash computation; sender recovery (ECDSA) is cached after first
computation.

---

## 8. Memory Management & Allocation

### 8.1 Jemalloc

**Key file:** `crates/cli/util/src/allocator.rs`

Reth uses **jemalloc** as the global allocator:
- Better multi-threaded performance than system allocator.
- Reduced fragmentation under concurrent workloads.
- Optional profiling support via `profiling` feature flag.

### 8.2 Stack-Allocated Keys

**Key file:** `crates/storage/db-api/src/models/sharded_key.rs`

Database keys use fixed-size stack allocation:
- `ShardedKey`: 28 bytes on stack (Address + block number).
- `StorageShardedKey`: Address + B256 + block number, all stack-allocated.
- Zero heap allocation for the most frequently accessed keys.

### 8.3 ArrayVec for Nibbles

**Key file:** `crates/trie/common/src/nibbles.rs`

Trie nibble paths use `ArrayVec` (or `SmallVec`) with 64-byte inline storage:
- Most nibble paths fit within the inline buffer.
- No heap allocation for typical trie operations.

### 8.4 Buffer Reuse

Throughout the codebase, buffers are reused rather than reallocated:
- MDBX cursor `buf: Vec<u8>` cleared and reused for compression (`cursor.rs:31`).
- NippyJar writer reuses internal buffers across row writes.
- ETL collector reuses flush buffers.
- `compress_to_buf_or_ref!` macro avoids allocation for incompressible types (B256, etc.).

### 8.5 Memory-Mapped I/O

Beyond MDBX's mmap:
- **NippyJar** memory-maps static files for zero-copy reads.
- OS page cache manages eviction — no application-level LRU needed for historical data.
- Effective memory usage can far exceed physical RAM.

### 8.6 Parallel Processing with Rayon

**Key file:** `crates/trie/common/src/hashed_state.rs`

State hashing uses rayon's parallel iterators:

```rust
pub fn from_bundle_state<'a, KH: KeyHasher>(
    state: impl IntoParallelIterator<Item = (&'a Address, &'a BundleAccount)>,
) -> Self {
    state.into_par_iter()
        .map(|(address, account)| {
            let hashed_address = KH::hash_key(address);
            // ... process in parallel
        })
        .collect()
}
```

Similarly, `StorageRootTargets` implements `IntoParallelIterator` for parallel storage root
computation without intermediate allocations.

---

## 9. Cross-Cutting Performance Patterns

### 9.1 Channel-Based Coordination

The codebase extensively uses channels instead of locks:
- `sync_channel(1)` for backpressure between parallel storage root computation and sequential
  account walk.
- `crossbeam` channels for proof worker pools.
- `tokio::watch` for pending block state (single-producer, multi-consumer).
- `oneshot` channels for persistence completion notification.

### 9.2 Metrics-Driven Optimization

Every performance-critical subsystem exposes metrics:
- Cache hit/miss rates, collision counts.
- Precomputed vs missed storage roots.
- Persistence latency, commit latency.
- Proof computation duration.
- Memory usage tracking.

### 9.3 Feature-Gated Parallelism

Parallel code paths are gated behind `#[cfg(feature = "rayon")]`, allowing single-threaded
builds for testing and embedded use.

### 9.4 Commit Order Safety

Database commits follow a strict order to ensure crash recovery:
- **Normal**: Static files → RocksDB → MDBX
- **Unwind**: MDBX → RocksDB → Static files

This ensures that the canonical state (MDBX) is always consistent or recoverable.

### 9.5 Epoch-Based Invalidation

The `ExecutionCache` uses epoch counters for O(1) cache invalidation rather than iterating all
entries. When a new block starts, the epoch advances and stale entries are lazily evicted on
access.

---

## 10. Performance Metrics & Observability

### 10.1 State Root Metrics

| Metric | Location | Purpose |
|--------|----------|---------|
| `precomputed_storage_roots` | `trie/parallel/src/metrics.rs` | Count of parallel pre-computations |
| `missed_leaves` | `trie/parallel/src/metrics.rs` | On-demand fallback count |
| `multiproof_skipped_account_nodes` | `trie/sparse/src/metrics.rs` | Redundant reveals |
| State root duration | `payload_processor/sparse_trie.rs` | End-to-end computation time |

### 10.2 Cache Metrics

| Metric | Location | Purpose |
|--------|----------|---------|
| Cache hits/misses | `cached_state.rs` | Per-pool hit rates |
| Collision count | `cached_state.rs` | Hash table quality |
| Precompile cache hits | `precompile_cache.rs` | Precompile efficiency |

### 10.3 Database Metrics

| Metric | Location | Purpose |
|--------|----------|---------|
| Transaction open duration | `mdbx/tx.rs` | Long transaction detection |
| Commit latency | `mdbx/tx.rs` | Write performance |
| Page counts (leaf/branch/overflow) | `mdbx/mod.rs` | Database size tracking |
| Freelist size | `mdbx/mod.rs` | Space reclamation |

### 10.4 Engine Metrics

| Metric | Location | Purpose |
|--------|----------|---------|
| Block execution time | `tree/mod.rs` | Per-block execution latency |
| Persistence duration | `persistence.rs` | Disk write latency |
| In-memory block count | `chain-state/in_memory.rs` | Memory pressure |

---

## Appendix: Key File Reference

| Subsystem | Key File | Purpose |
|-----------|----------|---------|
| Engine handler | `crates/engine/tree/src/engine.rs` | Event loop |
| Tree state | `crates/engine/tree/src/tree/mod.rs` | Block tree management |
| Payload pipeline | `crates/engine/tree/src/tree/payload_processor/mod.rs` | Four-task pipeline |
| Prewarming | `crates/engine/tree/src/tree/payload_processor/prewarm.rs` | Speculative execution |
| MultiProof | `crates/engine/tree/src/tree/payload_processor/multiproof.rs` | Parallel proofs |
| Sparse trie task | `crates/engine/tree/src/tree/payload_processor/sparse_trie.rs` | Incremental root |
| Execution cache | `crates/engine/tree/src/tree/payload_processor/cached_state.rs` | Fixed-size pools |
| Precompile cache | `crates/engine/tree/src/tree/precompile_cache.rs` | Per-address LRU |
| Persistence | `crates/engine/tree/src/persistence.rs` | Async disk writes |
| Sparse trie | `crates/trie/sparse/src/trie.rs` | Incremental trie |
| Prefix sets | `crates/trie/common/src/prefix_set.rs` | Changed key tracking |
| Parallel root | `crates/trie/parallel/src/root.rs` | Parallel storage roots |
| Lazy trie data | `crates/trie/common/src/lazy.rs` | Deferred computation |
| MDBX config | `crates/storage/db/src/implementation/mdbx/mod.rs` | Database tuning |
| NippyJar | `crates/storage/nippy-jar/src/lib.rs` | Columnar static files |
| ETL | `crates/etl/src/lib.rs` | Bulk loading |
| RocksDB provider | `crates/storage/provider/src/rocksdb/provider.rs` | RocksDB integration |
| Either writer | `crates/storage/provider/src/either_writer.rs` | Backend routing |
| Compact codec | `crates/storage/codecs/src/lib.rs` | Storage encoding |
| Sealed header | `crates/primitives-traits/src/header/sealed.rs` | Cached hash |
| CachedReads | `crates/revm/src/cached.rs` | Payload build cache |
| RPC cache | `crates/rpc/rpc-eth-types/src/cache/mod.rs` | RPC state cache |
| Allocator | `crates/cli/util/src/allocator.rs` | Jemalloc setup |
| Deferred trie | `crates/chain-state/src/deferred_trie.rs` | Copy-on-write trie |
