use alloy_primitives::B256;

/// The consensus-selected hash used for both key derivation and tree merkelization.
pub trait TreeHasher: Clone + std::fmt::Debug + Send + Sync + 'static {
    /// Hashes the concatenation of the supplied byte slices.
    fn hash(parts: &[&[u8]]) -> B256;
}

/// BLAKE3-256, as used by the EIP-8297 reference implementation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Blake3Hasher;

impl TreeHasher for Blake3Hasher {
    fn hash(parts: &[&[u8]]) -> B256 {
        // State keys and branch commitments usually fit in two BLAKE3 blocks.
        // The one-shot path avoids initializing/updating a streaming hasher for
        // each short node; long generic keys retain the streaming fallback.
        if let Some(length) =
            parts.iter().try_fold(0usize, |total, part| total.checked_add(part.len()))
            && length <= 128
        {
            let mut bytes = [0; 128];
            let mut offset = 0;
            for part in parts {
                bytes[offset..offset + part.len()].copy_from_slice(part);
                offset += part.len();
            }
            return B256::from(*blake3::hash(&bytes[..length]).as_bytes());
        }
        let mut hasher = blake3::Hasher::new();
        for part in parts {
            hasher.update(part);
        }
        B256::from(*hasher.finalize().as_bytes())
    }
}

pub(crate) fn leaf_hash<H: TreeHasher>(key: &[u8], value: B256) -> B256 {
    H::hash(&[&[0], key, value.as_slice()])
}

pub(crate) fn branch_hash<H: TreeHasher>(prefix: &[bool], left: B256, right: B256) -> B256 {
    if prefix.len() <= 64 {
        let mut encoded = [0; 10];
        encoded[..2].copy_from_slice(&(prefix.len() as u16).to_be_bytes());
        for (index, bit) in prefix.iter().enumerate() {
            encoded[2 + index / 8] |= u8::from(*bit) << (7 - index % 8);
        }
        return H::hash(&[
            &[1],
            &encoded[..2 + prefix.len().div_ceil(8)],
            left.as_slice(),
            right.as_slice(),
        ]);
    }
    let mut encoded = vec![0; 2 + prefix.len().div_ceil(8)];
    encoded[..2].copy_from_slice(&(prefix.len() as u16).to_be_bytes());
    for (index, bit) in prefix.iter().enumerate() {
        encoded[2 + index / 8] |= u8::from(*bit) << (7 - index % 8);
    }
    H::hash(&[&[1], &encoded, left.as_slice(), right.as_slice()])
}

pub(crate) fn packed_branch_hash<H: TreeHasher>(
    prefix: &crate::prefix::Prefix,
    left: B256,
    right: B256,
) -> B256 {
    H::hash(&[
        &[1],
        &(prefix.len() as u16).to_be_bytes(),
        prefix.bytes(),
        left.as_slice(),
        right.as_slice(),
    ])
}
