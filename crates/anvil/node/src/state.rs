//! Shared mutable state for the Anvil node.
//!
//! This module contains [`AnvilState`], a thread-safe container for all
//! runtime-mutable state that anvil RPC methods need to read and write.

use alloy_primitives::{Address, B256, U256};
use parking_lot::RwLock;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

/// Shared anvil node state.
///
/// This is cheaply cloneable (all fields are `Arc`-wrapped) and shared between
/// the RPC handler, the payload attributes builder, and the miner.
#[derive(Debug, Clone)]
pub struct AnvilState {
    inner: Arc<RwLock<AnvilStateInner>>,
}

/// Interior mutable state.
#[derive(Debug)]
pub struct AnvilStateInner {
    // -- impersonation --
    /// Accounts currently being impersonated.
    pub impersonated: HashSet<Address>,
    /// Whether all accounts are auto-impersonated.
    pub auto_impersonate: bool,

    // -- mining --
    /// Whether automine is enabled.
    pub automine: bool,
    /// Interval mining period in seconds (0 = disabled).
    pub interval_mining_secs: u64,

    // -- time --
    /// Offset applied to system time for block timestamps (seconds).
    pub time_offset_secs: i64,
    /// If set, the exact timestamp to use for the next block.
    pub next_block_timestamp: Option<u64>,
    /// If set, fixed interval between block timestamps.
    pub block_timestamp_interval: Option<u64>,

    // -- fees & gas --
    /// If set, override base fee for the next block.
    pub next_block_base_fee: Option<u64>,
    /// If set, override gas limit for the next block.
    pub block_gas_limit: Option<u64>,
    /// If set, override coinbase for the next block.
    pub coinbase: Option<Address>,

    // -- snapshots --
    /// Active snapshots: id -> (block_number, block_hash).
    pub snapshots: BTreeMap<U256, (u64, B256)>,
    /// Counter for snapshot IDs.
    pub next_snapshot_id: u64,
}

impl Default for AnvilStateInner {
    fn default() -> Self {
        Self {
            impersonated: HashSet::new(),
            auto_impersonate: false,
            automine: true,
            interval_mining_secs: 0,
            time_offset_secs: 0,
            next_block_timestamp: None,
            block_timestamp_interval: None,
            next_block_base_fee: None,
            block_gas_limit: None,
            coinbase: None,
            snapshots: BTreeMap::new(),
            next_snapshot_id: 1,
        }
    }
}

impl AnvilState {
    /// Create a new default state.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(AnvilStateInner::default())),
        }
    }

    /// Read access to inner state.
    pub fn read(&self) -> parking_lot::RwLockReadGuard<'_, AnvilStateInner> {
        self.inner.read()
    }

    /// Write access to inner state.
    pub fn write(&self) -> parking_lot::RwLockWriteGuard<'_, AnvilStateInner> {
        self.inner.write()
    }

    /// Returns the adjusted current timestamp (system time + offset).
    pub fn current_timestamp(&self) -> u64 {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let offset = self.read().time_offset_secs;
        if offset >= 0 {
            now + offset as u64
        } else {
            now.saturating_sub((-offset) as u64)
        }
    }

    /// Consumes and returns the next block timestamp override, if set.
    /// Also applies block_timestamp_interval if configured.
    pub fn take_next_block_timestamp(&self, parent_timestamp: u64) -> Option<u64> {
        let mut state = self.write();
        if let Some(ts) = state.next_block_timestamp.take() {
            return Some(ts);
        }
        if let Some(interval) = state.block_timestamp_interval {
            return Some(parent_timestamp + interval);
        }
        None
    }

    /// Consumes and returns the next block base fee override, if set.
    pub fn take_next_block_base_fee(&self) -> Option<u64> {
        self.write().next_block_base_fee.take()
    }

    /// Returns the block gas limit override, if set.
    pub fn block_gas_limit(&self) -> Option<u64> {
        self.read().block_gas_limit
    }

    /// Returns the coinbase override, if set.
    pub fn coinbase(&self) -> Option<Address> {
        self.read().coinbase
    }

    /// Create a new snapshot, returns the snapshot ID.
    pub fn create_snapshot(&self, block_number: u64, block_hash: B256) -> U256 {
        let mut state = self.write();
        let id = U256::from(state.next_snapshot_id);
        state.next_snapshot_id += 1;
        state.snapshots.insert(id, (block_number, block_hash));
        id
    }

    /// Remove and return a snapshot by ID.
    pub fn remove_snapshot(&self, id: U256) -> Option<(u64, B256)> {
        self.write().snapshots.remove(&id)
    }
}
