//! The generator of the M5 conflict corpus: eight templates, each filled in
//! with its own names and values per variant, 100 cases in all.
//! Deterministic: the same generator version writes the same files, and a
//! test checks that the checked-in corpus is what it writes
//! (`hord-eval-m5 generate --out corpora/m5/cases` regenerates it).
//!
//! | template | conflict | resolvable |
//! |---|---|---|
//! | `same-tokens`: both rewrite one expression | semantic or hard | yes |
//! | `signature-vs-caller`: a new parameter against a new caller | semantic | yes |
//! | `rename-vs-caller`: a rename against a new caller of the old name | semantic | yes |
//! | `move-vs-edit`: a definition moved to a module against an edit of it | hard | yes |
//! | `field-vs-literal`: a new struct field against a new struct literal | semantic | yes |
//! | `variant-vs-match`: a new enum variant against a new exhaustive match | semantic | yes |
//! | `return-vs-caller`: `Option` to `Result` against a new caller | semantic | yes |
//! | `contradiction`: both set one constant, or one changes behavior another pins | hard or semantic | no |

use std::collections::BTreeMap;

use crate::corpus::{Case, Step, Task};

/// Written into every case's `made_by`; bump it when a template changes.
pub const VERSION: &str = "hord-eval-m5 generate v1";

/// Its own `[workspace]`, so a case built inside another workspace (such
/// as hord's `target/`) is not taken for a member of it.
const CARGO_TOML: &str =
    "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n";
const GITIGNORE: &str = "target/\n";
const LIB: &str = "src/lib.rs";
const TEST_A: &str = "tests/m5_a.rs";
const TEST_B: &str = "tests/m5_b.rs";

/// `template` with each `@KEY@` replaced by its value.
fn fill(template: &str, vars: &[(&str, &str)]) -> String {
    vars.iter().fold(template.to_owned(), |text, (key, value)| {
        text.replace(&format!("@{key}@"), value)
    })
}

fn test_file(name: &str, body: &str) -> String {
    format!("#[test]\nfn {name}() {{\n    {body}\n}}\n")
}

