//! Property tests: parse/project losslessness (spec §11.1).

use hord_lang::LangAdapter;
use hord_lang_toml::TomlAdapter;
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn parse_project_round_trip(bytes in prop::collection::vec(any::<u8>(), 0..192)) {
        let adapter = TomlAdapter;
        let tree = adapter.parse(&bytes).expect("tree-sitter always yields a tree");
        let projected = adapter.project(&tree);
        prop_assert_eq!(projected.as_slice(), bytes.as_slice());
    }

    #[test]
    fn utf8_toml_round_trip(s in "\\PC{0,96}") {
        let bytes = s.as_bytes();
        let adapter = TomlAdapter;
        let tree = adapter.parse(bytes).expect("parse");
        let projected = adapter.project(&tree);
        prop_assert_eq!(projected.as_slice(), bytes);
    }
}
