//! Packed branch paths with allocation-free storage for common short prefixes.

#[derive(Clone, Debug)]
pub(crate) enum Prefix {
    Inline { len: u8, bytes: [u8; 7] },
    Heap { len: u16, bytes: Box<[u8]> },
}

impl Prefix {
    pub(crate) fn new(bits: impl ExactSizeIterator<Item = bool>) -> Self {
        let len = bits.len();
        assert!(len <= u16::MAX as usize);
        if len <= 56 {
            let mut bytes = [0; 7];
            for (index, bit) in bits.enumerate() {
                bytes[index / 8] |= u8::from(bit) << (7 - index % 8);
            }
            Self::Inline { len: len as u8, bytes }
        } else {
            let mut bytes = vec![0; len.div_ceil(8)];
            for (index, bit) in bits.enumerate() {
                bytes[index / 8] |= u8::from(bit) << (7 - index % 8);
            }
            Self::Heap { len: len as u16, bytes: bytes.into_boxed_slice() }
        }
    }

    pub(crate) const fn len(&self) -> usize {
        match self {
            Self::Inline { len, .. } => *len as usize,
            Self::Heap { len, .. } => *len as usize,
        }
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        match self {
            Self::Inline { bytes, .. } => &bytes[..self.len().div_ceil(8)],
            Self::Heap { bytes, .. } => bytes,
        }
    }

    pub(crate) fn bit(&self, index: usize) -> bool {
        self.bytes()[index / 8] & (0x80 >> (index % 8)) != 0
    }

    pub(crate) fn iter(&self) -> impl ExactSizeIterator<Item = bool> + '_ {
        (0..self.len()).map(|index| self.bit(index))
    }

    pub(crate) fn to_vec(&self) -> Vec<bool> {
        let mut bits = Vec::with_capacity(self.len());
        for byte in self.bytes() {
            let count = (self.len() - bits.len()).min(8);
            bits.extend_from_slice(&EXPANDED[*byte as usize][..count]);
        }
        bits
    }

    pub(crate) fn prefix(&self, len: usize) -> Self {
        Self::new((0..len).map(|index| self.bit(index)))
    }

    pub(crate) fn suffix(&self, start: usize) -> Self {
        Self::new((start..self.len()).map(|index| self.bit(index)))
    }

    pub(crate) fn concat(&self, direction: bool, child: &Self) -> Self {
        Self::new((0..self.len() + 1 + child.len()).map(|index| {
            if index < self.len() {
                self.bit(index)
            } else if index == self.len() {
                direction
            } else {
                child.bit(index - self.len() - 1)
            }
        }))
    }

    pub(crate) fn matches(&self, key: &[u8], depth: usize) -> bool {
        if self.len() == 0 {
            return true;
        }
        if depth.saturating_add(self.len()) > key.len().saturating_mul(8) {
            return false;
        }
        let shift = depth % 8;
        for (index, expected) in self.bytes().iter().enumerate() {
            let offset = depth / 8 + index;
            let window = (u16::from(key[offset]) << 8)
                | u16::from(key.get(offset + 1).copied().unwrap_or_default());
            let actual = (window >> (8 - shift)) as u8;
            let remaining = self.len() - index * 8;
            let mask = if remaining < 8 { 0xff << (8 - remaining) } else { 0xff };
            if (actual ^ expected) & mask != 0 {
                return false;
            }
        }
        true
    }

    pub(crate) fn common_prefix(&self, key: &[u8], depth: usize) -> usize {
        if self.matches(key, depth) {
            self.len()
        } else {
            self.iter()
                .enumerate()
                .take_while(|(index, bit)| crate::tree::bit(key, depth + index) == Some(*bit))
                .count()
        }
    }
}

const EXPANDED: [[bool; 8]; 256] = {
    let mut table = [[false; 8]; 256];
    let mut byte = 0;
    while byte < 256 {
        let mut bit = 0;
        while bit < 8 {
            table[byte][bit] = byte & (0x80 >> bit) != 0;
            bit += 1;
        }
        byte += 1;
    }
    table
};

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::{collection, prelude::*};

    proptest! {
        #[test]
        fn packed_paths_match_bit_reference(bits in collection::vec(any::<bool>(), 0..200), key in collection::vec(any::<u8>(), 0..40), depth in 0usize..16) {
            let prefix = Prefix::new(bits.iter().copied());
            prop_assert_eq!(&prefix.to_vec(), &bits);
            prop_assert_eq!(prefix.matches(&key, depth), crate::tree::matches_prefix(&key, depth, &bits));
            let matched = bits.iter().enumerate().take_while(|(index, bit)| crate::tree::bit(&key, depth + index) == Some(**bit)).count();
            prop_assert_eq!(prefix.common_prefix(&key, depth), matched);
        }
    }
}
