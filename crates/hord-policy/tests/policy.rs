//! Spec §7.2: the example policy, parsed and enforced rule by rule.

use std::collections::BTreeSet;

use hord_core::{EvidenceResult, NodeId, RepoPath};
use hord_policy::{
    ActorClass, CompiledPolicy, Decision, EvidenceFact, EvidenceState, Facts, Location,
    TouchedDefinition, Trigger, Violation, ViolationSource, parse,
};

/// The policy exactly as spec §7.2 shows it.
const SPEC_POLICY: &str = r#"[land]
require = ["check", "test:selected", "lint"]   # evidence kinds that must be Pass
strict_reads = false                            # write-read conflicts (§6.3)
max_write_set = 200                             # definitions; larger changes need review
max_replay_attempts = 2

[[rule]]
name = "unsafe requires human"
when = { touches_kind = "unsafe_block" }
require = ["review:human"]

[[rule]]
name = "public API"
when = { touches_visibility = "pub", paths = ["crates/hord-core/**"] }
require = ["review:human", "test:full"]

[[rule]]
name = "agent-authored large changes"
when = { actor = "agent", write_set_gt = 50 }
require = ["review:agent-reviewer", "bench:no-regression"]
"#;

fn policy() -> CompiledPolicy {
    parse(SPEC_POLICY).expect("spec policy parses")
}

fn path(s: &str) -> RepoPath {
    s.parse().expect("parse path literal")
}

fn node(n: u128) -> NodeId {
    NodeId::from_u128(n)
}

fn def(n: u128, file: &str, kinds: &[&str], visibility: Option<&str>) -> TouchedDefinition {
    TouchedDefinition {
        node: node(n),
        path: path(file),
        kinds: kinds
            .iter()
            .map(|k| (*k).to_owned())
            .collect::<BTreeSet<_>>(),
        visibility: visibility.map(str::to_owned),
    }
}

fn pass(tag: &str) -> EvidenceFact {
    EvidenceFact {
        tag: tag.parse().expect("parse evidence tag literal"),
        result: EvidenceResult::Pass,
    }
}

/// A human change to one private function with the `[land]` evidence.
fn baseline(actor: ActorClass) -> Facts {
    let mut facts = Facts::new(actor);
    facts.write_set_len = 1;
    facts.definitions = vec![def(1, "src/lib.rs", &["function_item"], None)];
    facts.paths = vec![path("src/lib.rs")];
    facts.evidence = vec![pass("check"), pass("test:selected"), pass("lint")];
    facts
}

fn reasons(decision: Decision) -> Vec<Violation> {
    match decision {
        Decision::Allow => Vec::new(),
        Decision::Deny { reasons } => reasons,
    }
}

fn unmet(decision: Decision) -> Vec<(Option<String>, String)> {
    reasons(decision)
        .into_iter()
        .map(|v| (v.rule, v.requirement.to_string()))
        .collect()
}

fn rule(name: &str, requirement: &str) -> (Option<String>, String) {
    (Some(name.to_owned()), requirement.to_owned())
}

#[test]
fn parses_the_spec_example() {
    let policy = policy();
    let land = policy.land();
    assert_eq!(land.require, ["check", "test:selected", "lint"]);
    assert!(!land.strict_reads);
    assert_eq!(land.max_write_set, Some(200));
    assert_eq!(land.max_replay_attempts, 2);
    let rules = &policy.policy().rules;
    assert_eq!(rules.len(), 3);
    assert_eq!(rules[0].name, "unsafe requires human");
    assert_eq!(rules[0].when.touches_kind.as_deref(), Some("unsafe_block"));
    assert_eq!(rules[1].when.touches_visibility.as_deref(), Some("pub"));
    assert_eq!(
        rules[1].when.paths.as_deref(),
        Some(&["crates/hord-core/**".to_owned()][..])
    );
    assert_eq!(rules[1].require, ["review:human", "test:full"]);
    assert_eq!(rules[2].when.actor.as_deref(), Some("agent"));
    assert_eq!(rules[2].when.write_set_gt, Some(50));
}

#[test]
fn object_round_trip_compiles_the_same() -> Result<(), Box<dyn std::error::Error>> {
    let policy = policy();
    let again = CompiledPolicy::from_object(policy.policy().clone())?;
    assert_eq!(again.policy(), policy.policy());
    let facts = baseline(ActorClass::Agent);
    assert_eq!(again.evaluate(&facts), policy.evaluate(&facts));
    Ok(())
}

#[test]
fn land_requirements_allow_when_all_pass() {
    assert_eq!(
        policy().evaluate(&baseline(ActorClass::Human)),
        Decision::Allow
    );
}

