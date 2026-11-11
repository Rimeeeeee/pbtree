use crate::{
    BinaryTree, Blake3Hasher, TreeError, TreeHasher,
    key::{self, BASIC_DATA_LEAF_KEY, CODE_HASH_LEAF_KEY, DELEGATION_LEAF_KEY},
};
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use imbl::{OrdMap, ordmap::Entry};
use std::collections::BTreeMap;

/// Ethereum state encoded into a single partitioned binary tree.
///
/// Account preimages and code references are retained alongside the commitment. Cloning gives
/// an independent state snapshot. This in-memory backend is intended for reference execution
/// and devnets; client database and historical-state providers remain separate.
#[derive(Clone, Debug)]
pub struct StateTree<H = Blake3Hasher> {
    tree: BinaryTree<H>,
    accounts: OrdMap<Address, AccountData>,
    codes: OrdMap<B256, CodeReference>,
    storages: OrdMap<Address, OrdMap<Vec<u8>, (U256, B256)>>,
    pending: Option<BTreeMap<Vec<u8>, Option<B256>>>,
    #[cfg(feature = "persistence")]
    addresses: OrdMap<B256, Address>,
    #[cfg(feature = "persistence")]
    empty_accounts: usize,
    #[cfg(feature = "persistence")]
    total_code_bytes: u128,
    #[cfg(feature = "persistence")]
    code_sizes: OrdMap<usize, usize>,
}

impl<H: TreeHasher> Default for StateTree<H> {
    fn default() -> Self {
        Self {
            tree: BinaryTree::new(),
            accounts: OrdMap::default(),
            codes: OrdMap::default(),
            storages: OrdMap::default(),
            pending: None,
            #[cfg(feature = "persistence")]
            addresses: OrdMap::default(),
            #[cfg(feature = "persistence")]
            empty_accounts: 0,
            #[cfg(feature = "persistence")]
            total_code_bytes: 0,
            #[cfg(feature = "persistence")]
            code_sizes: OrdMap::default(),
        }
    }
}

impl<H: TreeHasher> StateTree<H> {
    /// Creates empty state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the complete state commitment.
    pub fn root(&self) -> B256 {
        debug_assert!(self.pending.is_none(), "finish the internal batch before publishing a root");
        self.tree.root()
    }

    /// Returns the underlying commitment for iteration and proof construction.
    pub const fn tree(&self) -> &BinaryTree<H> {
        &self.tree
    }

    /// Atomically applies a sequence of account/storage changes to a new snapshot.
    /// Invalid updates leave the original state unchanged. Large batches rebuild
    /// their final leaf set once instead of rehashing shared paths per update.
    pub fn apply_batch(
        &mut self,
        updates: impl IntoIterator<Item = StateUpdate>,
    ) -> Result<(), StateError> {
        let mut next = self.clone();
        next.begin_batch();
        for update in updates {
            match update {
                StateUpdate::Account { address, account } => {
                    next.apply_account(address, account)?
                }
                StateUpdate::Storage { address, slot, value } => {
                    next.set_storage(address, slot, value)?
                }
                StateUpdate::ClearStorage { address } => next.clear_storage(address),
            }
        }
        next.finish_batch()?;
        *self = next;
        Ok(())
    }

    pub(crate) fn begin_batch(&mut self) {
        assert!(self.pending.is_none(), "batches cannot be nested");
        self.pending = Some(BTreeMap::new());
    }

    pub(crate) fn finish_batch(&mut self) -> Result<(), StateError> {
        if let Some(updates) = self.pending.take() {
            self.tree.apply_updates(updates)?;
        }
        Ok(())
    }

    fn remove_leaf(&mut self, key: &[u8]) {
        if let Some(pending) = &mut self.pending {
            pending.insert(key.to_vec(), None);
        } else {
            self.tree.remove(key);
        }
    }

    /// Returns retained account data, or `None` for an absent account.
    pub fn account(&self, address: Address) -> Option<&AccountData> {
        self.accounts.get(&address)
    }

    /// Iterates over account preimages and data in address order.
    pub fn accounts(&self) -> impl Iterator<Item = (Address, &AccountData)> {
        self.accounts.iter().map(|(address, account)| (*address, account))
    }

