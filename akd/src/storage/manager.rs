// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// This source code is licensed under both the MIT license found in the
// LICENSE-MIT file in the root directory of this source tree and the Apache
// License, Version 2.0 found in the LICENSE-APACHE file in the root directory
// of this source tree.

//! Storage management module for AKD. A wrapper around the underlying database interaction
//! to manage interactions with the data layer to optimize things like caching and
//! transaction management

use crate::storage::cache::TimedCache;
use crate::storage::transaction::Transaction;
use crate::storage::types::DbRecord;
use crate::storage::types::KeyData;
use crate::storage::types::ValueState;
use crate::storage::types::ValueStateKey;
use crate::storage::Database;
use crate::storage::DbSetState;
use crate::storage::Storable;
use crate::storage::StorageError;
use crate::AkdLabel;
use crate::AkdValue;

use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::types::ValueStateRetrievalFlag;

type Metric = usize;

const METRIC_GET: Metric = 0;
const METRIC_BATCH_GET: Metric = 1;
const METRIC_SET: Metric = 2;
const METRIC_BATCH_SET: Metric = 3;
const METRIC_READ_TIME: Metric = 4;
const METRIC_WRITE_TIME: Metric = 5;
const METRIC_TOMBSTONE: Metric = 6;
const METRIC_GET_USER_STATE: Metric = 7;
const METRIC_GET_USER_DATA: Metric = 8;
const METRIC_GET_USER_STATE_VERSIONS: Metric = 9;

const NUM_METRICS: usize = 10;

/// Represents the manager of the storage mediums, including caching
/// and transactional operations (creating the transaction, commiting it, etc)
pub struct StorageManager<Db: Database + Sync + Send> {
    cache: Option<TimedCache>,
    transaction: Transaction,
    /// The underlying database managed by this storage manager
    pub db: Db,

    metrics: [Arc<AtomicU64>; NUM_METRICS],
}

impl<Db: Database + Sync + Send> Clone for StorageManager<Db> {
    fn clone(&self) -> Self {
        Self {
            cache: self.cache.clone(),
            transaction: Transaction::new(),
            db: self.db.clone(),
            metrics: self.metrics.clone(),
        }
    }
}

unsafe impl<Db: Database + Sync + Send> Sync for StorageManager<Db> {}
unsafe impl<Db: Database + Sync + Send> Send for StorageManager<Db> {}

impl<Db: Database + Sync + Send> StorageManager<Db> {
    /// Create a new storage manager with NO CACHE
    pub fn new_no_cache(db: &Db) -> Self {
        Self {
            cache: None,
            transaction: Transaction::new(),
            db: db.clone(),
            metrics: [0; NUM_METRICS].map(|_| Arc::new(AtomicU64::new(0))),
        }
    }

    /// Create a new storage manager with a cache utilizing the options provided (or defaults)
    pub fn new(
        db: &Db,
        cache_item_lifetime: Option<Duration>,
        cache_limit_bytes: Option<usize>,
        cache_clean_frequency: Option<Duration>,
    ) -> Self {
        Self {
            cache: Some(TimedCache::new(
                cache_item_lifetime,
                cache_limit_bytes,
                cache_clean_frequency,
            )),
            transaction: Transaction::new(),
            db: db.clone(),
            metrics: [0; NUM_METRICS].map(|_| Arc::new(AtomicU64::new(0))),
        }
    }

    /// Log metrics from the storage manager (cache, transaction, and storage hit rates etc)
    pub async fn log_metrics(&self, level: log::Level) {
        if let Some(cache) = &self.cache {
            cache.log_metrics(level)
        }

        self.transaction.log_metrics(level).await;

        let snapshot = self
            .metrics
            .iter()
            .map(|metric| metric.load(Ordering::Relaxed))
            .collect::<Vec<_>>();

        let msg = format!(
            "
===================================================
============ Database operation counts ============
===================================================
    SET {}, 
    BATCH SET {}, 
    GET {}, 
    BATCH GET {}
    TOMBSTONE {}
    GET USER STATE {}
    GET USER DATA {}
    GET USER STATE VERSIONS {}
===================================================
============ Database operation timing ============
===================================================
    TIME READ {} ms
    TIME WRITE {} ms",
            snapshot[METRIC_SET],
            snapshot[METRIC_BATCH_SET],
            snapshot[METRIC_GET],
            snapshot[METRIC_BATCH_GET],
            snapshot[METRIC_TOMBSTONE],
            snapshot[METRIC_GET_USER_STATE],
            snapshot[METRIC_GET_USER_DATA],
            snapshot[METRIC_GET_USER_STATE_VERSIONS],
            snapshot[METRIC_READ_TIME],
            snapshot[METRIC_WRITE_TIME]
        );

        match level {
            // Currently logs cannot be captured unless they are
            // println!. Normally Level::Trace should use the trace! macro.
            log::Level::Trace => println!("{}", msg),
            log::Level::Debug => debug!("{}", msg),
            log::Level::Info => info!("{}", msg),
            log::Level::Warn => warn!("{}", msg),
            _ => error!("{}", msg),
        }
    }

