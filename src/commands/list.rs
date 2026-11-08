use anyhow::{Result, bail};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};

use crate::manifest::SnapshotHeader;
use crate::repo::{ObjectId, Repo};

/// All snapshots, newest first.
pub fn list(repo: &Repo) -> Result<Vec<(ObjectId, SnapshotHeader)>> {
    let mut out = repo
        .snapshot_ids()?
        .into_iter()
        .map(|id| Ok((id, repo.load_header(&id)?)))
        .collect::<Result<Vec<_>>>()?;
    out.sort_by_key(|(_, h)| std::cmp::Reverse(h.created));
    Ok(out)
}

/// Resolve what the user typed to a snapshot: an id (any unique prefix), `latest`, or a
/// local time (`2026-11-07`, `2026-11-07 03:00`, `2026-11-07T03:00:00`), meaning the
/// newest snapshot at or before then (a bare date = the end of that day).
///
/// `latest` and times refuse to guess when the repo holds snapshots of several sources
/// that would match; use an id then.
pub fn resolve(repo: &Repo, spec: &str) -> Result<ObjectId> {
    let spec = spec.trim();
    if spec.eq_ignore_ascii_case("latest") {
        return newest(&list(repo)?, None, spec);
    }
    if let Some(t) = parse_time(spec, &Local)? {
        return newest(&list(repo)?, Some(t), spec);
    }
    repo.resolve(spec)
}

/// `Ok(None)` if `s` doesn't look like a time at all (so it's an id).
pub fn parse_time<Tz: TimeZone>(s: &str, tz: &Tz) -> Result<Option<DateTime<Utc>>> {
    let naive = if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        d.and_hms_opt(23, 59, 59).unwrap()
    } else if let Some(n) = ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"]
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(s, f).ok())
    {
        n
    } else if s.contains('-') || s.contains(':') {
        bail!("{s:?} is neither a snapshot id nor a time like 2026-11-07 or \"2026-11-07 03:00\"");
    } else {
        return Ok(None);
    };
    match tz.from_local_datetime(&naive).earliest() {
        Some(t) => Ok(Some(t.with_timezone(&Utc))),
        None => bail!("{s:?} doesn't exist in local time (a daylight-saving gap)"),
    }
}

/// The newest of `snaps` (newest first) created at or before `at` (any time if `None`).
fn newest(snaps: &[(ObjectId, SnapshotHeader)], at: Option<DateTime<Utc>>, spec: &str) -> Result<ObjectId> {
    let candidates: Vec<_> = snaps.iter().filter(|(_, h)| at.is_none_or(|t| h.created <= t)).collect();
    let Some((id, _)) = candidates.first() else {
        bail!("no snapshot at or before {spec:?} (see `vxl list`)");
    };
    let mut sources: Vec<&str> = candidates.iter().map(|(_, h)| h.source.as_str()).collect();
    sources.sort();
    sources.dedup();
    if sources.len() > 1 {
        bail!(
            "{spec:?} is ambiguous: this repo has snapshots of {} sources ({}); use a snapshot id",
            sources.len(),
            sources.join(", ")
        );
    }
    Ok(*id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Stats;

    fn snap(n: u8, created: &str, source: &str) -> (ObjectId, SnapshotHeader) {
        let created = DateTime::parse_from_rfc3339(created).unwrap().with_timezone(&Utc);
        let h = SnapshotHeader {
            version: 2,
            created,
            message: None,
            source: source.into(),
            stats: Stats::default(),
            world_names: vec![],
        };
        (ObjectId::of(&[n]), h)
    }

    #[test]
    fn times() {
        let t = |s: &str| parse_time(s, &Utc).unwrap().map(|t| t.to_rfc3339());
        assert_eq!(t("2026-11-07").as_deref(), Some("2026-11-07T23:59:59+00:00"));
        assert_eq!(t("2026-11-07 03:00").as_deref(), Some("2026-11-07T03:00:00+00:00"));
        assert_eq!(t("2026-11-07T03:00:05").as_deref(), Some("2026-11-07T03:00:05+00:00"));
        assert_eq!(t("3f2a9c"), None);
        assert!(parse_time("2026-13-01", &Utc).is_err());
        assert!(parse_time("yesterday-ish", &Utc).is_err());
    }

    #[test]
    fn newest_at_or_before() {
        let snaps = [
            snap(3, "2026-11-08T03:00:00Z", "/srv/mc"),
            snap(2, "2026-11-07T03:00:00Z", "/srv/mc"),
            snap(1, "2026-11-06T03:00:00Z", "/srv/mc"),
        ];
        let at = |s: &str| parse_time(s, &Utc).unwrap();
        assert_eq!(newest(&snaps, None, "latest").unwrap(), snaps[0].0);
        assert_eq!(newest(&snaps, at("2026-11-07"), "x").unwrap(), snaps[1].0);
        assert_eq!(newest(&snaps, at("2026-11-07 03:00"), "x").unwrap(), snaps[1].0);
        assert_eq!(newest(&snaps, at("2026-11-07 02:59"), "x").unwrap(), snaps[2].0);
        assert!(newest(&snaps, at("2026-11-01"), "x").unwrap_err().to_string().contains("no snapshot"));
    }

    #[test]
    fn several_sources_are_ambiguous() {
        let snaps = [snap(2, "2026-11-08T03:00:00Z", "/srv/a"), snap(1, "2026-11-07T03:00:00Z", "/srv/b")];
        assert!(newest(&snaps, None, "latest").unwrap_err().to_string().contains("ambiguous"));
        // …unless the time rules all but one out.
        let at = parse_time("2026-11-07 12:00", &Utc).unwrap();
        assert_eq!(newest(&snaps, at, "x").unwrap(), snaps[1].0);
    }
}
