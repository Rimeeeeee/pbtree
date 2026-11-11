//! Ethereum state lifecycle and shared-code tests.

use alloy_primitives::{Address, B256, U256, keccak256};
use pbt::{AccountData, Blake3Hasher, StateError, StateTree, StateUpdate, key};
use proptest::{collection, prelude::*};

fn account(code: Vec<u8>) -> AccountData {
    AccountData { nonce: 1, balance: U256::from(5), code: code.into() }
}

proptest! {
    #[test]
    fn ordered_batches_match_serial_state_machine(operations in collection::vec((any::<u8>(), any::<u8>(), any::<u8>()), 0..150)) {
        let mut initial = StateTree::<Blake3Hasher>::new();
        for index in 0..4 { initial.set_account(Address::repeat_byte(index), account(Vec::new())).unwrap(); }
        let mut serial = initial.clone();
        let mut changes = Vec::new();
        let mut failed = false;
        for (address, action, value) in operations {
            let address = Address::repeat_byte(address % 8);
            let result = match action % 4 {
                0 => {
                    let code = if value % 3 == 0 { Vec::new() } else { vec![0x7f; usize::from(value % 4) * 31] };
                    let data = AccountData { nonce: u64::from(value % 3), balance: U256::from(value % 5), code: code.into() };
                    changes.push(StateUpdate::Account { address, account: Some(data.clone()) });
                    serial.apply_account(address, Some(data))
                }
                1 => {
                    changes.push(StateUpdate::Account { address, account: None });
                    serial.apply_account(address, None)
                }
                2 => {
                    let slot = U256::from(value);
                    let new_value = if value % 7 == 0 { B256::ZERO } else { B256::repeat_byte(value) };
                    changes.push(StateUpdate::Storage { address, slot, value: new_value });
                    serial.set_storage(address, slot, new_value)
                }
                _ => {
                    changes.push(StateUpdate::ClearStorage { address });
                    serial.clear_storage(address);
                    Ok(())
                }
            };
            if result.is_err() { failed = true; break; }
        }
        let mut batched = initial.clone();
        #[cfg(feature = "parallel")]
        let mut parallel = initial.clone();
        #[cfg(feature = "parallel")]
        let parallel_result = parallel.apply_batch_parallel(changes.clone());
        let result = batched.apply_batch(changes);
        #[cfg(feature = "parallel")]
        {
            prop_assert_eq!(&parallel_result, &result);
            prop_assert_eq!(parallel.root(), batched.root());
            prop_assert_eq!(parallel.tree().entries().collect::<Vec<_>>(), batched.tree().entries().collect::<Vec<_>>());
        }
        if failed {
            prop_assert!(result.is_err());
            prop_assert_eq!(batched.root(), initial.root());
        } else {
            prop_assert!(result.is_ok());
            prop_assert_eq!(batched.root(), serial.root());
            prop_assert_eq!(batched.tree().entries().collect::<Vec<_>>(), serial.tree().entries().collect::<Vec<_>>());
        }
    }
}

#[test]
fn state_batches_match_serial_transitions_and_reject_atomically() {
    let address = Address::repeat_byte(1);
    let mut batched = StateTree::<Blake3Hasher>::new();
    let mut serial = StateTree::<Blake3Hasher>::new();
    let mut updates =
        vec![StateUpdate::Account { address, account: Some(account(vec![0x7f; 95])) }];
    serial.set_account(address, account(vec![0x7f; 95])).unwrap();
    for slot in 0..256 {
        let slot = U256::from(slot);
        let value = B256::from(slot + U256::from(1));
        updates.push(StateUpdate::Storage { address, slot, value });
        serial.set_storage(address, slot, value).unwrap();
    }
    updates.push(StateUpdate::ClearStorage { address });
    serial.clear_storage(address);
    updates.push(StateUpdate::Storage { address, slot: U256::MAX, value: B256::repeat_byte(5) });
    serial.set_storage(address, U256::MAX, B256::repeat_byte(5)).unwrap();
    batched.apply_batch(updates).unwrap();
    assert_eq!(batched.root(), serial.root());
    assert_eq!(
        batched.tree().entries().collect::<Vec<_>>(),
        serial.tree().entries().collect::<Vec<_>>()
    );
    let snapshot = batched.clone();
    assert!(
        batched
            .apply_batch([
                StateUpdate::Account { address, account: None },
                StateUpdate::Storage { address, slot: U256::ZERO, value: B256::repeat_byte(1) },
            ])
            .is_err()
    );
    assert_eq!(batched.root(), snapshot.root());
    assert_eq!(batched.account(address), snapshot.account(address));
}