    /// Iterates over an account's nonzero storage and retained plain slot preimages.
    /// Entries follow derived PBT key order, not plain slot order.
    pub fn storage_entries(&self, address: Address) -> impl Iterator<Item = (U256, B256)> + '_ {
        self.storages.get(&address).into_iter().flat_map(|slots| slots.values().copied())
    }

    /// Replaces account header/code data while preserving its storage.
    ///
    /// This encodes even an explicitly supplied empty account, as required by the standalone
    /// embedding vectors. Use [`Self::apply_account`] for post-Merge state clearing. Invalid
    /// balances are rejected before mutation. Shared code survives deletion of one owner.
    pub fn set_account(
        &mut self,
        address: Address,
        account: AccountData,
    ) -> Result<(), StateError> {
        let basic = account.basic_data()?;
        let previous = self.accounts.get(&address).cloned();
        if previous.as_ref() == Some(&account) {
            return Ok(());
        }
        #[cfg(feature = "persistence")]
        {
            self.empty_accounts -=
                usize::from(previous.as_ref().is_some_and(AccountData::is_empty));
            self.empty_accounts += usize::from(account.is_empty());
        }
        let position = key::account_hash::<H>(address);
        let same_code = previous.as_ref().is_some_and(|previous| previous.code == account.code);
        let previous_hash =
            (!same_code).then(|| previous.as_ref().and_then(AccountData::contract_hash)).flatten();
        let next_hash = (!same_code).then(|| account.contract_hash()).flatten();
        if previous_hash != next_hash {
            if let Some(hash) = previous_hash {
                self.release_code(hash);
            }
            if let Some(hash) = next_hash {
                let reference = self
                    .codes
                    .entry(hash)
                    .or_insert_with(|| CodeReference { count: 0, code: account.code.clone() });
                reference.count += 1;
                if reference.count == 1 {
                    #[cfg(feature = "persistence")]
                    {
                        let size = account.code.len();
                        self.total_code_bytes += size as u128;
                        *self.code_sizes.entry(size).or_default() += 1;
                    }
                    for (index, chunk) in key::chunkify_code(&account.code).into_iter().enumerate()
                    {
                        self.write(key::code_key::<H>(hash, index as u64).to_vec(), chunk);
                    }
                }
            }
        }
        self.write(key::header_key_from_hash(position, BASIC_DATA_LEAF_KEY).to_vec(), basic);
        if same_code {
            // Only basic data changed; code ownership and code/delegation leaves
            // already describe the same bytes in this snapshot.
        } else if account.is_delegation() {
            self.remove_leaf(&key::header_key_from_hash(position, CODE_HASH_LEAF_KEY));
            let mut value = [0; 32];
            value[..23].copy_from_slice(&account.code);
            self.write(
                key::header_key_from_hash(position, DELEGATION_LEAF_KEY).to_vec(),
                B256::from(value),
            );
        } else {
            self.remove_leaf(&key::header_key_from_hash(position, DELEGATION_LEAF_KEY));
            self.write(
                key::header_key_from_hash(position, CODE_HASH_LEAF_KEY).to_vec(),
                next_hash.unwrap_or_else(|| keccak256(&account.code)),
            );
        }
        self.accounts.insert(address, account);
        #[cfg(feature = "persistence")]
        self.addresses.insert(position, address);
        Ok(())
    }

    /// Applies an execution account update, clearing empty or destroyed accounts.
    pub fn apply_account(
        &mut self,
        address: Address,
        account: Option<AccountData>,
    ) -> Result<(), StateError> {
        match account {
            Some(account) if !account.is_empty() => self.set_account(address, account),
            _ => {
                self.remove_account(address);
                Ok(())
            }
        }
    }

    /// Deletes an account's header, all its storage, and code no remaining account uses.
    pub fn remove_account(&mut self, address: Address) -> Option<AccountData> {
        let account = self.accounts.remove(&address)?;
        #[cfg(feature = "persistence")]
        {
            self.empty_accounts -= usize::from(account.is_empty());
        }
        #[cfg(feature = "persistence")]
        self.addresses.remove(&key::account_hash::<H>(address));
        if let Some(hash) = account.contract_hash() {
            self.release_code(hash);
        }
        self.clear_storage(address);
        for index in [BASIC_DATA_LEAF_KEY, CODE_HASH_LEAF_KEY, DELEGATION_LEAF_KEY] {
            self.remove_leaf(&key::header_key::<H>(address, index));
        }
        Some(account)
    }

    /// Reads a plain storage slot. Absent slots read as zero.
    pub fn storage(&self, address: Address, slot: U256) -> B256 {
        let key = key::storage_key::<H>(address, slot);
        self.pending
            .as_ref()
            .and_then(|pending| pending.get(&key))
            .copied()
            .unwrap_or_else(|| self.tree.get(&key))
            .unwrap_or_default()
    }

    /// Writes a plain slot; a zero value deletes the corresponding leaf.
    pub fn set_storage(
        &mut self,
        address: Address,
        slot: U256,
        value: B256,
    ) -> Result<(), StateError> {
        if !self.accounts.contains_key(&address) {
            return Err(StateError::MissingAccount(address));
        }
        let key = key::storage_key::<H>(address, slot);
        if self
            .storages
            .get(&address)
            .and_then(|slots| slots.get(&key))
            .map_or(B256::ZERO, |(_, value)| *value)
            == value
        {
            return Ok(());
        }
        self.write(key.clone(), value);
        let storages = &mut self.storages;
        if value.is_zero() {
            if let Some(slots) = storages.get_mut(&address) {
                slots.remove(&key);
                if slots.is_empty() {
                    storages.remove(&address);
                }
            }
        } else {
            storages.entry(address).or_default().insert(key, (slot, value));
        }
        Ok(())
    }

    /// Clears storage while retaining the account header and code.
    pub fn clear_storage(&mut self, address: Address) {
        if let Some(slots) = self.storages.remove(&address) {
            for (key, _) in slots {
                self.remove_leaf(&key);
            }
        }
    }

    fn release_code(&mut self, hash: B256) {
        let entry = self.codes.entry(hash);
        assert!(matches!(&entry, Entry::Occupied(_)), "contract reference is tracked");
        if let Entry::Occupied(mut entry) = entry {
            let reference = entry.get_mut();
            assert!(reference.count > 0, "contract reference has an owner");
            reference.count -= 1;
            if reference.count == 0 {
                let size = entry.remove().code.len();
                #[cfg(feature = "persistence")]
                {
                    self.total_code_bytes -= size as u128;
                    let count = self.code_sizes.get_mut(&size).expect("code size is tracked");
                    *count -= 1;
                    if *count == 0 {
                        self.code_sizes.remove(&size);
                    }
                }
                for index in 0..size.div_ceil(31) {
                    self.remove_leaf(&key::code_key::<H>(hash, index as u64));
                }
            }
        }
    }

    fn write(&mut self, key: Vec<u8>, value: B256) {
        if let Some(pending) = &mut self.pending {
            pending.insert(key, (!value.is_zero()).then_some(value));
        } else if value.is_zero() {
            self.tree.remove(&key);
        } else {
            self.tree.insert(key, value).expect("zone-separated state keys are prefix-free");
        }
    }

    #[cfg(feature = "persistence")]
    pub(crate) fn address_preimage(&self, hash: B256) -> Option<Address> {
        self.addresses.get(&hash).copied()
    }

    #[cfg(feature = "persistence")]
    pub(crate) fn storage_preimage(&self, address: Address, key: &[u8]) -> Option<U256> {
        self.storages.get(&address)?.get(key).map(|(slot, _)| *slot)
    }

    #[cfg(feature = "persistence")]
    pub(crate) fn account_count(&self) -> usize {
        self.accounts.len()
    }

    #[cfg(feature = "persistence")]
    pub(crate) fn validation_stats(&self) -> (usize, u128, usize) {
        (
            self.empty_accounts,
            self.total_code_bytes,
            self.code_sizes.get_max().map_or(0, |(size, _)| *size),
        )
    }
}