#[test]
fn land_requirements_deny_missing_failed_and_skipped() -> Result<(), Box<dyn std::error::Error>> {
    let mut facts = baseline(ActorClass::Human);
    facts.evidence = vec![
        EvidenceFact {
            tag: "test:selected".parse()?,
            result: EvidenceResult::Fail {
                summary: "1 failed".into(),
            },
        },
        EvidenceFact {
            tag: "lint".parse()?,
            result: EvidenceResult::Skipped {
                reason: "no clippy".into(),
            },
        },
    ];
    let reasons = reasons(policy().evaluate(&facts));
    let got: Vec<_> = reasons
        .iter()
        .map(|v| (v.source, v.requirement.to_string(), v.evidence))
        .collect();
    assert_eq!(
        got,
        [
            (ViolationSource::Land, "check".into(), EvidenceState::Absent),
            (
                ViolationSource::Land,
                "test:selected".into(),
                EvidenceState::Failed
            ),
            (ViolationSource::Land, "lint".into(), EvidenceState::Skipped),
        ]
    );
    assert!(
        reasons
            .iter()
            .all(|v| v.rule.is_none() && v.triggers.is_empty())
    );
    Ok(())
}

#[test]
fn a_pass_beside_a_failure_meets_the_requirement() -> Result<(), Box<dyn std::error::Error>> {
    let mut facts = baseline(ActorClass::Human);
    facts.evidence.push(EvidenceFact {
        tag: "check".parse()?,
        result: EvidenceResult::Fail {
            summary: "earlier run".into(),
        },
    });
    assert_eq!(policy().evaluate(&facts), Decision::Allow);
    Ok(())
}

#[test]
fn test_full_meets_test_selected_but_not_the_reverse() {
    // ADR 0026: a full run is a superset of a selected one.
    let mut facts = baseline(ActorClass::Human);
    facts.evidence = vec![pass("check"), pass("test:full"), pass("lint")];
    assert_eq!(policy().evaluate(&facts), Decision::Allow);

    facts.definitions = vec![def(
        3,
        "crates/hord-core/src/id.rs",
        &["function_item"],
        Some("pub"),
    )];
    facts.evidence = vec![
        pass("check"),
        pass("test:selected"),
        pass("lint"),
        pass("review:human"),
    ];
    assert_eq!(
        unmet(policy().evaluate(&facts)),
        [rule("public API", "test:full")]
    );
}

#[test]
fn max_write_set_needs_review() {
    let mut facts = baseline(ActorClass::Human);
    facts.write_set_len = 200;
    assert_eq!(policy().evaluate(&facts), Decision::Allow);

    facts.write_set_len = 201;
    let reasons = reasons(policy().evaluate(&facts));
    assert_eq!(reasons.len(), 1);
    assert_eq!(reasons[0].source, ViolationSource::MaxWriteSet);
    assert_eq!(reasons[0].requirement.to_string(), "review");
    assert_eq!(
        reasons[0].triggers,
        [Trigger::WriteSet {
            size: 201,
            limit: 200
        }]
    );

    facts.evidence.push(pass("review:human"));
    assert_eq!(policy().evaluate(&facts), Decision::Allow);
}

#[test]
fn unsafe_requires_human() {
    let mut facts = baseline(ActorClass::Human);
    facts.definitions.push(def(
        2,
        "src/raw.rs",
        &["function_item", "unsafe_block"],
        None,
    ));
    let reasons = reasons(policy().evaluate(&facts));
    assert_eq!(reasons.len(), 1);
    assert_eq!(reasons[0].source, ViolationSource::Rule);
    assert_eq!(reasons[0].rule.as_deref(), Some("unsafe requires human"));
    assert_eq!(reasons[0].requirement.to_string(), "review:human");
    assert_eq!(reasons[0].evidence, EvidenceState::Absent);
    assert_eq!(
        reasons[0].triggers,
        [Trigger::Definition {
            node: node(2),
            path: "src/raw.rs".into()
        }]
    );

    // An agent review is not a human review.
    facts.evidence.push(pass("review:agent-reviewer"));
    assert_eq!(
        unmet(policy().evaluate(&facts)),
        [rule("unsafe requires human", "review:human")]
    );

    facts.evidence.push(pass("review:human"));
    assert_eq!(policy().evaluate(&facts), Decision::Allow);
}