#[test]
fn deleting_shared_code_preserves_other_owner() {
    let a = Address::repeat_byte(1);
    let b = Address::repeat_byte(2);
    let code = vec![1; 31 * 257];
    let mut state = StateTree::<Blake3Hasher>::new();
    state.set_account(a, account(code.clone())).unwrap();
    let a_only = state.clone();
    state.set_account(b, account(code.clone())).unwrap();
    assert_eq!(state.tree().len(), 261);
    state.remove_account(b);
    assert_eq!(state.root(), a_only.root());
    assert!(state.tree().get(&key::code_key::<Blake3Hasher>(keccak256(&code), 256)).is_some());
    state.remove_account(a);
    assert_eq!(state.root(), B256::ZERO);
}

#[test]
fn delegation_replacement_and_clearing() {
    let a = Address::repeat_byte(1);
    let mut state = StateTree::<Blake3Hasher>::new();
    state.set_account(a, account(vec![1; 100])).unwrap();
    let mut indicator = vec![0xef, 1, 0];
    indicator.extend_from_slice(Address::repeat_byte(2).as_slice());
    state.set_account(a, account(indicator)).unwrap();
    assert_eq!(state.tree().len(), 2);
    assert!(state.tree().get(&key::header_key::<Blake3Hasher>(a, 1)).is_none());
    assert!(state.tree().get(&key::header_key::<Blake3Hasher>(a, 2)).is_some());
    state.set_account(a, account(Vec::new())).unwrap();
    assert_eq!(state.tree().len(), 2);
    assert_eq!(state.tree().get(&key::header_key::<Blake3Hasher>(a, 1)), Some(keccak256([])));
    assert!(state.tree().get(&key::header_key::<Blake3Hasher>(a, 2)).is_none());
}

#[test]
fn storage_wipe_and_account_clear_cover_both_zones() {
    let a = Address::repeat_byte(1);
    let b = Address::repeat_byte(2);
    let mut state = StateTree::<Blake3Hasher>::new();
    for address in [a, b] {
        state.set_account(address, account(Vec::new())).unwrap();
        for slot in [0, 63, 64, 255, 256] {
            state.set_storage(address, U256::from(slot), B256::repeat_byte(1)).unwrap();
        }
    }
    let snapshot = state.clone();
    state.clear_storage(a);
    for slot in [0, 63, 64, 255, 256] {
        assert_eq!(state.storage(a, U256::from(slot)), B256::ZERO);
        assert_eq!(state.storage(b, U256::from(slot)), B256::repeat_byte(1));
        assert_eq!(snapshot.storage(a, U256::from(slot)), B256::repeat_byte(1));
    }
    state.apply_account(b, Some(AccountData::default())).unwrap();
    let mut expected = StateTree::<Blake3Hasher>::new();
    expected.set_account(a, account(Vec::new())).unwrap();
    assert_eq!(state.root(), expected.root());
}

#[test]
fn zero_writes_do_not_leave_leaves() {
    let a = Address::repeat_byte(1);
    let mut state = StateTree::<Blake3Hasher>::new();
    state.set_account(a, account(vec![0; 62])).unwrap();
    assert_eq!(state.tree().len(), 2);
    let before = state.root();
    state.set_storage(a, U256::MAX, B256::repeat_byte(1)).unwrap();
    state.set_storage(a, U256::MAX, B256::ZERO).unwrap();
    assert_eq!(state.root(), before);
    assert!(state.tree().entries().all(|(_, value)| !value.is_zero()));
}

#[test]
fn invalid_account_is_rejected_atomically() {
    let a = Address::repeat_byte(1);
    let mut state = StateTree::<Blake3Hasher>::new();
    state.set_account(a, account(vec![1])).unwrap();
    let root = state.root();
    assert_eq!(
        state.set_account(a, AccountData { balance: U256::MAX, ..account(vec![2]) }),
        Err(StateError::BalanceOverflow)
    );
    assert_eq!(state.root(), root);
    assert_eq!(state.account(a).unwrap().code.as_ref(), [1]);
    assert_eq!(
        state.set_storage(Address::ZERO, U256::ZERO, B256::ZERO),
        Err(StateError::MissingAccount(Address::ZERO))
    );
}

#[test]
fn push32_can_continue_across_two_boundaries() {
    let mut code = vec![1; 30];
    code.push(0x7f);
    code.extend_from_slice(&[0x7f; 32]);
    code.push(0x00);
    let chunks = key::chunkify_code(&code);
    assert_eq!(chunks[0][0], 0);
    assert_eq!(chunks[1][0], 31);
    assert_eq!(chunks[2][0], 1);
}
