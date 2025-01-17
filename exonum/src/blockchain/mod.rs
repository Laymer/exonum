// Copyright 2019 The Exonum Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The module containing building blocks for creating blockchains powered by
//! the Exonum framework.
//!
//! Services are the main extension point for the Exonum framework. To create
//! your service on top of Exonum blockchain you need to perform the following steps:
//!
//! - Define your own information schema.
//! - Create one or more transaction types using the `TransactionSet` auto derive macro from
//!   `exonum_derive` and implement the [`Transaction`] trait for them.
//! - Create a data structure implementing the [`Service`] trait.
//! - Write API handlers for the service, if required.
//!
//! You may consult [the service creation tutorial][doc:create-service] for a detailed
//! instruction on how to create services.
//!
//! [`Transaction`]: ./trait.Transaction.html
//! [`Service`]: ./trait.Service.html
//! [doc:create-service]: https://exonum.com/doc/version/latest/get-started/create-service

pub use self::{
    block::{Block, BlockProof},
    config::{ConsensusConfig, StoredConfiguration, ValidatorKeys},
    genesis::GenesisConfig,
    schema::{Schema, TxLocation},
    service::{Service, ServiceContext, SharedNodeState},
    transaction::{
        ExecutionError, ExecutionResult, Transaction, TransactionContext, TransactionError,
        TransactionErrorType, TransactionMessage, TransactionResult, TransactionSet,
    },
};

pub mod config;

use byteorder::{ByteOrder, LittleEndian};

use std::{
    collections::{BTreeMap, HashMap},
    fmt, iter, mem, panic,
    sync::Arc,
};

use crate::crypto::{self, CryptoHash, Hash, PublicKey, SecretKey};
use crate::helpers::{Height, Round, ValidatorId};
use crate::messages::{Connect, Message, Precommit, ProtocolMessage, RawTransaction, Signed};
use crate::node::ApiSender;
use exonum_merkledb::{
    self, Database, Error as StorageError, Fork, IndexAccess, MapIndex, ObjectHash, Patch,
    Result as StorageResult, Snapshot,
};

mod block;
mod genesis;
mod schema;
mod service;
#[macro_use]
mod transaction;
#[cfg(test)]
mod tests;

/// Id of core service table family.
pub const CORE_SERVICE: u16 = 0;

/// Exonum blockchain instance with a certain services set and data storage.
///
/// Only nodes with an identical set of services and genesis block can be combined
/// into a single network.
pub struct Blockchain {
    db: Arc<dyn Database>,
    service_map: Arc<HashMap<u16, Box<dyn Service>>>,
    #[doc(hidden)]
    pub service_keypair: (PublicKey, SecretKey),
    pub(crate) api_sender: ApiSender,
}

impl Blockchain {
    /// Constructs a blockchain for the given `storage` and list of `services`.
    pub fn new<D: Into<Arc<dyn Database>>>(
        storage: D,
        services: Vec<Box<dyn Service>>,
        service_public_key: PublicKey,
        service_secret_key: SecretKey,
        api_sender: ApiSender,
    ) -> Self {
        let mut service_map = HashMap::new();
        for service in services {
            let id = service.service_id();
            if service_map.contains_key(&id) {
                panic!(
                    "Services have already contain service with id={}, please change it.",
                    id
                );
            }
            service_map.insert(id, service);
        }

        Self {
            db: storage.into(),
            service_map: Arc::new(service_map),
            service_keypair: (service_public_key, service_secret_key),
            api_sender,
        }
    }

    /// Recreates the blockchain to reuse with a sandbox.
    #[doc(hidden)]
    pub fn clone_with_api_sender(&self, api_sender: ApiSender) -> Self {
        Self {
            api_sender,
            ..self.clone()
        }
    }

    /// Returns mapping from the service identifier (`u16`) to service (`Box<dyn Service>`) for
    /// all services.
    pub fn service_map(&self) -> &Arc<HashMap<u16, Box<dyn Service>>> {
        &self.service_map
    }