fn files(pairs: &[(&str, String)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

/// The pieces of one case before numbering.
struct Draft {
    kind: &'static str,
    conflict: &'static str,
    variant: String,
    lib: String,
    a: TaskDraft,
    b: TaskDraft,
    /// Files on top of the first task's result that meet both intents.
    resolution: Option<Vec<(String, String)>>,
}

struct TaskDraft {
    summary: String,
    body: String,
    test: String,
    assertion: String,
    files: Vec<(String, String)>,
}

impl TaskDraft {
    fn build(self, name: &str, test_path: &str) -> Task {
        Task {
            name: name.into(),
            summary: self.summary,
            body: self.body,
            test_source: test_file(&self.test, &self.assertion),
            test: self.test,
            test_path: test_path.into(),
            files: self.files.into_iter().collect(),
        }
    }
}

fn same_tokens(i: usize) -> Draft {
    const NAMES: [&str; 13] = [
        "widen", "boost", "stretch", "amplify", "inflate", "enlarge", "magnify", "raise", "expand",
        "extend", "grow", "swell", "pump",
    ];
    let f = NAMES[i];
    let k = i + 2;
    let ks = k.to_string();
    let vars = [("F", f), ("K", ks.as_str())];
    let base = fill("pub fn @F@(x: u32) -> u32 {\n    x * @K@\n}\n", &vars);
    let with = |expr: &str| base.replace(&format!("x * {k}"), expr);
    Draft {
        kind: "same-tokens",
        conflict: "semantic",
        variant: format!("{f} x{k}"),
        a: TaskDraft {
            summary: format!("{f} adds one after scaling"),
            body: format!(
                "Callers need {f}(x) to be x times {k} plus one, so that zero never maps to zero."
            ),
            test: format!("{f}_adds_one"),
            assertion: format!("assert_eq!(fixture::{f}(3), {});", 3 * k + 1),
            files: vec![(LIB.into(), with(&format!("x * {k} + 1")))],
        },
        b: TaskDraft {
            summary: format!("{f} saturates instead of overflowing"),
            body: format!(
                "{f} panics in debug builds on large inputs. Make it saturate at u32::MAX."
            ),
            test: format!("{f}_saturates"),
            assertion: format!("assert_eq!(fixture::{f}(u32::MAX), u32::MAX);"),
            files: vec![(LIB.into(), with(&format!("x.saturating_mul({k})")))],
        },
        resolution: Some(vec![(
            LIB.into(),
            with(&format!("x.saturating_mul({k}).saturating_add(1)")),
        )]),
        lib: base,
    }
}

fn signature_vs_caller(i: usize) -> Draft {
    const NAMES: [(&str, &str); 13] = [
        ("scale", "scale_plus_one"),
        ("weigh", "weigh_padded"),
        ("stretch", "stretch_once_more"),
        ("mult", "mult_and_bump"),
        ("resize", "resize_margin"),
        ("price", "price_with_fee"),
        ("volume", "volume_plus"),
        ("cost", "cost_with_tax"),
        ("area", "area_with_border"),
        ("speed", "speed_boosted"),
        ("power", "power_offset"),
        ("gain", "gain_biased"),
        ("dose", "dose_extra"),
    ];
    let (f, g) = NAMES[i];
    let p = i + 3;
    let vars = [("F", f), ("G", g)];
    let base = fill(
        "pub fn @F@(x: u32) -> u32 {\n    x * 2\n}\n\npub fn @F@_twice(x: u32) -> u32 {\n    @F@(@F@(x))\n}\n",
        &vars,
    );
    let changed = fill(
        "pub fn @F@(x: u32, factor: u32) -> u32 {\n    x * factor\n}\n\npub fn @F@_twice(x: u32) -> u32 {\n    @F@(@F@(x, 2), 2)\n}\n",
        &vars,
    );
    let caller = |call: &str| {
        fill(
            &format!("\npub fn @G@(x: u32) -> u32 {{\n    {call} + 1\n}}\n"),
            &vars,
        )
    };
    Draft {
        kind: "signature-vs-caller",
        conflict: "semantic",
        variant: format!("{f}/{g}"),
        a: TaskDraft {
            summary: format!("{f} takes its factor as a parameter"),
            body: format!(
                "{f} always doubles. Callers need other factors: add a `factor` parameter and keep {f}_twice doubling twice."
            ),
            test: format!("{f}_takes_a_factor"),
            assertion: format!("assert_eq!(fixture::{f}(3, {p}), {});", 3 * p),
            files: vec![(LIB.into(), changed.clone())],
        },
        b: TaskDraft {
            summary: format!("add {g}: {f} plus one"),
            body: format!("Add {g}(x), which is {f}(x) plus one."),
            test: format!("{g}_is_one_more"),
            assertion: format!("assert_eq!(fixture::{g}(4), 9);"),
            files: vec![(LIB.into(), format!("{base}{}", caller(&format!("{f}(x)"))))],
        },
        resolution: Some(vec![(
            LIB.into(),
            format!("{changed}{}", caller(&format!("{f}(x, 2)"))),
        )]),
        lib: base,
    }
}

fn rename_vs_caller(i: usize) -> Draft {
    const NAMES: [(&str, &str, &str); 12] = [
        ("total", "sum_all", "double_total"),
        ("count", "count_items", "count_twice"),
        ("tally", "tally_votes", "tally_doubled"),
        ("aggregate", "sum_values", "aggregate_twice"),
        ("accumulate", "sum_up", "accumulate_twice"),
        ("combine", "sum_parts", "combine_twice"),
        ("summarize", "sum_entries", "summarize_twice"),
        ("collect_sum", "sum_collected", "collect_twice"),
        ("add_all", "sum_every", "add_all_twice"),
        ("fold_sum", "sum_folded", "fold_twice"),
        ("measure", "sum_measure", "measure_twice"),
        ("gather", "sum_gathered", "gather_twice"),
    ];
    let (old, new, g) = NAMES[i];
    let body_fn = "(xs: &[i32]) -> i32 {\n    xs.iter().sum()\n}\n";
    let base = format!("pub fn {old}{body_fn}");
    let renamed = format!("pub fn {new}{body_fn}");
    let caller =
        |name: &str| format!("\npub fn {g}(xs: &[i32]) -> i32 {{\n    {name}(xs) * 2\n}}\n");
    Draft {
        kind: "rename-vs-caller",
        conflict: "semantic",
        variant: format!("{old}->{new}/{g}"),
        a: TaskDraft {
            summary: format!("rename {old} to {new}"),
            body: format!(
                "{old} is a vague name. Rename it to {new}; its behavior stays the same."
            ),
            test: format!("{new}_sums"),
            assertion: format!("assert_eq!(fixture::{new}(&[1, 2, 3]), 6);"),
            files: vec![(LIB.into(), renamed.clone())],
        },
        b: TaskDraft {
            summary: format!("add {g}: twice the {old}"),
            body: format!("Add {g}(xs), twice the sum that {old} computes."),
            test: format!("{g}_doubles"),
            assertion: format!("assert_eq!(fixture::{g}(&[1, 2]), 6);"),
            files: vec![(LIB.into(), format!("{base}{}", caller(old)))],
        },
        resolution: Some(vec![(LIB.into(), format!("{renamed}{}", caller(new)))]),
        lib: base,
    }
}

fn move_vs_edit(i: usize) -> Draft {
    const NAMES: [(&str, &str); 12] = [
        ("parse_id", "ids"),
        ("parse_port", "net"),
        ("parse_count", "counts"),
        ("parse_age", "people"),
        ("parse_level", "levels"),
        ("parse_score", "scores"),
        ("parse_size", "sizes"),
        ("parse_rank", "ranks"),
        ("parse_year", "dates"),
        ("parse_code", "codes"),
        ("parse_limit", "limits"),
        ("parse_seed", "seeds"),
    ];
    let (f, m) = NAMES[i];
    let func = |expr: &str| format!("pub fn {f}(s: &str) -> Option<u32> {{\n    {expr}\n}}\n");
    let base = func("s.parse().ok()");
    let module_lib = format!("pub mod {m};\n\npub use {m}::{f};\n");
    let module_path = format!("src/{m}.rs");
    Draft {
        kind: "move-vs-edit",
        conflict: "hard",
        variant: format!("{f} into {m}"),
        a: TaskDraft {
            summary: format!("move {f} into module {m}"),
            body: format!(
                "Move {f} into a new public module `{m}`, re-exported at the crate root."
            ),
            test: format!("{f}_lives_in_{m}"),
            assertion: format!("assert_eq!(fixture::{m}::{f}(\"7\"), Some(7));"),
            files: vec![
                (LIB.into(), module_lib.clone()),
                (module_path.clone(), base.clone()),
            ],
        },
        b: TaskDraft {
            summary: format!("{f} ignores surrounding whitespace"),
            body: format!("{f} rejects input with spaces around the number. Trim it first."),
            test: format!("{f}_trims"),
            assertion: format!("assert_eq!(fixture::{f}(\" 7 \"), Some(7));"),
            files: vec![(LIB.into(), func("s.trim().parse().ok()"))],
        },
        resolution: Some(vec![
            (LIB.into(), module_lib),
            (module_path, func("s.trim().parse().ok()")),
        ]),
        lib: base,
    }
}

fn field_vs_literal(i: usize) -> Draft {
    const NAMES: [(&str, &str, &str); 13] = [
        ("User", "age", "guest"),
        ("Account", "level", "admin_account"),
        ("Order", "priority", "rush_order"),
        ("Item", "weight", "sample_item"),
        ("Ticket", "severity", "blank_ticket"),
        ("Invoice", "discount", "draft_invoice"),
        ("Task", "retries", "chore"),
        ("Device", "port", "loopback_device"),
        ("Sensor", "rate", "test_sensor"),
        ("Report", "version", "empty_report"),
        ("Session", "timeout", "anonymous_session"),
        ("Profile", "score", "default_profile"),
        ("Badge", "tier", "starter_badge"),
    ];
    let (s, field, g) = NAMES[i];
    let d = (i + 1) * 5;
    let ds = d.to_string();
    let vars = [("S", s), ("FIELD", field), ("G", g), ("D", ds.as_str())];
    let base = fill(
        "pub struct @S@ {\n    pub name: String,\n}\n\nimpl @S@ {\n    pub fn new(name: &str) -> Self {\n        @S@ { name: name.into() }\n    }\n}\n",
        &vars,
    );
    let with_field = fill(
        "pub struct @S@ {\n    pub name: String,\n    pub @FIELD@: u32,\n}\n\nimpl @S@ {\n    pub fn new(name: &str) -> Self {\n        @S@ { name: name.into(), @FIELD@: @D@ }\n    }\n}\n",
        &vars,
    );
    let maker = |extra: &str| {
        fill(
            &format!("\npub fn @G@() -> @S@ {{\n    @S@ {{ name: \"@G@\".into(){extra} }}\n}}\n"),
            &vars,
        )
    };
    Draft {
        kind: "field-vs-literal",
        conflict: "semantic",
        variant: format!("{s}.{field}/{g}"),
        a: TaskDraft {
            summary: format!("{s} gets a {field} field, {d} by default"),
            body: format!("Add `{field}: u32` to {s}. {s}::new sets it to {d}."),
            test: format!("{}_{field}_defaults", s.to_lowercase()),
            assertion: format!("assert_eq!(fixture::{s}::new(\"x\").{field}, {d});"),
            files: vec![(LIB.into(), with_field.clone())],
        },
        b: TaskDraft {
            summary: format!("add {g}, a {s} named {g}"),
            body: format!("Add {g}(), which builds a {s} named \"{g}\"."),
            test: format!("{g}_is_named"),
            assertion: format!("assert_eq!(fixture::{g}().name, \"{g}\");"),
            files: vec![(LIB.into(), format!("{base}{}", maker("")))],
        },
        resolution: Some(vec![(
            LIB.into(),
            format!("{with_field}{}", maker(&format!(", {field}: {d}"))),
        )]),
        lib: base,
    }
}

fn variant_vs_match(i: usize) -> Draft {
    const NAMES: [(&str, &str, &str, &str); 12] = [
        ("Shape", "Circle", "Square", "sides"),
        ("Color", "Green", "Red", "warmth"),
        ("Status", "Open", "Closed", "weight"),
        ("Mode", "Read", "Write", "cost"),
        ("Level", "Low", "High", "rank"),
        ("Kind", "Plain", "Fancy", "extra"),
        ("Phase", "Start", "Finish", "order"),
        ("State", "Idle", "Busy", "load"),
        ("Tone", "Soft", "Loud", "volume"),
        ("Flavor", "Sweet", "Sour", "strength"),
        ("Tier", "Free", "Paid", "quota"),
        ("Suit", "Hearts", "Spades", "value"),
    ];
    let (e, v1, v2, g) = NAMES[i];
    let (l1, l2) = (v1.to_lowercase(), v2.to_lowercase());
    let vars = [
        ("E", e),
        ("V1", v1),
        ("V2", v2),
        ("L1", l1.as_str()),
        ("L2", l2.as_str()),
        ("G", g),
    ];
    let base = fill(
        "pub enum @E@ {\n    @V1@,\n}\n\npub fn label(e: &@E@) -> &'static str {\n    match e {\n        @E@::@V1@ => \"@L1@\",\n    }\n}\n",
        &vars,
    );
    let with_variant = fill(
        "pub enum @E@ {\n    @V1@,\n    @V2@,\n}\n\npub fn label(e: &@E@) -> &'static str {\n    match e {\n        @E@::@V1@ => \"@L1@\",\n        @E@::@V2@ => \"@L2@\",\n    }\n}\n",
        &vars,
    );
    let scorer = |arms: &str| {
        fill(
            &format!("\npub fn @G@(e: &@E@) -> u32 {{\n    match e {{\n{arms}    }}\n}}\n"),
            &vars,
        )
    };
    let one = fill("        @E@::@V1@ => 1,\n", &vars);
    let two = fill("        @E@::@V1@ => 1,\n        @E@::@V2@ => 2,\n", &vars);
    Draft {
        kind: "variant-vs-match",
        conflict: "semantic",
        variant: format!("{e}::{v2}/{g}"),
        a: TaskDraft {
            summary: format!("add {e}::{v2}, labelled {l2}"),
            body: format!("{e} needs a {v2} case. Its label is \"{l2}\"."),
            test: format!("{l2}_is_labelled"),
            assertion: format!("assert_eq!(fixture::label(&fixture::{e}::{v2}), \"{l2}\");"),
            files: vec![(LIB.into(), with_variant.clone())],
        },
        b: TaskDraft {
            summary: format!("add {g}, a number for each {e}"),
            body: format!("Add {g}(e), which is 1 for {v1}."),
            test: format!("{g}_of_{l1}"),
            assertion: format!("assert_eq!(fixture::{g}(&fixture::{e}::{v1}), 1);"),
            files: vec![(LIB.into(), format!("{base}{}", scorer(&one)))],
        },
        resolution: Some(vec![(
            LIB.into(),
            format!("{with_variant}{}", scorer(&two)),
        )]),
        lib: base,
    }
}

fn return_vs_caller(i: usize) -> Draft {
    const NAMES: [(&str, &str); 13] = [
        ("parse_id", "is_id"),
        ("parse_port", "is_port"),
        ("parse_count", "is_count"),
        ("parse_age", "is_age"),
        ("parse_level", "is_level"),
        ("parse_score", "is_score"),
        ("parse_size", "is_size"),
        ("parse_rank", "is_rank"),
        ("parse_year", "is_year"),
        ("parse_code", "is_code"),
        ("parse_limit", "is_limit"),
        ("parse_seed", "is_seed"),
        ("parse_slot", "is_slot"),
    ];
    let (f, g) = NAMES[i];
    let base = format!("pub fn {f}(s: &str) -> Option<u32> {{\n    s.parse().ok()\n}}\n");
    let result = format!(
        "pub fn {f}(s: &str) -> Result<u32, String> {{\n    s.parse().map_err(|e: std::num::ParseIntError| e.to_string())\n}}\n"
    );
    let check =
        |method: &str| format!("\npub fn {g}(s: &str) -> bool {{\n    {f}(s).{method}()\n}}\n");
    Draft {
        kind: "return-vs-caller",
        conflict: "semantic",
        variant: format!("{f}/{g}"),
        a: TaskDraft {
            summary: format!("{f} says why parsing failed"),
            body: format!("Return a Result from {f} whose error is the parse error's message."),
            test: format!("{f}_reports_errors"),
            assertion: format!("assert!(fixture::{f}(\"x\").is_err());"),
            files: vec![(LIB.into(), result.clone())],
        },
        b: TaskDraft {
            summary: format!("add {g}, whether {f} accepts the input"),
            body: format!("Add {g}(s): true when {f} accepts s."),
            test: format!("{g}_accepts_digits"),
            assertion: format!("assert!(fixture::{g}(\"5\") && !fixture::{g}(\"x\"));"),
            files: vec![(LIB.into(), format!("{base}{}", check("is_some")))],
        },
        resolution: Some(vec![(LIB.into(), format!("{result}{}", check("is_ok")))]),
        lib: base,
    }
}

fn contradiction(i: usize) -> Draft {
    const CONSTS: [&str; 6] = [
        "LIMIT",
        "MAX_USERS",
        "RETRIES",
        "TIMEOUT_SECS",
        "BATCH",
        "WORKERS",
    ];
    const FNS: [&str; 6] = ["scale", "rate", "factor", "gain", "multiplier", "ratio"];
    if i.is_multiple_of(2) {
        let c = CONSTS[i / 2];
        let (a, b) = (20 + i, 30 + i);
        let decl = |v: usize| format!("pub const {c}: u32 = {v};\n");
        Draft {
            kind: "contradiction",
            conflict: "hard",
            variant: format!("{c} = {a} or {b}"),
            a: TaskDraft {
                summary: format!("raise {c} to {a}"),
                body: format!("Production needs {c} at {a}."),
                test: format!("{}_is_{a}", c.to_lowercase()),
                assertion: format!("assert_eq!(fixture::{c}, {a});"),
                files: vec![(LIB.into(), decl(a))],
            },
            b: TaskDraft {
                summary: format!("raise {c} to {b}"),
                body: format!("Load tests need {c} at {b}."),
                test: format!("{}_is_{b}", c.to_lowercase()),
                assertion: format!("assert_eq!(fixture::{c}, {b});"),
                files: vec![(LIB.into(), decl(b))],
            },
            resolution: None,
            lib: decl(10),
        }
    } else {
        let f = FNS[i / 2];
        let (k1, k2) = (2 + i / 2, 5 + i);
        let func = |k: usize| format!("pub fn {f}(x: u32) -> u32 {{\n    x * {k}\n}}\n");
        Draft {
            kind: "contradiction",
            conflict: "semantic",
            variant: format!("{f} x{k1} or x{k2}"),
            a: TaskDraft {
                summary: format!("{f} multiplies by {k2}"),
                body: format!("The new pricing model needs {f} to multiply by {k2}."),
                test: format!("{f}_is_{k2}x"),
                assertion: format!("assert_eq!(fixture::{f}(2), {});", 2 * k2),
                files: vec![(LIB.into(), func(k2))],
            },
            b: TaskDraft {
                summary: format!("pin {f} at {k1}x with a test"),
                body: format!(
                    "Billing relies on {f} multiplying by {k1}; add a test so it cannot change."
                ),
                test: format!("{f}_stays_{k1}x"),
                assertion: format!("assert_eq!(fixture::{f}(2), {});", 2 * k1),
                files: Vec::new(),
            },
            resolution: None,
            lib: func(k1),
        }
    }
}

/// What the scripted harness does for the `n`th case (from 0): mostly one
/// resolving attempt, with some failed, killed, over-budget, and
/// give-up-twice attempts mixed in; ambiguous cases never resolve.
fn script(n: usize, ambiguous: bool) -> Vec<Step> {
    if ambiguous {
        return if n.is_multiple_of(2) {
            vec![Step::GiveUp]
        } else {
            vec![Step::Wrong, Step::Wrong]
        };
    }
    if n % 11 == 10 {
        vec![Step::GiveUp, Step::GiveUp]
    } else if n % 13 == 6 {
        vec![Step::Sleep, Step::Resolve]
    } else if n % 9 == 4 {
        vec![Step::Wrong, Step::Resolve]
    } else if n % 7 == 3 {
        vec![Step::OverBudget, Step::Resolve]
    } else {
        vec![Step::Resolve]
    }
}

/// A template: one case per variant number.
type Template = fn(usize) -> Draft;

/// The corpus: 100 cases, in order.
#[must_use]
pub fn corpus() -> Vec<Case> {
    let templates: [(Template, usize); 8] = [
        (same_tokens, 13),
        (signature_vs_caller, 13),
        (rename_vs_caller, 12),
        (move_vs_edit, 12),
        (field_vs_literal, 13),
        (variant_vs_match, 12),
        (return_vs_caller, 13),
        (contradiction, 12),
    ];
    let mut out = Vec::new();
    for (template, count) in templates {
        for i in 0..count {
            let n = out.len();
            let draft = template(i);
            let ambiguous = draft.resolution.is_none();
            let second = draft.b.build("b", TEST_B);
            let resolution = draft.resolution.map(|files| {
                let mut files: BTreeMap<String, String> = files.into_iter().collect();
                // The second task's acceptance test is part of meeting its
                // intent; the first's is already on the new base.
                files.insert(second.test_path.clone(), second.test_source.clone());
                files
            });
            let base = files(&[
                ("Cargo.toml", CARGO_TOML.into()),
                (".gitignore", GITIGNORE.into()),
                (LIB, draft.lib),
            ]);
            out.push(Case {
                id: format!("m5-{:03}", n + 1),
                kind: draft.kind.into(),
                conflict: draft.conflict.into(),
                ambiguous,
                made_by: format!("{VERSION}: {} #{} ({})", draft.kind, i + 1, draft.variant),
                base,
                tasks: vec![draft.a.build("a", TEST_A), second],
                resolution,
                script: script(n, ambiguous),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn a_hundred_valid_cases_with_distinct_ids() -> anyhow::Result<()> {
        let cases = corpus();
        assert_eq!(cases.len(), 100);
        let ids: std::collections::BTreeSet<_> = cases.iter().map(|c| c.id.clone()).collect();
        assert_eq!(ids.len(), 100);
        for case in &cases {
            case.validate()?;
        }
        let ambiguous = cases.iter().filter(|c| c.ambiguous).count();
        assert_eq!(ambiguous, 12);
        Ok(())
    }

    /// The checked-in corpus is what this generator writes.
    #[test]
    fn the_checked_in_corpus_is_the_generators() -> anyhow::Result<()> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpora/m5/cases");
        let loaded = crate::corpus::load(&dir)?;
        let generated = corpus();
        assert_eq!(
            loaded.len(),
            generated.len(),
            "regenerate: hord-eval-m5 generate"
        );
        for ((path, _), case) in loaded.iter().zip(&generated) {
            let text = crate::corpus::read_text(path)?;
            assert_eq!(
                text,
                crate::corpus::write(case)?,
                "{} differs from the generator; regenerate: hord-eval-m5 generate",
                path.display()
            );
        }
        Ok(())
    }

    /// A case file converted to CRLF (git on Windows) reads as the same case.
    #[test]
    fn a_crlf_checkout_reads_the_same_case() -> anyhow::Result<()> {
        let case = &corpus()[0];
        let lf = crate::corpus::write(case)?;
        let dir = std::env::temp_dir().join(format!("hord-m5-crlf-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("case.toml");
        std::fs::write(&path, lf.replace('\n', "\r\n"))?;
        let read = crate::corpus::read(&path);
        let text = crate::corpus::read_text(&path);
        std::fs::remove_dir_all(&dir)?;
        assert_eq!(text?, lf);
        assert_eq!(crate::corpus::write(&read?)?, lf);
        Ok(())
    }
}
