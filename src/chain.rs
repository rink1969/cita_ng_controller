// Copyright Rivtower Technologies LLC.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::auth::Authentication;
use crate::pool::Pool;
use crate::util::{
    check_block, check_block_exists, check_proposal_exists, exec_block, get_block, get_tx,
    hash_data, load_data, move_tx, print_main_chain, reconfigure, remove_proposal, store_data,
    store_tx_info, unix_now, write_block, write_proposal,
};
use crate::utxo_set::{LOCK_ID_BLOCK_INTERVAL, LOCK_ID_VALIDATORS};
use crate::GenesisBlock;
use cita_cloud_proto::blockchain::{BlockHeader, CompactBlock, CompactBlockBody};
use cita_cloud_proto::controller::raw_transaction::Tx::UtxoTx;
use log::{info, warn};
use prost::Message;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct Chain {
    kms_port: u16,
    storage_port: u16,
    executor_port: u16,
    consensus_port: u16,
    block_number: u64,
    block_hash: Vec<u8>,
    block_delay_number: u32,
    // hashmap for each index
    // key of hashmap is block_hash
    // value of hashmap is (block, proof)
    #[allow(clippy::type_complexity)]
    fork_tree: Vec<HashMap<Vec<u8>, (CompactBlock, Option<Vec<u8>>)>>,
    main_chain: Vec<Vec<u8>>,
    main_chain_tx_hash: Vec<Vec<u8>>,
    candidate_block: Option<(u64, Vec<u8>)>,
    pool: Arc<RwLock<Pool>>,
    auth: Arc<RwLock<Authentication>>,
    genesis: GenesisBlock,
    key_id: u64,
    node_address: Vec<u8>,
}