    /// Start an in-memory transaction of changes
    pub async fn begin_transaction(&self) -> bool {
        let started = self.transaction.begin_transaction().await;

        // disable the cache cleaning since we're in a write transaction
        // and will want to keep cache'd objects for the life of the transaction
        if let Some(cache) = &self.cache {
            cache.disable_clean();
        }

        started
    }

    /// Commit a transaction in the database
    pub async fn commit_transaction(&self) -> Result<(), StorageError> {
        // this retrieves all the trans operations, and "de-activates" the transaction flag
        let records = self.transaction.commit_transaction().await?;

        // The transaction is now complete (or reverted) and therefore we can re-enable
        // the cache cleaning status
        if let Some(cache) = &self.cache {
            cache.enable_clean();
        }

        if records.is_empty() {
            // no-op, there's nothing to commit
            return Ok(());
        }

        let _epoch = match records.last() {
            Some(DbRecord::Azks(azks)) => Ok(azks.latest_epoch),
            other => Err(StorageError::Transaction(format!(
                "The last record in the transaction log is NOT an Azks record {:?}",
                other
            ))),
        }?;

        // update the cache
        if let Some(cache) = &self.cache {
            cache.batch_put(&records).await;
        }

        // Write to the database
        self.tic_toc(
            METRIC_WRITE_TIME,
            self.db.batch_set(records, DbSetState::TransactionCommit),
        )
        .await?;
        self.increment_metric(METRIC_BATCH_SET);
        Ok(())
    }

    /// Rollback a transaction
    pub async fn rollback_transaction(&self) -> Result<(), StorageError> {
        self.transaction.rollback_transaction().await?;
        // The transaction is being reverted and therefore we can re-enable
        // the cache cleaning status
        if let Some(cache) = &self.cache {
            cache.enable_clean();
        }
        Ok(())
    }

    /// Retrieve a flag determining if there is a transaction active
    pub async fn is_transaction_active(&self) -> bool {
        self.transaction.is_transaction_active().await
    }

    /// Store a record in the database
    pub async fn set(&self, record: DbRecord) -> Result<(), StorageError> {
        // we're in a transaction, set the item in the transaction
        if self.is_transaction_active().await {
            self.transaction.set(&record).await;
            return Ok(());
        }

        // update the cache
        if let Some(cache) = &self.cache {
            cache.put(&record).await;
        }

        // write to the database
        self.tic_toc(METRIC_WRITE_TIME, self.db.set(record)).await?;
        self.increment_metric(METRIC_SET);
        Ok(())
    }

    /// Set a batch of records in the database
    pub async fn batch_set(&self, records: Vec<DbRecord>) -> Result<(), StorageError> {
        if records.is_empty() {
            // nothing to do, save the cycles
            return Ok(());
        }

        // we're in a transaction, set the items in the transaction
        if self.is_transaction_active().await {
            self.transaction.batch_set(&records).await;
            return Ok(());
        }

        // update the cache
        if let Some(cache) = &self.cache {
            cache.batch_put(&records).await;
        }

        // Write to the database
        self.tic_toc(
            METRIC_WRITE_TIME,
            self.db.batch_set(records, DbSetState::General),
        )
        .await?;
        self.increment_metric(METRIC_BATCH_SET);
        Ok(())
    }

    /// Retrieve a stored record directly from the data layer, ignoring any caching or transaction processes
    pub async fn get_direct<St: Storable>(
        &self,
        id: &St::StorageKey,
    ) -> Result<DbRecord, StorageError> {
        // cache miss, read direct from db
        let record = self
            .tic_toc(METRIC_READ_TIME, self.db.get::<St>(id))
            .await?;
        self.increment_metric(METRIC_GET);
        Ok(record)
    }

    /// Retrieve a stored record from the database
    pub async fn get<St: Storable>(&self, id: &St::StorageKey) -> Result<DbRecord, StorageError> {
        // we're in a transaction, meaning the object _might_ be newer and therefore we should try and read if from the transaction
        // log instead of the raw storage layer
        if self.is_transaction_active().await {
            if let Some(result) = self.transaction.get::<St>(id).await {
                return Ok(result);
            }
        }

        // check for a cache hit
        if let Some(cache) = &self.cache {
            if let Some(result) = cache.hit_test::<St>(id).await {
                return Ok(result);
            }
        }

        // cache miss, read direct from db
        let record = self
            .tic_toc(METRIC_READ_TIME, self.db.get::<St>(id))
            .await?;
        if let Some(cache) = &self.cache {
            // cache the result
            cache.put(&record).await;
        }
        self.increment_metric(METRIC_GET);
        Ok(record)
    }

