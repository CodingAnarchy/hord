//! `hord audit --since <date> [--until <date>]` (spec §12 M6): M6's
//! acceptance criteria over a window of the log, through the `Audit`
//! service of the session's repository (the daemon, `--remote`, or, with
//! `--no-daemon`, this process). Prints the report, as text or the
//! `AuditReport` JSON mapping, and fails when it has a violation.
//!
//! In process, keys are checked against `server.toml`'s `[auth] file`
//! when the repository has one.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use hord_api::proto::{self, AuditCriterion, AuditOrigin};
use hord_api::{AuditBackend, wire};
use hord_server::{AuthStore, LocalAudit, ServerConfig};
use hord_txn::LocalRepo;
use time::format_description::well_known::{Iso8601, Rfc3339};
use time::{Date, OffsetDateTime, Time};

use crate::output;
use crate::session::{Session, Target};
use crate::txn::{block_on, short};

/// `date` (`YYYY-MM-DD`, midnight UTC) or an RFC 3339 time, in ms since
/// the Unix epoch.
fn parse_time(date: &str) -> Result<u64> {
    let at = match OffsetDateTime::parse(date, &Rfc3339) {
        Ok(at) => at,
        Err(_) => Date::parse(date, &Iso8601::DEFAULT)
            .map(|day| day.with_time(Time::MIDNIGHT).assume_utc())
            .with_context(|| {
                format!("{date:?} is neither a date (2026-09-01) nor an RFC 3339 time")
            })?,
    };
    u64::try_from(at.unix_timestamp_nanos() / 1_000_000)
        .with_context(|| format!("{date:?} is before 1970"))
}

/// `ms` as an RFC 3339 time, for the text report.
fn show_time(ms: u64) -> String {
    i128::from(ms)
        .checked_mul(1_000_000)
        .and_then(|nanos| OffsetDateTime::from_unix_timestamp_nanos(nanos).ok())
        .and_then(|at| at.format(&Rfc3339).ok())
        .unwrap_or_else(|| format!("{ms} ms"))
}

fn origin_name(origin: AuditOrigin) -> &'static str {
    match origin {
        AuditOrigin::Signed => "signed",
        AuditOrigin::BridgeVouched => "bridge-vouched",
        AuditOrigin::Replay => "replay",
        AuditOrigin::Arbitration => "arbitration",
        AuditOrigin::SignedUnbound => "signed (binding not checked)",
        AuditOrigin::Unsigned => "unsigned",
        AuditOrigin::Unspecified => "unknown",
    }
}

fn criterion_name(criterion: AuditCriterion) -> &'static str {
    match criterion {
        AuditCriterion::Intent => "intent",
        AuditCriterion::Provenance => "provenance",
        AuditCriterion::Evidence => "evidence",
        AuditCriterion::Policy => "policy",
        AuditCriterion::Review => "review",
        AuditCriterion::Arbitration => "arbitration",
        AuditCriterion::UnrecordedLanding => "store edit",
        AuditCriterion::BridgeDiverged => "bridge diverged",
        AuditCriterion::BridgeGap => "bridge checks",
        AuditCriterion::Unspecified => "note",
    }
}

fn print_finding(finding: &proto::AuditFinding) {
    let change = finding
        .change
        .as_deref()
        .map(|c| format!(" {}", short(c)))
        .unwrap_or_default();
    println!(
        "  {}{change}: {}",
        criterion_name(finding.criterion()),
        finding.detail
    );
}

fn print_report(report: &proto::AuditReport) {
    println!(
        "audit {} .. {}",
        show_time(report.since_ms),
        show_time(report.until_ms)
    );
    let count = |origin: AuditOrigin| {
        report
            .changes
            .iter()
            .filter(|c| c.origin() == origin)
            .count()
    };
    println!(
        "{} landed: {} signed, {} bridge-vouched, {} replays, {} arbitration results, \
         {} signed without a binding check, {} unsigned",
        report.changes.len(),
        count(AuditOrigin::Signed),
        count(AuditOrigin::BridgeVouched),
        count(AuditOrigin::Replay),
        count(AuditOrigin::Arbitration),
        count(AuditOrigin::SignedUnbound),
        count(AuditOrigin::Unsigned),
    );
    println!(
        "{} reviews, {} arbitrations",
        report.reviews, report.arbitrations
    );
    let bridge = report.bridge.unwrap_or_default();
    if bridge.checks == 0 {
        println!("bridge: no bridge checks recorded");
    } else {
        println!(
            "bridge: {} checks, {} diverged, longest gap {} min",
            bridge.checks,
            bridge.diverged,
            bridge.longest_gap_ms / 60_000
        );
    }
    for change in &report.changes {
        let mark = if change.ok { "ok  " } else { "FAIL" };
        let actor = change.actor.as_ref().map_or("?", wire::actor_id);
        println!(
            "{mark} {:>5} {} {} [{}] {}",
            change.position,
            short(&change.change),
            actor,
            origin_name(change.origin()),
            change.summary
        );
    }
    if !report.notes.is_empty() {
        println!("notes:");
        report.notes.iter().for_each(print_finding);
    }
    if report.violations.is_empty() {
        println!("ok: no violations");
    } else {
        println!("violations:");
        report.violations.iter().for_each(print_finding);
    }
}

/// The in-process audit of a directly opened repository, with the key
/// bindings of `server.toml`'s `[auth] file`, if any.
fn local_audit(repo: &hord_txn::Repo) -> Result<LocalAudit> {
    let root = repo.store().repo_root();
    let config = ServerConfig::load(&root.join(hord_store::HORD_DIR).join("server.toml"))?;
    let auth = match &config.auth {
        Some(auth) => Some(Arc::new(
            AuthStore::open(&auth.file)
                .with_context(|| format!("open auth file {}", auth.file.display()))?,
        )),
        None => None,
    };
    Ok(LocalAudit::new(
        Arc::new(LocalRepo::without_lander(repo.clone())),
        auth,
    ))
}

pub fn run(
    json: bool,
    target: &Target,
    since: &str,
    until: Option<&str>,
    max_bridge_gap: Option<u64>,
    require_bridge: bool,
) -> Result<()> {
    let request = proto::AuditRequest {
        since_ms: parse_time(since)?,
        until_ms: until.map(parse_time).transpose()?,
        max_bridge_gap_ms: max_bridge_gap.map_or(0, |minutes| minutes.saturating_mul(60_000)),
        require_bridge,
    };
    let session = Session::open(target)?;
    let report = match &session {
        Session::Daemon { remote } | Session::Remote { remote, .. } => {
            block_on(remote.audit().audit_log(request))?
        }
        Session::Direct { repo } => block_on(local_audit(repo)?.audit_log(request))?,
    };
    if json {
        output::print_json(&report)?;
    } else {
        print_report(&report);
    }
    if !report.ok {
        bail!(
            "audit failed: {} violation{}",
            report.violations.len(),
            if report.violations.len() == 1 {
                ""
            } else {
                "s"
            }
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_times_parse_to_epoch_ms() -> Result<()> {
        assert_eq!(parse_time("1970-01-02")?, 86_400_000);
        assert_eq!(parse_time("1970-01-01T00:00:01Z")?, 1_000);
        assert_eq!(parse_time("1970-01-01T01:00:00+01:00")?, 0);
        assert!(parse_time("yesterday").is_err());
        assert!(parse_time("1969-12-31").is_err());
        assert_eq!(show_time(1_000), "1970-01-01T00:00:01Z");
        Ok(())
    }
}