#[test]
fn public_api_needs_pub_inside_hord_core() {
    let policy = policy();
    let with = |d: TouchedDefinition| {
        let mut facts = baseline(ActorClass::Human);
        facts.paths.push(d.path.clone());
        facts.definitions.push(d);
        facts
    };

    // pub, in hord-core: both requirements, triggered by that definition.
    let facts = with(def(
        3,
        "crates/hord-core/src/id.rs",
        &["function_item"],
        Some("pub"),
    ));
    let reasons = reasons(policy.evaluate(&facts));
    assert_eq!(
        reasons
            .iter()
            .map(|v| (v.rule.clone(), v.requirement.to_string()))
            .collect::<Vec<_>>(),
        [
            rule("public API", "review:human"),
            rule("public API", "test:full")
        ]
    );
    assert_eq!(
        reasons[0].triggers,
        [Trigger::Definition {
            node: node(3),
            path: "crates/hord-core/src/id.rs".into()
        }]
    );

    // pub elsewhere, private in hord-core, pub(crate) in hord-core: no rule.
    for d in [
        def(
            4,
            "crates/hord-txn/src/lib.rs",
            &["function_item"],
            Some("pub"),
        ),
        def(5, "crates/hord-core/src/id.rs", &["function_item"], None),
        def(
            6,
            "crates/hord-core/src/id.rs",
            &["function_item"],
            Some("pub(crate)"),
        ),
        def(7, "crates/hord-core.rs", &["function_item"], Some("pub")),
    ] {
        assert_eq!(policy.evaluate(&with(d.clone())), Decision::Allow, "{d:?}");
    }

    // A pub definition elsewhere and a private one in hord-core do not
    // combine into a match: the predicates hold on one definition.
    let mut facts = with(def(
        4,
        "crates/hord-txn/src/lib.rs",
        &["function_item"],
        Some("pub"),
    ));
    facts.definitions.push(def(
        5,
        "crates/hord-core/src/id.rs",
        &["function_item"],
        None,
    ));
    assert_eq!(policy.evaluate(&facts), Decision::Allow);

    let mut facts = with(def(
        3,
        "crates/hord-core/src/id.rs",
        &["function_item"],
        Some("pub"),
    ));
    facts
        .evidence
        .extend([pass("review:human"), pass("test:full")]);
    assert_eq!(policy.evaluate(&facts), Decision::Allow);
}

#[test]
fn agent_authored_large_changes() {
    let policy = policy();
    let mut facts = baseline(ActorClass::Agent);
    facts.write_set_len = 50;
    assert_eq!(policy.evaluate(&facts), Decision::Allow);

    facts.write_set_len = 51;
    let reasons = reasons(policy.evaluate(&facts));
    assert_eq!(
        reasons
            .iter()
            .map(|v| (v.rule.clone(), v.requirement.to_string()))
            .collect::<Vec<_>>(),
        [
            rule("agent-authored large changes", "review:agent-reviewer"),
            rule("agent-authored large changes", "bench:no-regression"),
        ]
    );
    assert_eq!(
        reasons[0].triggers,
        [
            Trigger::Actor {
                actor: ActorClass::Agent
            },
            Trigger::WriteSet {
                size: 51,
                limit: 50
            },
        ]
    );

    // The same change by a human is not an agent-authored change.
    let mut human = facts.clone();
    human.actor = ActorClass::Human;
    assert_eq!(policy.evaluate(&human), Decision::Allow);

    facts
        .evidence
        .extend([pass("review:agent-reviewer"), pass("bench:no-regression")]);
    assert_eq!(policy.evaluate(&facts), Decision::Allow);
}

#[test]
fn rules_combine_in_policy_order() {
    let mut facts = baseline(ActorClass::Agent);
    facts.write_set_len = 300;
    facts.evidence.clear();
    facts.definitions.push(def(
        9,
        "crates/hord-core/src/unsafe_bits.rs",
        &["impl_item", "unsafe_block"],
        Some("pub"),
    ));
    let got: Vec<_> = reasons(policy().evaluate(&facts))
        .into_iter()
        .map(|v| (v.source, v.rule, v.requirement.to_string()))
        .collect();
    let land = |r: &str| (ViolationSource::Land, None, r.to_owned());
    let rule = |n: &str, r: &str| (ViolationSource::Rule, Some(n.to_owned()), r.to_owned());
    assert_eq!(
        got,
        [
            land("check"),
            land("test:selected"),
            land("lint"),
            (ViolationSource::MaxWriteSet, None, "review".to_owned()),
            rule("unsafe requires human", "review:human"),
            rule("public API", "review:human"),
            rule("public API", "test:full"),
            rule("agent-authored large changes", "review:agent-reviewer"),
            rule("agent-authored large changes", "bench:no-regression"),
        ]
    );
}