/// One ordered account or storage transition in an atomic state batch.
#[derive(Clone, Debug)]
pub enum StateUpdate {
    /// Apply account data, or clear the account when absent/empty.
    Account {
        /// Account address.
        address: Address,
        /// New account data, or destruction.
        account: Option<AccountData>,
    },
    /// Replace a slot; zero deletes it. The account must exist at this point.
    Storage {
        /// Owning account.
        address: Address,
        /// Plain storage slot.
        slot: U256,
        /// New value; zero deletes the slot.
        value: B256,
    },
    /// Remove every slot while keeping the account.
    ClearStorage {
        /// Account whose storage is cleared.
        address: Address,
    },
}

/// Account data needed to encode a PBT header and its code leaves.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccountData {
    /// Account nonce.
    pub nonce: u64,
    /// Balance, checked against the 128-bit PBT encoding before writing.
    pub balance: U256,
    /// Complete bytecode, or a 23-byte EIP-7702 delegation indicator.
    pub code: Bytes,
}

impl AccountData {
    /// Packs version/reserved bytes, code size, nonce, and balance, all big-endian.
    pub fn basic_data(&self) -> Result<B256, StateError> {
        let size = u32::try_from(self.code.len()).map_err(|_| StateError::CodeSizeOverflow)?;
        key::encode_basic_data(size, self.nonce, self.balance)
    }