    /// Retrieve a batch of records by id from the database
    pub async fn batch_get<St: Storable>(
        &self,
        ids: &[St::StorageKey],
    ) -> Result<Vec<DbRecord>, StorageError> {
        let mut map = Vec::new();

        if ids.is_empty() {
            // nothing to retrieve, save the cycles
            return Ok(map);
        }

        let mut key_set: HashSet<St::StorageKey> = ids.iter().cloned().collect::<HashSet<_>>();

        let trans_active = self.is_transaction_active().await;
        // first check the transaction log & cache records
        for id in ids.iter() {
            if trans_active {
                // we're in a transaction, meaning the object _might_ be newer and therefore we should try and read if from the transaction
                // log instead of the raw storage layer
                if let Some(result) = self.transaction.get::<St>(id).await {
                    map.push(result);
                    key_set.remove(id);
                    continue;
                }
            }

            // check if item is cached
            if let Some(cache) = &self.cache {
                if let Some(result) = cache.hit_test::<St>(id).await {
                    map.push(result);
                    key_set.remove(id);
                    continue;
                }
            }
        }

        if !key_set.is_empty() {
            // these are items to be retrieved from the backing database (not in pending transaction or in the object cache)
            let keys = key_set.into_iter().collect::<Vec<_>>();
            let mut results = self
                .tic_toc(METRIC_READ_TIME, self.db.batch_get::<St>(&keys))
                .await?;
            map.append(&mut results);
            self.increment_metric(METRIC_BATCH_GET);
        }
        Ok(map)
    }

    /// Flush the caching of objects (if present)
    pub async fn flush_cache(&self) {
        if let Some(cache) = &self.cache {
            cache.flush().await;
        }
    }

    /// Tombstone a set of records adhereing to the caching + transactional
    /// settings of the storage manager
    pub async fn tombstone_value_states(&self, keys: &[ValueStateKey]) -> Result<(), StorageError> {
        if keys.is_empty() {
            return Ok(());
        }

        let data = self.batch_get::<ValueState>(keys).await?;
        let mut new_data = vec![];
        for record in data {
            if let DbRecord::ValueState(value_state) = record {
                new_data.push(DbRecord::ValueState(ValueState {
                    epoch: value_state.epoch,
                    label: value_state.label,
                    plaintext_val: crate::AkdValue(crate::TOMBSTONE.to_vec()),
                    username: value_state.username,
                    version: value_state.version,
                }));
            }
        }
        if !new_data.is_empty() {
            debug!("Tombstoning {} entries", new_data.len());
            self.batch_set(new_data).await?;
            self.increment_metric(METRIC_TOMBSTONE);
        }

        Ok(())
    }

    /// Retrieve the specified user state object based on the retrieval flag from the database
    pub async fn get_user_state(
        &self,
        username: &AkdLabel,
        flag: ValueStateRetrievalFlag,
    ) -> Result<ValueState, StorageError> {
        let maybe_db_state = match self
            .tic_toc(METRIC_READ_TIME, self.db.get_user_state(username, flag))
            .await
        {
            Err(StorageError::NotFound(_)) => Ok(None),
            Ok(something) => Ok(Some(something)),
            Err(other) => Err(other),
        }?;
        self.increment_metric(METRIC_GET_USER_STATE);

        // in the event we are in a transaction, there may be an updated object in the
        // transactional storage. Therefore we should update the db retrieved value if
        // we can with what's in the transaction log
        if self.is_transaction_active().await {
            if let Some(transaction_value) = self.transaction.get_user_state(username, flag).await {
                if let Some(db_value) = &maybe_db_state {
                    if let Some(record) = Self::compare_db_and_transaction_records(
                        db_value.epoch,
                        transaction_value,
                        flag,
                    ) {
                        return Ok(record);
                    }
                } else {
                    // no db record, but there is a transaction record so use that
                    return Ok(transaction_value);
                }
            }
        }

        if let Some(state) = maybe_db_state {
            // cache the item for future access
            if let Some(cache) = &self.cache {
                cache.put(&DbRecord::ValueState(state.clone())).await;
            }

            Ok(state)
        } else {
            Err(StorageError::NotFound(format!("ValueState {:?}", username)))
        }
    }