#[test]
fn paths_alone_match_touched_files() -> Result<(), Box<dyn std::error::Error>> {
    let policy = parse(
        r#"[land]
require = []
strict_reads = true
max_write_set = 10
max_replay_attempts = 0

[[rule]]
name = "manifests"
when = { paths = ["**/Cargo.toml", "Cargo.lock"] }
require = ["review"]

[[rule]]
name = "always"
when = {}
require = ["check"]
"#,
    )?;
    assert!(policy.land().strict_reads);
    let mut facts = Facts::new(ActorClass::Human);
    facts.evidence.push(pass("check"));
    assert_eq!(policy.evaluate(&facts), Decision::Allow);

    facts.paths = vec![path("crates/a/Cargo.toml"), path("crates/a/src/lib.rs")];
    let reasons = reasons(policy.evaluate(&facts));
    assert_eq!(reasons.len(), 1);
    assert_eq!(
        reasons[0].triggers,
        [Trigger::Path {
            path: "crates/a/Cargo.toml".into()
        }]
    );

    // `*` does not cross `/`; `**` does.
    facts.paths = vec![path("sub/Cargo.lock")];
    assert_eq!(policy.evaluate(&facts), Decision::Allow);

    facts.paths = vec![path("Cargo.toml")];
    facts.evidence.push(pass("review:human"));
    assert_eq!(policy.evaluate(&facts), Decision::Allow);
    Ok(())
}

#[test]
fn deny_serializes_machine_readably() -> Result<(), Box<dyn std::error::Error>> {
    let mut facts = baseline(ActorClass::Human);
    facts.definitions = vec![def(
        2,
        "src/raw.rs",
        &["function_item", "unsafe_block"],
        None,
    )];
    let json = serde_json::to_value(policy().evaluate(&facts))?;
    assert_eq!(
        json,
        serde_json::json!({
            "decision": "deny",
            "reasons": [{
                "source": "rule",
                "rule": "unsafe requires human",
                "requirement": "review:human",
                "evidence": "absent",
                "triggers": [{
                    "on": "definition",
                    "node": node(2).to_string(),
                    "path": "src/raw.rs",
                }],
            }],
        })
    );
    let back: Decision = serde_json::from_value(json)?;
    assert_eq!(back, policy().evaluate(&facts));
    assert_eq!(
        serde_json::to_value(Decision::Allow)?,
        serde_json::json!({ "decision": "allow" })
    );
    Ok(())
}

/// The error for `source`, which must fail to parse.
fn error(source: &str) -> (Option<Location>, String) {
    let err = parse(source).expect_err("should not parse");
    (err.location, err.message)
}

fn at(line: usize, column: usize) -> Option<Location> {
    Some(Location { line, column })
}

const LAND: &str =
    "[land]\nrequire = []\nstrict_reads = false\nmax_write_set = 1\nmax_replay_attempts = 1\n";

#[test]
fn syntax_error_has_line_and_column() {
    let (location, _) = error("[land]\nrequire = [\"check\"\n");
    assert_eq!(location.map(|l| l.line), Some(2));
    let err =
        parse("[land]\nrequire = [\"check\"\n").expect_err("an unclosed array does not parse");
    assert!(err.to_string().starts_with("2:"), "{err}");
}

#[test]
fn unknown_key_is_located() {
    let src = format!(
        "{LAND}\n[[rule]]\nname = \"x\"\nwhen = {{ touches_kinds = \"a\" }}\nrequire = []\n"
    );
    let (location, message) = error(&src);
    assert_eq!(location.map(|l| l.line), Some(9));
    assert!(message.contains("touches_kinds"), "{message}");
}

#[test]
fn wrong_type_is_located() {
    let (location, message) = error(
        "[land]\nrequire = []\nstrict_reads = \"no\"\nmax_write_set = 1\nmax_replay_attempts = 1\n",
    );
    assert_eq!(location, at(3, 16));
    assert!(message.contains("bool"), "{message}");

    let (location, _) = error(
        "[land]\nrequire = []\nstrict_reads = false\nmax_write_set = -1\nmax_replay_attempts = 1\n",
    );
    assert_eq!(location, at(4, 17));
}