    /// Whether the account is empty under the post-Merge state-clearing invariant.
    pub fn is_empty(&self) -> bool {
        self.nonce == 0 && self.balance.is_zero() && self.code.is_empty()
    }

    /// Whether this code is an EIP-7702 delegation indicator.
    pub fn is_delegation(&self) -> bool {
        self.code.len() == 23 && self.code.starts_with(&[0xef, 0x01, 0x00])
    }

    fn contract_hash(&self) -> Option<B256> {
        (!self.code.is_empty() && !self.is_delegation()).then(|| keccak256(&self.code))
    }
}

#[derive(Clone, Debug)]
struct CodeReference {
    count: usize,
    code: Bytes,
}

/// Invalid state transition input.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StateError {
    /// The balance cannot fit the basic-data leaf's 16-byte balance field.
    #[error("balance exceeds the PBT 128-bit field")]
    BalanceOverflow,
    /// The code length cannot fit the basic-data leaf's four-byte size field.
    #[error("code length exceeds the PBT 32-bit field")]
    CodeSizeOverflow,
    /// Storage cannot be attached to an account absent from the state.
    #[error("storage write to absent account {0}")]
    MissingAccount(Address),
    /// Invalid generic tree input.
    #[error(transparent)]
    Tree(#[from] TreeError),
}

#[cfg(all(test, feature = "persistence"))]
mod stats_tests {
    use super::*;

    #[test]
    fn cached_limits_match_account_scan_across_snapshots_and_code_changes() {
        let mut state = StateTree::<Blake3Hasher>::new();
        let mut random = 1u64;
        for step in 0..512 {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            let address = Address::repeat_byte((random % 8) as u8);
            let snapshot = state.clone();
            let previous = snapshot.validation_stats();
            if step % 7 == 0 {
                state.remove_account(address);
            } else {
                let code = match random % 5 {
                    0 => Vec::new(),
                    1 => [vec![0xef, 1, 0], Address::ZERO.to_vec()].concat(),
                    value => vec![0x7f; value as usize * 31],
                };
                state
                    .set_account(
                        address,
                        AccountData {
                            nonce: u64::from(step % 3 != 0),
                            code: code.into(),
                            ..Default::default()
                        },
                    )
                    .unwrap();
            }
            let empty = state.accounts().filter(|(_, account)| account.is_empty()).count();
            let unique: BTreeMap<_, _> = state
                .accounts()
                .filter_map(|(_, account)| {
                    account.contract_hash().map(|hash| (hash, account.code.len()))
                })
                .collect();
            let total = unique.values().map(|size| *size as u128).sum();
            let maximum = unique.values().copied().max().unwrap_or_default();
            assert_eq!(state.validation_stats(), (empty, total, maximum));
            assert_eq!(snapshot.validation_stats(), previous);
        }
    }
}