    /// Retrieve all values states for a given user
    pub async fn get_user_data(&self, username: &AkdLabel) -> Result<KeyData, StorageError> {
        let maybe_db_data = match self
            .tic_toc(METRIC_READ_TIME, self.db.get_user_data(username))
            .await
        {
            Err(StorageError::NotFound(_)) => Ok(None),
            Ok(something) => Ok(Some(something)),
            Err(other) => Err(other),
        }?;
        self.increment_metric(METRIC_GET_USER_DATA);

        if self.is_transaction_active().await {
            // there are transaction-based values in the current transaction, they should override database-retrieved values
            let mut map = maybe_db_data
                .map(|data| {
                    data.states
                        .into_iter()
                        .map(|state| (state.epoch, state))
                        .collect::<HashMap<u64, _>>()
                })
                .unwrap_or_else(HashMap::new);

            let transaction_records = self
                .transaction
                .get_users_data(&[username.clone()])
                .await
                .remove(username)
                .unwrap_or_default();
            for transaction_record in transaction_records.into_iter() {
                map.insert(transaction_record.epoch, transaction_record);
            }

            return Ok(KeyData {
                states: map.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
            });
        }

        if let Some(data) = maybe_db_data {
            Ok(data)
        } else {
            Err(StorageError::NotFound(format!(
                "ValueState records for {:?}",
                username
            )))
        }
    }

    /// Retrieve the user -> state version mapping in bulk. This is the same as get_user_state in a loop, but with less data retrieved from the storage layer
    pub async fn get_user_state_versions(
        &self,
        usernames: &[AkdLabel],
        flag: ValueStateRetrievalFlag,
    ) -> Result<HashMap<AkdLabel, (u64, AkdValue)>, StorageError> {
        let mut data = self
            .tic_toc(
                METRIC_READ_TIME,
                self.db.get_user_state_versions(usernames, flag),
            )
            .await?;
        self.increment_metric(METRIC_GET_USER_STATE_VERSIONS);

        // in the event we are in a transaction, there may be an updated object in the
        // transactional storage. Therefore we should update the db retrieved value if
        // we can with what's in the transaction log
        if self.is_transaction_active().await {
            let transaction_records = self.transaction.get_users_states(usernames, flag).await;
            for (label, value_state) in transaction_records.into_iter() {
                if let Some((epoch, _)) = data.get(&label) {
                    // there is an existing DB record, check if we should updated it from the transaction log
                    if let Some(updated_record) =
                        Self::compare_db_and_transaction_records(*epoch, value_state, flag)
                    {
                        data.insert(label, (*epoch, updated_record.plaintext_val));
                    }
                } else {
                    // there is no db-equivalent record, but there IS a record in the transaction log.
                    // Take the transaction log value
                    data.insert(label, (value_state.epoch, value_state.plaintext_val));
                }
            }
        }

        Ok(data)
    }

    fn compare_db_and_transaction_records(
        state_epoch: u64,
        transaction_value: ValueState,
        flag: ValueStateRetrievalFlag,
    ) -> Option<ValueState> {
        match flag {
            ValueStateRetrievalFlag::SpecificVersion(_) => {
                return Some(transaction_value);
            }
            ValueStateRetrievalFlag::SpecificEpoch(_) => {
                return Some(transaction_value);
            }
            ValueStateRetrievalFlag::LeqEpoch(_) => {
                if transaction_value.epoch >= state_epoch {
                    // the transaction has either the same epoch or an epoch in the future, and therefore should
                    // override the db value
                    return Some(transaction_value);
                }
            }
            ValueStateRetrievalFlag::MaxEpoch => {
                if transaction_value.epoch >= state_epoch {
                    // the transaction has either the same epoch or an epoch in the future, and therefore should
                    // override the db value
                    return Some(transaction_value);
                }
            }
            ValueStateRetrievalFlag::MinEpoch => {
                if transaction_value.epoch <= state_epoch {
                    // the transaction has either the same epoch or an older epoch, and therefore should
                    // override the db value
                    return Some(transaction_value);
                }
            }
        }
        None
    }

    fn increment_metric(&self, _metric: Metric) {
        #[cfg(feature = "runtime_metrics")]
        {
            self.metrics[_metric].fetch_add(1, Ordering::Relaxed);
        }
    }

    async fn tic_toc<T>(&self, _metric: Metric, f: impl std::future::Future<Output = T>) -> T {
        #[cfg(feature = "runtime_metrics")]
        {
            let tic = std::time::Instant::now();
            let out = f.await;
            let delta = std::time::Instant::now().duration_since(tic);

            self.metrics[_metric].fetch_add(delta.as_millis() as u64, Ordering::Relaxed);

            out
        }
        #[cfg(not(feature = "runtime_metrics"))]
        {
            f.await
        }
    }
}
