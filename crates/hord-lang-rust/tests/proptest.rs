//! Property tests: parse/project losslessness (spec §11.1).

use hord_lang::LangAdapter;
use hord_lang_rust::RustAdapter;
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn any_bytes_are_lossless(bytes in prop::collection::vec(any::<u8>(), 0..192)) {
        let adapter = RustAdapter;
        let tree = adapter.parse(&bytes).expect("parse");
        let projected = adapter.project(&tree);
        prop_assert_eq!(projected.as_slice(), bytes.as_slice());
    }

    #[test]
    fn any_utf8_is_lossless(s in "\\PC{0,96}") {
        let adapter = RustAdapter;
        let bytes = s.as_bytes();
        let tree = adapter.parse(bytes).expect("parse");
        let projected = adapter.project(&tree);
        prop_assert_eq!(projected.as_slice(), bytes);
    }
}
