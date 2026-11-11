#![no_main]

use alloy_primitives::{B256, b256};
use libfuzzer_sys::fuzz_target;
use pbt::{
    Blake3Hasher,
    migration::{Artifacts, DecodeLimits, mpt_root},
};

// Fixed (PBT, MPT) roots for the eight canonical artifact seeds. Fuzzer-controlled
// bytes never supply the trusted commitments used to authenticate a candidate.
const ROOTS: [(B256, B256); 8] = [
    (B256::ZERO, b256!("56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421")),
    (
        b256!("67cab7e6c826271d88c0b6769444722f491a3fa63db64273b26b4ba8fd23fafd"),
        b256!("532dcef15dc70e045307028f8048302d9b14805a47e226945ed6a3f5d4cf181a"),
    ),
    (
        b256!("c09d3c1d0bd86305ed70cc08d37c21c6dc75088dadc8b04f3e0fabf483239213"),
        b256!("72f9a5a3223981247a0cae24827bca7eb0ed54f57da7f55fd6c09cb50de374c1"),
    ),
    (
        b256!("2920a20ea3f67c8612493e3c14b4f84a030e8a8a0d771c86a6a8b59d68ebf187"),
        b256!("e5a93ebbbe2a6163b6ac6dc732ca7f4bb9f60e1ad016d9eee754801772c8dc28"),
    ),
    (
        b256!("276f45e73bb9bbab20e81532ab87be2c6b8d590c14ea4764da7db58027f169b8"),
        b256!("60cbcb665946d4e0a797d0bb07a28dc4e07ec7a1e2daf4478e949d875079b0c1"),
    ),
    (
        b256!("4846c62cfdf5e454dc22a029285b798c49399cf754cf4c505a08d88f7ab1aef8"),
        b256!("76a055163bf3b8464ab9efb00e9ee14e15b487340d377d9c0b81f2640c97c1d8"),
    ),
    (
        b256!("d07840a306deb096525f551d8c92f9596e00434f75be876d8e713145da024634"),
        b256!("5ae3d73e6eb1a70b47735a063f4f0a0464db3728cb207196bf74f8484f63de5d"),
    ),
    (
        b256!("2bffa979d53b80f52d03eef6172390ccf8175caa471c674c90805d0d3a9f1781"),
        b256!("4108a0bfb3803a4104c5ceb43c764d21f5c3526772b46fc6074991c72818a8cf"),
    ),
];

fn check(data: &[u8], expected: Option<bool>) {
    assert!(expected != Some(true) || data.len() >= 5, "truncated canonical seed");
    if data.len() < 5 {
        return;
    }
    let (root, mpt) = ROOTS[usize::from(data[0]) % ROOTS.len()];
    let length = u32::from_le_bytes(data[1..5].try_into().unwrap()) as usize;
    assert!(expected != Some(true) || length <= data.len() - 5, "truncated canonical snapshot");
    if length > data.len() - 5 {
        return;
    }
    let artifacts = Artifacts {
        snapshot: data[5..5 + length].to_vec(),
        preimages: data[5 + length..].to_vec(),
    };
    let limits = DecodeLimits {
        max_accounts: 32,
        max_leaves: 1024,
        max_code_size: 16384,
        max_total_code_size: 65536,
    };
    let restored = artifacts.restore::<Blake3Hasher>(root, limits);
    let verified = artifacts.verify::<Blake3Hasher>(mpt, limits);
    if let Some(expected) = expected {
        assert_eq!(restored.is_ok(), expected, "seed acceptance changed");
    }
    assert_eq!(restored.is_ok(), verified.is_ok(), "PBT and MPT authentication disagree");
    if let Ok(state) = restored {
        assert_eq!(state.root(), root);
        assert_eq!(mpt_root(&state), mpt);
        assert_eq!(
            Artifacts::from_state(&state).unwrap(),
            artifacts,
            "accepted noncanonical artifacts"
        );
        assert_eq!(verified.unwrap().root(), root);
    }
}

fuzz_target!(|data: &[u8]| {
    // Ensure the accepted-state checks remain reachable, even during a run whose
    // supplied corpus contains only malformed inputs.
    static SEEDS: std::sync::Once = std::sync::Once::new();
    SEEDS.call_once(|| {
        let seeds: [&[u8]; 8] = [
            include_bytes!("../seeds/artifact_decode/canonical-0"),
            include_bytes!("../seeds/artifact_decode/canonical-1"),
            include_bytes!("../seeds/artifact_decode/canonical-2"),
            include_bytes!("../seeds/artifact_decode/canonical-3"),
            include_bytes!("../seeds/artifact_decode/canonical-4"),
            include_bytes!("../seeds/artifact_decode/canonical-5"),
            include_bytes!("../seeds/artifact_decode/canonical-6"),
            include_bytes!("../seeds/artifact_decode/canonical-7"),
        ];
        for seed in seeds {
            check(seed, Some(true));
        }
        let rejected: [&[u8]; 12] = [
            include_bytes!("../seeds/artifact_decode/reject-truncated-header"),
            include_bytes!("../seeds/artifact_decode/reject-oversized-snapshot-length"),
            include_bytes!("../seeds/artifact_decode/reject-truncated-snapshot"),
            include_bytes!("../seeds/artifact_decode/reject-truncated-preimages"),
            include_bytes!("../seeds/artifact_decode/reject-snapshot-trailing-byte"),
            include_bytes!("../seeds/artifact_decode/reject-preimages-trailing-byte"),
            include_bytes!("../seeds/artifact_decode/reject-unknown-tag"),
            include_bytes!("../seeds/artifact_decode/reject-wrong-trusted-root"),
            include_bytes!("../seeds/artifact_decode/reject-preimage-mismatch"),
            include_bytes!("../seeds/artifact_decode/reject-snapshot-mismatch"),
            include_bytes!("../seeds/artifact_decode/reject-empty-snapshot"),
            include_bytes!("../seeds/artifact_decode/reject-empty-preimages"),
        ];
        for seed in rejected {
            check(seed, Some(false));
        }
    });
    check(data, None);
});