impl Chain {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        storage_port: u16,
        kms_port: u16,
        executor_port: u16,
        consensus_port: u16,
        block_delay_number: u32,
        current_block_number: u64,
        current_block_hash: Vec<u8>,
        pool: Arc<RwLock<Pool>>,
        auth: Arc<RwLock<Authentication>>,
        genesis: GenesisBlock,
        key_id: u64,
        node_address: Vec<u8>,
    ) -> Self {
        let fork_tree_size = (block_delay_number * 2 + 2) as usize;
        let mut fork_tree = Vec::with_capacity(fork_tree_size);
        for _ in 0..=fork_tree_size {
            fork_tree.push(HashMap::new());
        }

        Chain {
            kms_port,
            storage_port,
            executor_port,
            consensus_port,
            block_number: current_block_number,
            block_hash: current_block_hash,
            block_delay_number,
            fork_tree,
            main_chain: Vec::new(),
            main_chain_tx_hash: Vec::new(),
            candidate_block: None,
            pool,
            auth,
            genesis,
            key_id,
            node_address,
        }
    }

    pub async fn init(&self, init_block_number: u64) {
        if init_block_number == 0 {
            info!("finalize genesis block");
            self.finalize_block(
                self.genesis.genesis_block(),
                vec![0u8; 32],
                self.block_hash.clone(),
                false,
            )
            .await;
        }
    }

    pub fn get_block_number(&self, is_pending: bool) -> u64 {
        if is_pending {
            self.block_number + self.main_chain.len() as u64
        } else {
            self.block_number
        }
    }

    pub async fn proc_sync_block(&mut self) {
        loop {
            let h = self.block_number + 1;
            if let Some((block, proof)) = get_block(h).await {
                let block_clone = block.clone();
                let block_header = block_clone.header.unwrap();
                let block_body = block_clone.body.unwrap();

                let height = block_header.height;
                if height != h {
                    panic!("proc_sync_block {} invalid block height", h)
                }
                let prevhash = block_header.prevhash.clone();
                if prevhash != self.block_hash {
                    panic!("proc_sync_block {} invalid block prevhash", h)
                }

                // check tx in block
                {
                    let tx_hash_list = block_body.tx_hashes;
                    let mut is_valid = true;
                    let pool = self.pool.read().await;
                    for hash in tx_hash_list.iter() {
                        if !pool.contains(hash) {
                            is_valid = false;
                            break;
                        }
                    }
                    if !is_valid {
                        warn!("find tx in sync block {} failed", height);
                        break;
                    }
                }

                let mut block_header_bytes = Vec::new();
                block_header
                    .encode(&mut block_header_bytes)
                    .expect("encode block header failed");

                let block_hash = {
                    let ret = hash_data(self.kms_port, block_header_bytes).await;
                    if ret.is_err() {
                        warn!("hash_data failed {:?}", ret);
                        return;
                    }
                    ret.unwrap()
                };
                let (pre_state_root, pre_proof) = {
                    self.extract_proposal_info(height)
                        .await
                        .expect("extract_proposal_info failed")
                };
                {
                    let mut proposal = Vec::new();
                    proposal.extend_from_slice(&block_hash); // len 32
                    proposal.extend_from_slice(&pre_state_root); // len 32
                    proposal.extend_from_slice(&pre_proof); // len unknown
                    let ret =
                        check_block(self.consensus_port, height, proposal, proof.clone()).await;
                    if ret.is_err() || !ret.unwrap() {
                        panic!("check_block failed")
                    }
                }
                // finalized block
                self.finalize_block(block, proof, block_hash.clone(), true)
                    .await;

                self.block_number += 1;
                self.block_hash = block_hash;

                self.clear_proposal();

                // renew main_chain
                self.main_chain = Vec::new();
                self.main_chain_tx_hash = Vec::new();

                // update fork_tree
                let new_fork_tree = self.fork_tree.split_off(1);
                for map in self.fork_tree.iter() {
                    for (block_hash, _) in map.iter() {
                        let filename = hex::encode(&block_hash);
                        remove_proposal(&filename).await;
                    }
                }
                self.fork_tree = new_fork_tree;
                self.fork_tree
                    .resize(self.block_delay_number as usize * 2 + 2, HashMap::new());
                info!("sync block to {}", height);
            } else {
                info!("sync break at {}", self.block_number);
                break;
            }
        }
    }

    pub async fn get_block_by_number(&self, block_number: u64) -> Option<CompactBlock> {
        if block_number == 0 {
            let genesis_block = self.genesis.genesis_block();
            Some(genesis_block)
        } else {
            get_block(block_number).await.map(|t| t.0)
        }
    }

    async fn extract_proposal_info(&self, h: u64) -> Option<(Vec<u8>, Vec<u8>)> {
        let pre_h = h - self.block_delay_number as u64 - 1;
        let key = pre_h.to_be_bytes().to_vec();

        let state_root = {
            let ret = load_data(self.storage_port, 6, key.clone()).await;
            if ret.is_err() {
                warn!("get_proposal get state_root failed");
                return None;
            }
            ret.unwrap()
        };

        let proof = {
            let ret = get_block(pre_h).await;
            if ret.is_none() {
                warn!("get_proposal get proof failed");
                return None;
            }
            ret.unwrap().1
        };

        Some((state_root, proof))
    }

    pub async fn get_proposal(&self) -> Option<(u64, Vec<u8>)> {
        if let Some((h, ref blk_hash)) = self.candidate_block {
            if let Some((state_root, proof)) = self.extract_proposal_info(h).await {
                let mut proposal = Vec::new();
                proposal.extend_from_slice(blk_hash); // len 32
                proposal.extend_from_slice(&state_root); // len 32
                proposal.extend_from_slice(&proof); // len unknown
                return Some((h, proposal));
            }
        }
        None
    }

    pub async fn add_remote_proposal(&mut self, block: CompactBlock) -> bool {
        let header = block.clone().header.unwrap();
        let block_height = header.height;
        if block_height <= self.block_number {
            warn!("block_height {} too low", block_height);
            return false;
        }

        if block_height - self.block_number > (self.block_delay_number * 2 + 2) as u64 {
            warn!("block_height {} too high", block_height);
            return false;
        }

        let mut block_header_bytes = Vec::new();
        header
            .encode(&mut block_header_bytes)
            .expect("encode block header failed");

        let ret = hash_data(self.kms_port, block_header_bytes).await;
        if let Ok(block_hash) = ret {
            info!("add remote proposal {}", hex::encode(&block_hash));
            self.fork_tree[block_height as usize - self.block_number as usize - 1]
                .entry(block_hash)
                .or_insert((block, None));
        } else {
            warn!("hash_data failed {:?}", ret);
        }
        true
    }

    pub async fn add_proposal(&mut self) {
        info!("main_chain_tx_hash len {}", self.main_chain_tx_hash.len());
        let mut filtered_tx_hash_list = Vec::new();
        // if we are no lucky, all tx is dup, try again
        for _ in 0..6usize {
            let tx_hash_list = {
                let pool = self.pool.read().await;
                pool.package()
            };

            info!("before filter tx hash list len {}", tx_hash_list.len());
            // this means that pool is empty
            // so don't need to retry
            if tx_hash_list.is_empty() {
                break;
            }

            // remove dup tx
            for hash in tx_hash_list.into_iter() {
                if !self.main_chain_tx_hash.contains(&hash) {
                    filtered_tx_hash_list.push(hash);
                }
            }

            info!(
                "after filter tx hash list len {}",
                filtered_tx_hash_list.len()
            );
            if !filtered_tx_hash_list.is_empty() {
                break;
            }
        }

        let mut data = Vec::new();
        for hash in filtered_tx_hash_list.iter() {
            data.extend_from_slice(hash);
        }
        let transactions_root;
        {
            let ret = hash_data(self.kms_port, data).await;
            if ret.is_err() {
                warn!("hash_data failed {:?}", ret);
                return;
            } else {
                transactions_root = ret.unwrap();
            }
        }

        let prevhash = if self.main_chain.is_empty() {
            self.block_hash.clone()
        } else {
            self.main_chain.last().unwrap().to_owned()
        };
        let height = self.block_number + self.main_chain.len() as u64 + 1;
        info!("proposal {} prevhash {}", height, hex::encode(&prevhash));
        let header = BlockHeader {
            prevhash,
            timestamp: unix_now(),
            height,
            transactions_root,
            proposer: self.node_address.clone(),
        };
        let body = CompactBlockBody {
            tx_hashes: filtered_tx_hash_list,
        };
        let block = CompactBlock {
            version: 0,
            header: Some(header.clone()),
            body: Some(body),
        };

        let mut block_header_bytes = Vec::new();
        header
            .encode(&mut block_header_bytes)
            .expect("encode block header failed");

        let block_hash;
        {
            let ret = hash_data(self.kms_port, block_header_bytes).await;
            if ret.is_err() {
                warn!("hash_data failed {:?}", ret);
                return;
            } else {
                block_hash = ret.unwrap();
            }
        }

        info!(
            "proposal {} block_hash {}",
            height,
            hex::encode(&block_hash)
        );
        self.candidate_block = Some((height, block_hash.clone()));
        self.fork_tree[self.main_chain.len()].insert(block_hash.clone(), (block.clone(), None));

        let is_exists = check_proposal_exists(block_hash.as_slice());
        if !is_exists {
            let mut block_bytes = Vec::new();
            block.encode(&mut block_bytes).expect("encode block failed");
            write_proposal(&block_hash, &block_bytes).await;
        }
    }

    pub fn clear_proposal(&mut self) {
        self.candidate_block = None;
    }

    pub async fn check_proposal(&self, h: u64, proposal: &[u8]) -> bool {
        if proposal.len() < 32 + 32 {
            warn!("proposal length invalid");
            return false;
        }

        // old proposal
        if h <= self.block_number {
            return true;
        }

        if h > self.block_number + self.fork_tree.len() as u64 {
            warn!("proposal {} too high", h);
            return false;
        }

        let block_hash = &proposal[..32];
        let proposal_state_root = &proposal[32..64];
        let proposal_proof = &proposal[64..];

        if let Some((blk, _)) =
            self.fork_tree[h as usize - self.block_number as usize - 1].get(block_hash)
        {
            let block_body = blk.clone().body.unwrap();
            // check tx in block
            {
                let tx_hash_list = block_body.tx_hashes;
                let pool = self.pool.read().await;
                for hash in tx_hash_list.iter() {
                    if !pool.contains(hash) {
                        warn!("can't find tx {} in proposal {}", hex::encode(&hash), h);
                        return false;
                    }
                }
            }

            let pre_h = h - self.block_delay_number as u64 - 1;
            let key = pre_h.to_be_bytes().to_vec();

            let state_root = {
                let ret = load_data(self.storage_port, 6, key).await;
                if ret.is_err() {
                    warn!("check_proposal get state_root failed");
                    return false;
                }
                ret.unwrap()
            };

            let proof = {
                let ret = get_block(pre_h).await;
                if ret.is_none() {
                    warn!("check_proposal get proof failed");
                    return false;
                }
                ret.unwrap().1
            };

            if proposal_state_root == state_root && proposal_proof == proof {
                return true;
            } else {
                warn!("check_proposal failed!\nproposal_state_root {}\nstate_root {}\nproposal_proof {}\nproof {}",
                    hex::encode(&proposal_state_root),
                    hex::encode(&state_root),
                    hex::encode(&proposal_proof),
                    hex::encode(&proof),
                );
                return false;
            }
        }

        warn!(
            "can't find proposal block {} in fork tree",
            hex::encode(&block_hash)
        );
        false
    }

    async fn finalize_block(
        &self,
        block: CompactBlock,
        proof: Vec<u8>,
        block_hash: Vec<u8>,
        is_sync: bool,
    ) {
        let block_clone = block.clone();
        let block_header = block.header.unwrap();
        let block_body = block.body.unwrap();
        let block_height = block_header.height;
        let key = block_height.to_be_bytes().to_vec();

        info!("finalize_block: {}", block_height);

        // region 5 : block_height - proof
        // store_data(self.storage_port, 5, key.clone(), proof.to_owned())
        //    .await
        //    .expect("store proof failed");

        // region 4 : block_height - block hash
        store_data(self.storage_port, 4, key.clone(), block_hash.clone())
            .await
            .expect("store_data failed");

        // region 8 : block hash - block_height
        store_data(self.storage_port, 8, block_hash.clone(), key.clone())
            .await
            .expect("store_data failed");

        if !is_sync || !check_block_exists(block_height) {
            // region 3: block_height - block body
            let mut block_body_bytes = Vec::new();
            block_body
                .encode(&mut block_body_bytes)
                .expect("encode block body failed");
            // store_data(self.storage_port, 3, key.clone(), block_body_bytes)
            //    .await
            //    .expect("store_data failed");

            // region 2: block_height - block header
            let mut block_header_bytes = Vec::new();
            block_header
                .encode(&mut block_header_bytes)
                .expect("encode block header failed");
            // store_data(self.storage_port, 2, key.clone(), block_header_bytes)
            //    .await
            //    .expect("store_data failed");

            // store block with proof in sync folder.
            write_block(
                block_height,
                block_header_bytes.as_slice(),
                block_body_bytes.as_slice(),
                proof.as_slice(),
            )
            .await;
        }

        // region 1: tx_hash - tx
        let tx_hash_list = block_body.tx_hashes;
        {
            for (tx_index, hash) in tx_hash_list.iter().enumerate() {
                move_tx(&hash).await;
                let raw_tx = get_tx(&hash).await.expect("get tx failed");
                // if tx is utxo tx, update sys_config
                {
                    if let UtxoTx(utxo_tx) = raw_tx.clone().tx.unwrap() {
                        let ret = {
                            let mut auth = self.auth.write().await;
                            auth.update_system_config(&utxo_tx)
                        };
                        if ret {
                            // if sys_config changed, store utxo tx hash into global region
                            let lock_id = utxo_tx.transaction.unwrap().lock_id;
                            let key = lock_id.to_be_bytes().to_vec();
                            let tx_hash = utxo_tx.transaction_hash;
                            store_data(self.storage_port, 0, key, tx_hash)
                                .await
                                .expect("store_data failed");

                            if lock_id == LOCK_ID_VALIDATORS || lock_id == LOCK_ID_BLOCK_INTERVAL {
                                let sys_config = {
                                    let auth = self.auth.read().await;
                                    auth.get_system_config()
                                };
                                reconfigure(self.consensus_port, block_height, sys_config)
                                    .await
                                    .expect("reconfigure failed");
                            }
                        }
                    }
                }

                // store tx info
                store_tx_info(&hash, block_height, tx_index as u64).await;
            }
        }

        // exec block
        // if exec_block after consensus, we should ignore the error, because all node will have same error.
        // if exec_block before consensus, we shouldn't ignore, because it means that block is invalid.
        // TODO: get length of hash from kms
        let executed_block_hash = exec_block(self.executor_port, block_clone)
            .await
            .unwrap_or_else(|_| vec![0u8; 32]);
        // region 6 : block_height - executed_block_hash
        store_data(self.storage_port, 6, key.clone(), executed_block_hash)
            .await
            .expect("store result failed");

        // this must be before update pool
        {
            let mut auth = self.auth.write().await;
            auth.insert_tx_hash(block_height, tx_hash_list.clone());
        }
        // update pool
        {
            let mut pool = self.pool.write().await;
            pool.update(tx_hash_list);
        }

        // region 0: 0 - current height; 1 - current hash
        store_data(self.storage_port, 0, 0u64.to_be_bytes().to_vec(), key)
            .await
            .expect("store_data failed");
        store_data(
            self.storage_port,
            0,
            1u64.to_be_bytes().to_vec(),
            block_hash.to_owned(),
        )
        .await
        .expect("store_data failed");
    }

    pub async fn commit_block(&mut self, height: u64, proposal: &[u8], proof: &[u8]) {
        let proposal_blk_hash = &proposal[..32];
        info!(
            "commit_block height {}: {}",
            height,
            hex::encode(&proposal_blk_hash)
        );

        let commit_block_index;
        let commit_block;
        for (index, map) in self.fork_tree.iter_mut().enumerate() {
            // make sure the block in fork_tree
            if let Some((block, proof_opt)) = map.get_mut(proposal_blk_hash) {
                commit_block_index = index;
                commit_block = block.clone();

                // store proof
                *proof_opt = Some(proof.to_owned());

                // try to backwards found a candidate_chain
                let mut candidate_chain = Vec::new();
                let mut candidate_chain_tx_hash = Vec::new();

                candidate_chain.push(proposal_blk_hash.to_owned());
                candidate_chain_tx_hash.extend_from_slice(&commit_block.body.unwrap().tx_hashes);

                let mut prevhash = commit_block.header.unwrap().prevhash;
                for i in 0..commit_block_index {
                    let map = self.fork_tree.get(commit_block_index - i - 1).unwrap();
                    if let Some((block, proof_opt)) = map.get(&prevhash) {
                        if proof_opt.is_none() {
                            warn!("candidate_chain has no proof");
                            return;
                        }
                        candidate_chain.push(prevhash.clone());
                        for hash in block.to_owned().body.unwrap().tx_hashes {
                            if candidate_chain_tx_hash.contains(&hash) {
                                // candidate_chain has dup tx, so failed
                                warn!("candidate_chain has dup tx");
                                return;
                            }
                        }
                        candidate_chain_tx_hash
                            .extend_from_slice(&block.to_owned().body.unwrap().tx_hashes);
                        prevhash = block.to_owned().header.unwrap().prevhash;
                    } else {
                        // candidate_chain interrupted, so failed
                        warn!("candidate_chain interrupted");
                        return;
                    }
                }

                if prevhash != self.block_hash {
                    warn!("candidate_chain can't fit finalized block");
                    // break this invalid chain
                    let blk_hash = candidate_chain.last().unwrap();
                    self.fork_tree.get_mut(0).unwrap().remove(blk_hash);
                    let filename = hex::encode(blk_hash);
                    remove_proposal(filename.as_str()).await;
                    return;
                }

                // if candidate_chain longer than original main_chain
                if candidate_chain.len() > self.main_chain.len() {
                    // replace the main_chain
                    candidate_chain.reverse();
                    self.main_chain = candidate_chain;
                    self.main_chain_tx_hash = candidate_chain_tx_hash;
                    print_main_chain(&self.main_chain, self.block_number);
                    // check if any block has been finalized
                    if self.main_chain.len() > self.block_delay_number as usize {
                        let finalized_blocks_number =
                            self.main_chain.len() - self.block_delay_number as usize;
                        info!("{} blocks finalized", finalized_blocks_number);
                        let new_main_chain = self.main_chain.split_off(finalized_blocks_number);
                        let mut finalized_tx_hash_list = Vec::new();
                        // save finalized blocks / txs / current height / current hash
                        for (index, block_hash) in self.main_chain.iter().enumerate() {
                            // get block
                            let (block, proof_opt) =
                                self.fork_tree[index].get(block_hash).unwrap().to_owned();
                            self.finalize_block(
                                block.to_owned(),
                                proof_opt.unwrap(),
                                block_hash.to_owned(),
                                false,
                            )
                            .await;
                            let block_body = block.to_owned().body.unwrap();
                            finalized_tx_hash_list
                                .extend_from_slice(block_body.tx_hashes.as_slice());
                        }
                        self.block_number += finalized_blocks_number as u64;
                        self.block_hash = self.main_chain[finalized_blocks_number - 1].to_owned();
                        self.main_chain = new_main_chain;
                        // update main_chain_tx_hash
                        self.main_chain_tx_hash = self
                            .main_chain_tx_hash
                            .iter()
                            .cloned()
                            .filter(|hash| !finalized_tx_hash_list.contains(hash))
                            .collect();
                        let new_fork_tree = self.fork_tree.split_off(finalized_blocks_number);
                        for map in self.fork_tree.iter() {
                            for (block_hash, _) in map.iter() {
                                let filename = hex::encode(&block_hash);
                                remove_proposal(&filename).await;
                            }
                        }
                        self.fork_tree = new_fork_tree;
                        self.fork_tree
                            .resize(self.block_delay_number as usize * 2 + 2, HashMap::new());
                    }
                    // candidate_block need update
                    self.clear_proposal();
                }
                break;
            }
        }
    }
}