    /// Creates a read-only snapshot of the current storage state.
    pub fn snapshot(&self) -> Box<dyn Snapshot> {
        self.db.snapshot()
    }

    /// Creates a snapshot of the current storage state that can be later committed into the storage
    /// via the `merge` method.
    pub fn fork(&self) -> Fork {
        self.db.fork()
    }

    /// Tries to create a `Transaction` object from the given raw message.
    /// A raw message can be converted into a `Transaction` object only
    /// if the following conditions are met:
    ///
    /// - Blockchain has a service with the `service_id` of the given raw message.
    /// - Service can deserialize the given raw message.
    pub fn tx_from_raw(&self, raw: RawTransaction) -> Result<Box<dyn Transaction>, failure::Error> {
        let service = self
            .service_map
            .get(&raw.service_id())
            .ok_or_else(|| format_err!("Service not found."))?;
        service.tx_from_raw(raw)
    }

    /// Commits changes from the patch to the blockchain storage.
    /// See [`Fork`](../../exonum_merkledb/struct.Fork.html) for details.
    pub fn merge(&mut self, patch: Patch) -> StorageResult<()> {
        self.db.merge(patch)
    }

    /// Returns the hash of the latest committed block.
    ///
    /// # Panics
    ///
    /// If the genesis block was not committed.
    pub fn last_hash(&self) -> Hash {
        Schema::new(&self.snapshot())
            .block_hashes_by_height()
            .last()
            .unwrap_or_else(Hash::default)
    }

    /// Returns the latest committed block.
    pub fn last_block(&self) -> Block {
        Schema::new(&self.snapshot()).last_block()
    }

    /// Returns the transactions pool size.
    pub fn pool_size(&self) -> u64 {
        Schema::new(&self.snapshot()).transactions_pool_len()
    }

    /// Creates and commits the genesis block with the given genesis configuration
    /// if the blockchain has not been initialized.
    ///
    /// # Panics
    ///
    /// * If the genesis block was not committed.
    /// * If storage version is not specified or not supported.
    pub fn initialize(&mut self, cfg: GenesisConfig) -> Result<(), failure::Error> {
        let has_genesis_block = !Schema::new(&self.snapshot())
            .block_hashes_by_height()
            .is_empty();
        if !has_genesis_block {
            self.create_genesis_block(cfg)?;
        }
        Ok(())
    }

    /// Creates and commits the genesis block with the given genesis configuration.
    fn create_genesis_block(&mut self, cfg: GenesisConfig) -> Result<(), failure::Error> {
        let mut config_propose = StoredConfiguration {
            previous_cfg_hash: Hash::zero(),
            actual_from: Height::zero(),
            validator_keys: cfg.validator_keys,
            consensus: cfg.consensus,
            services: BTreeMap::new(),
        };

        let patch = {
            let fork = self.fork();
            // Update service tables
            for (_, service) in self.service_map.iter() {
                let cfg = service.initialize(&fork);
                let name = service.service_name();
                if config_propose.services.contains_key(name) {
                    panic!(
                        "Services already contain service with '{}' name, please change it",
                        name
                    );
                }
                config_propose.services.insert(name.into(), cfg);
            }
            // Commit actual configuration
            {
                let mut schema = Schema::new(&fork);
                if schema.block_hash_by_height(Height::zero()).is_some() {
                    // TODO create genesis block for MemoryDB and compare it hash with zero block. (ECR-1630)
                    return Ok(());
                }
                schema.commit_configuration(config_propose);
            };
            self.merge(fork.into_patch())?;
            self.create_patch(
                ValidatorId::zero(),
                Height::zero(),
                &[],
                &mut BTreeMap::new(),
            )
            .1
        };
        self.merge(patch)?;
        Ok(())
    }