#[test]
fn land_keys_default() -> Result<(), Box<dyn std::error::Error>> {
    // ADR 0026: every `[land]` key is optional.
    for source in [
        "",
        "[land]\n",
        "[[rule]]\nname = \"x\"\nwhen = {}\nrequire = []\n",
    ] {
        let policy = parse(source)?;
        let land = policy.land();
        assert!(land.require.is_empty(), "{source:?}");
        assert!(!land.strict_reads);
        assert_eq!(land.max_write_set, None);
        assert_eq!(land.max_replay_attempts, 2);
    }
    assert_eq!(parse("")?.land().max_impact, None);
    let policy = parse("[land]\nmax_impact = 40\n")?;
    assert_eq!(policy.land().max_impact, Some(40));
    assert_eq!(policy.land().max_write_set, None);
    let (location, _) = error("[land]\nmax_impact = -1\n");
    assert_eq!(location, at(2, 14));
    let policy = parse("[land]\nstrict_reads = true\n")?;
    assert!(policy.land().strict_reads);
    assert_eq!(policy.land().max_replay_attempts, 2);
    assert_eq!(parse("")?.policy(), CompiledPolicy::default().policy());
    Ok(())
}

#[test]
fn no_max_write_set_is_no_limit() -> Result<(), Box<dyn std::error::Error>> {
    let policy = parse("[land]\nrequire = [\"check\"]\n")?;
    let mut facts = Facts::new(ActorClass::Agent);
    facts.write_set_len = 1_000_000;
    facts.evidence.push(pass("check"));
    assert_eq!(policy.evaluate(&facts), Decision::Allow);
    assert_eq!(CompiledPolicy::default().evaluate(&facts), Decision::Allow);
    Ok(())
}

#[test]
fn unknown_land_key_is_located() {
    let (location, message) = error("[land]\nrequire = []\nmax_writeset = 3\n");
    assert_eq!(location.map(|l| l.line), Some(3));
    assert!(message.contains("max_writeset"), "{message}");
}

#[test]
fn unknown_actor_is_located() {
    let src = format!(
        "{LAND}\n[[rule]]\nname = \"x\"\nwhen = {{ actor = \"robot\" }}\nrequire = [\"review\"]\n"
    );
    let (location, message) = error(&src);
    assert_eq!(location, at(9, 18));
    assert!(message.contains("robot"), "{message}");
}

#[test]
fn bad_requirement_is_located() {
    let (location, message) = error(
        "[land]\nrequire = [\"check\", \"test::full\"]\nstrict_reads = false\nmax_write_set = 1\nmax_replay_attempts = 1\n",
    );
    assert_eq!(location, at(2, 21));
    assert!(message.contains("test::full"), "{message}");

    let src = format!("{LAND}\n[[rule]]\nname = \"x\"\nwhen = {{}}\nrequire = [\"\"]\n");
    assert_eq!(error(&src).0, at(10, 12));
}

#[test]
fn bad_glob_is_located() {
    let src = format!(
        "{LAND}\n[[rule]]\nname = \"x\"\nwhen = {{ paths = [\"ok/**\", \"a[\"] }}\nrequire = []\n"
    );
    let (location, message) = error(&src);
    assert_eq!(location, at(9, 28));
    assert!(message.contains("a["), "{message}");

    let src = format!("{LAND}\n[[rule]]\nname = \"x\"\nwhen = {{ paths = [] }}\nrequire = []\n");
    let (location, message) = error(&src);
    assert_eq!(location, at(9, 18));
    assert!(message.contains("empty"), "{message}");
}

#[test]
fn duplicate_rule_name_is_located() {
    let src = format!(
        "{LAND}\n[[rule]]\nname = \"x\"\nwhen = {{}}\nrequire = []\n\n[[rule]]\nname = \"x\"\nwhen = {{}}\nrequire = []\n"
    );
    let (location, message) = error(&src);
    assert_eq!(location, at(13, 8));
    assert!(message.contains("duplicate"), "{message}");
}

#[test]
fn object_errors_have_no_location() {
    let mut object = policy().policy().clone();
    object.rules[0].when.actor = Some("robot".into());
    let err =
        CompiledPolicy::from_object(object).expect_err("an invalid policy object does not compile");
    assert_eq!(err.location, None);
    assert!(err.message.contains("robot"));
}

#[test]
fn location_counts_characters() {
    assert_eq!(
        Location::of("ab\ncé\nx", 0),
        Location { line: 1, column: 1 }
    );
    assert_eq!(
        Location::of("ab\ncé\nx", 3),
        Location { line: 2, column: 1 }
    );
    // `é` is two bytes and one column.
    assert_eq!(
        Location::of("ab\ncé\nx", 6),
        Location { line: 2, column: 3 }
    );
    assert_eq!(
        Location::of("ab\ncé\nx", 7),
        Location { line: 3, column: 1 }
    );
    assert_eq!(Location::of("ab", 99), Location { line: 1, column: 3 });
}