    /// Helper function to map a tuple (`u16`, `u16`) of service table coordinates
    /// to a 32-byte value to be used as the `ProofMapIndex` key (it currently
    /// supports only fixed size keys). The `hash` function is used to distribute
    /// keys uniformly (compared to padding).
    /// # Arguments
    ///
    /// * `service_id` - `service_id` as returned by instance of type of
    /// `Service` trait
    /// * `table_idx` - index of service table in `Vec`, returned by the
    /// `state_hash` method of instance of type of `Service` trait
    // also, it was the first idea around, to use `hash`
    pub fn service_table_unique_key(service_id: u16, table_idx: usize) -> Hash {
        debug_assert!(table_idx <= u16::max_value() as usize);
        let size = mem::size_of::<u16>();
        let mut vec = vec![0; 2 * size];
        LittleEndian::write_u16(&mut vec[0..size], service_id);
        LittleEndian::write_u16(&mut vec[size..2 * size], table_idx as u16);
        crypto::hash(&vec)
    }

    #[doc(hidden)]
    pub fn broadcast_raw_transaction(&self, tx: RawTransaction) -> Result<(), failure::Error> {
        let service_id = tx.service_id();
        if !self.service_map.contains_key(&service_id) {
            return Err(format_err!(
                "Unable to broadcast transaction: no service with ID={} found",
                service_id
            ));
        }
        let msg = Message::sign_transaction(
            tx.service_transaction(),
            service_id,
            self.service_keypair.0,
            &self.service_keypair.1,
        );

        self.api_sender.broadcast_transaction(msg)
    }

    /// Executes the given transactions from the pool.
    /// Then collects the resulting changes from the current storage state and returns them
    /// with the hash of the resulting block.
    pub fn create_patch(
        &self,
        proposer_id: ValidatorId,
        height: Height,
        tx_hashes: &[Hash],
        tx_cache: &mut BTreeMap<Hash, Signed<RawTransaction>>,
    ) -> (Hash, Patch) {
        // Create fork
        let mut fork = self.fork();

        let block_hash = {
            // Get last hash.
            let last_hash = self.last_hash();
            // Save & execute transactions.
            for (index, hash) in tx_hashes.iter().enumerate() {
                self.execute_transaction(*hash, height, index, &mut fork, tx_cache)
                    // Execution could fail if the transaction
                    // cannot be deserialized or it isn't in the pool.
                    .expect("Transaction execution error.");
            }

            // Invoke execute method for all services.
            for service in self.service_map.values() {
                // Skip execution for genesis block.
                if height > Height(0) {
                    before_commit(service.as_ref(), &mut fork);
                }
            }

            // Get tx & state hash.
            let (tx_hash, state_hash) = {
                let state_hashes = {
                    let schema = Schema::new(&fork);

                    let vec_core_state = schema.core_state_hash();
                    let mut state_hashes = Vec::new();

                    for (idx, core_table_hash) in vec_core_state.into_iter().enumerate() {
                        let key = Self::service_table_unique_key(CORE_SERVICE, idx);
                        state_hashes.push((key, core_table_hash));
                    }

                    for service in self.service_map.values() {
                        let service_id = service.service_id();
                        let vec_service_state = service.state_hash((&fork).snapshot());
                        for (idx, service_table_hash) in vec_service_state.into_iter().enumerate() {
                            let key = Self::service_table_unique_key(service_id, idx);
                            state_hashes.push((key, service_table_hash));
                        }
                    }

                    state_hashes
                };

                let schema = Schema::new(&fork);

                let state_hash = {
                    let mut sum_table = schema.state_hash_aggregator();
                    for (key, hash) in state_hashes {
                        sum_table.put(&key, hash)
                    }
                    sum_table.object_hash()
                };

                let tx_hash = schema.block_transactions(height).object_hash();

                (tx_hash, state_hash)
            };

            // Create block.
            let block = Block::new(
                proposer_id,
                height,
                tx_hashes.len() as u32,
                &last_hash,
                &tx_hash,
                &state_hash,
            );
            trace!("execute block = {:?}", block);
            // Calculate block hash.
            let block_hash = block.hash();
            // Update height.
            let schema = Schema::new(&fork);
            schema.block_hashes_by_height().push(block_hash);
            // Save block.
            schema.blocks().put(&block_hash, block);

            block_hash
        };

        (block_hash, fork.into_patch())
    }

    fn execute_transaction(
        &self,
        tx_hash: Hash,
        height: Height,
        index: usize,
        fork: &mut Fork,
        tx_cache: &mut BTreeMap<Hash, Signed<RawTransaction>>,
    ) -> Result<(), failure::Error> {
        let (tx, raw, service_name) = {
            let new_fork = &*fork;
            let snapshot = new_fork.snapshot();
            let schema = Schema::new(snapshot);

            let raw = get_tx(&tx_hash, &schema.transactions(), &tx_cache).ok_or_else(|| {
                failure::err_msg(format!(
                    "BUG: Cannot find transaction in database. tx: {:?}",
                    tx_hash
                ))
            })?;

            let service_name = self
                .service_map
                .get(&raw.service_id())
                .ok_or_else(|| {
                    failure::err_msg(format!(
                        "Service not found. Service id: {}",
                        raw.service_id()
                    ))
                })?
                .service_name();

            let tx = self.tx_from_raw(raw.payload().clone()).map_err(|error| {
                format_err!("Service <{}>: {}, tx: {:?}", service_name, error, tx_hash)
            })?;

            (tx, raw, service_name)
        };

        let catch_result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let context = TransactionContext::new(&*fork, service_name, &raw);
            tx.execute(context)
        }));

        let tx_result = TransactionResult(match catch_result {
            Ok(execution_result) => {
                if let Err(ref e) = execution_result {
                    // Unlike panic, transaction failure isn't that rare, so logging the
                    // whole transaction body is an overkill: it can be relatively big.
                    info!(
                        "Service <{}>: {:?} transaction execution failed: {:?}",
                        service_name, tx_hash, e
                    );
                    fork.rollback();
                }
                execution_result.map_err(TransactionError::from)
            }
            Err(err) => {
                if err.is::<StorageError>() {
                    // Continue panic unwind if the reason is StorageError.
                    panic::resume_unwind(err);
                }
                fork.rollback();
                error!(
                    "Service <{}>: {:?} transaction execution panicked: {:?}",
                    service_name, tx, err
                );

                Err(TransactionError::from_panic(&err))
            }
        });

        let mut schema = Schema::new(&*fork);
        schema.transaction_results().put(&tx_hash, tx_result);
        schema.commit_transaction(&tx_hash, raw);
        tx_cache.remove(&tx_hash);
        schema.block_transactions(height).push(tx_hash);
        let location = TxLocation::new(height, index as u64);
        schema.transactions_locations().put(&tx_hash, location);
        fork.flush();
        Ok(())
    }

    /// Commits to the blockchain a new block with the indicated changes (patch),
    /// hash and Precommit messages. After that invokes `after_commit`
    /// for each service in the increasing order of their identifiers.
    pub fn commit<I>(
        &mut self,
        patch: Patch,
        block_hash: Hash,
        precommits: I,
        tx_cache: &mut BTreeMap<Hash, Signed<RawTransaction>>,
    ) -> Result<(), failure::Error>
    where
        I: Iterator<Item = Signed<Precommit>>,
    {
        let patch = {
            let fork: Fork = patch.into();

            {
                let mut schema = Schema::new(&fork);
                schema.precommits(&block_hash).extend(precommits);

                // Consensus messages cache is useful only during one height, so it should be
                // cleared when a new height is achieved.
                schema.consensus_messages_cache().clear();
                let txs_in_block = schema.last_block().tx_count();

                schema.update_transaction_count(u64::from(txs_in_block));

                let tx_hashes = tx_cache.keys().cloned().collect::<Vec<Hash>>();
                for tx_hash in tx_hashes {
                    if let Some(tx) = tx_cache.remove(&tx_hash) {
                        if !schema.transactions().contains(&tx_hash) {
                            schema.add_transaction_into_pool(tx);
                        }
                    }
                }
            }
            fork.into_patch()
        };
        self.merge(patch)?;

        // Invokes `after_commit` for each service in order of their identifiers
        for (service_id, service) in self.service_map.iter() {
            let context = ServiceContext::new(
                self.service_keypair.0,
                self.service_keypair.1.clone(),
                self.api_sender.clone(),
                self.fork(),
                *service_id,
            );
            service.after_commit(&context);
        }
        Ok(())
    }

    /// Saves the `Connect` message from a peer to the cache.
    pub(crate) fn save_peer(&mut self, pubkey: &PublicKey, peer: Signed<Connect>) {
        let fork = self.fork();
        Schema::new(&fork).peers_cache().put(pubkey, peer);
        self.merge(fork.into_patch())
            .expect("Unable to save peer to the peers cache");
    }

    /// Removes from the cache the `Connect` message from a peer.
    pub fn remove_peer_with_pubkey(&mut self, key: &PublicKey) {
        let fork = self.fork();
        Schema::new(&fork).peers_cache().remove(key);
        self.merge(fork.into_patch())
            .expect("Unable to remove peer from the peers cache");
    }

    /// Returns `Connect` messages from peers saved in the cache, if any.
    pub fn get_saved_peers(&self) -> HashMap<PublicKey, Signed<Connect>> {
        let snapshot = self.snapshot();
        Schema::new(&snapshot).peers_cache().iter().collect()
    }

    /// Saves the given raw message to the consensus messages cache.
    pub(crate) fn save_message<T: ProtocolMessage>(&mut self, round: Round, raw: Signed<T>) {
        self.save_messages(round, iter::once(raw.into()));
    }

    /// Saves a collection of SignedMessage to the consensus messages cache with single access to the
    /// `Fork` instance.
    pub(crate) fn save_messages<I>(&mut self, round: Round, iter: I)
    where
        I: IntoIterator<Item = Message>,
    {
        let fork = self.fork();

        {
            let mut schema = Schema::new(&fork);
            schema.consensus_messages_cache().extend(iter);
            schema.set_consensus_round(round);
        }

        self.merge(fork.into_patch())
            .expect("Unable to save messages to the consensus cache");
    }
}

fn before_commit(service: &dyn Service, fork: &mut Fork) {
    match panic::catch_unwind(panic::AssertUnwindSafe(|| service.before_commit(fork))) {
        Ok(..) => fork.flush(),
        Err(err) => {
            if err.is::<StorageError>() {
                // Continue panic unwind if the reason is StorageError.
                panic::resume_unwind(err);
            }
            fork.rollback();
            error!(
                "{} service before_commit failed with error: {:?}",
                service.service_name(),
                err
            );
        }
    }
}

impl fmt::Debug for Blockchain {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Blockchain(..)")
    }
}

impl Clone for Blockchain {
    fn clone(&self) -> Self {
        Self {
            db: Arc::clone(&self.db),
            service_map: Arc::clone(&self.service_map),
            api_sender: self.api_sender.clone(),
            service_keypair: self.service_keypair.clone(),
        }
    }
}

/// Return transaction from persistent pool. If transaction is not present in pool, try
/// to return it from transactions cache.
pub(crate) fn get_tx<T: IndexAccess>(
    hash: &Hash,
    txs: &MapIndex<T, Hash, Signed<RawTransaction>>,
    tx_cache: &BTreeMap<Hash, Signed<RawTransaction>>,
) -> Option<Signed<RawTransaction>> {
    txs.get(&hash).or_else(|| tx_cache.get(&hash).cloned())
}

/// Checks that transaction exists in the persistent pool or in the transaction cache.
pub(crate) fn check_tx<T: IndexAccess>(
    hash: &Hash,
    txs: &MapIndex<T, Hash, Signed<RawTransaction>>,
    tx_cache: &BTreeMap<Hash, Signed<RawTransaction>>,
) -> bool {
    txs.contains(&hash) || tx_cache.contains_key(&hash)
}
