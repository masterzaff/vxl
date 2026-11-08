//! Retention policies: which snapshots `prune` keeps.
//!
//! Like restic's `forget`: `--keep-daily 7` keeps the newest snapshot of each of the
//! 7 most recent days that have snapshots, and so on for each period. A snapshot is
//! kept if any rule keeps it. Snapshots are grouped by what they back up (source
//! folder + worlds), and each group gets the policy on its own, so one repo can hold
//! several servers or worlds without one pushing out the others.

use std::collections::BTreeMap;

use chrono::{Datelike, TimeZone, Timelike};
use serde::{Deserialize, Serialize};

use crate::manifest::SnapshotHeader;
use crate::repo::ObjectId;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// The newest N snapshots.
    pub last: Option<u32>,
    /// The newest snapshot of each of the last N hours / days / weeks / months / years
    /// that have one.
    pub hourly: Option<u32>,
    pub daily: Option<u32>,
    pub weekly: Option<u32>,
    pub monthly: Option<u32>,
    pub yearly: Option<u32>,
}

impl Policy {
    /// No rule at all: `prune` keeps everything and only collects garbage.
    pub fn is_empty(&self) -> bool {
        self.rules().iter().all(|(_, n)| n.is_none())
    }

    fn rules(&self) -> [(Period, Option<u32>); 6] {
        [
            (Period::Last, self.last),
            (Period::Hourly, self.hourly),
            (Period::Daily, self.daily),
            (Period::Weekly, self.weekly),
            (Period::Monthly, self.monthly),
            (Period::Yearly, self.yearly),
        ]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Period {
    Last,
    Hourly,
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

impl Period {
    pub fn name(self) -> &'static str {
        match self {
            Period::Last => "last",
            Period::Hourly => "hourly",
            Period::Daily => "daily",
            Period::Weekly => "weekly",
            Period::Monthly => "monthly",
            Period::Yearly => "yearly",
        }
    }

    /// Which period a moment falls into (`None` for `Last`: every snapshot is its own).
    fn key<Tz: TimeZone>(self, h: &SnapshotHeader, tz: &Tz) -> Option<i64> {
        let t = h.created.with_timezone(tz);
        let (y, m) = (t.year() as i64, t.month() as i64);
        let days = t.date_naive().num_days_from_ce() as i64;
        match self {
            Period::Last => None,
            Period::Hourly => Some(days * 24 + t.hour() as i64),
            Period::Daily => Some(days),
            Period::Weekly => {
                let w = t.iso_week();
                Some(w.year() as i64 * 100 + w.week() as i64)
            }
            Period::Monthly => Some(y * 12 + m),
            Period::Yearly => Some(y),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Decision {
    pub id: ObjectId,
    /// Why it's kept; empty = it goes.
    pub kept_by: Vec<Period>,
}

/// Decide for every snapshot whether `policy` keeps it, in `tz` (the user's local
/// time for real use). Output is newest first. An empty policy keeps everything.
pub fn apply<Tz: TimeZone>(policy: &Policy, snaps: &[(ObjectId, SnapshotHeader)], tz: &Tz) -> Vec<Decision> {
    // Group by what was backed up; inside a group, newest first.
    type Group<'a> = Vec<&'a (ObjectId, SnapshotHeader)>;
    let mut groups: BTreeMap<(&str, &[String]), Group> = BTreeMap::new();
    for s in snaps {
        groups.entry((s.1.source.as_str(), s.1.world_names.as_slice())).or_default().push(s);
    }
    let mut out = Vec::with_capacity(snaps.len());
    for group in groups.values_mut() {
        group.sort_by(|a, b| b.1.created.cmp(&a.1.created).then(a.0.cmp(&b.0)));
        let mut kept_by = vec![Vec::new(); group.len()];
        if policy.is_empty() {
            kept_by.iter_mut().for_each(|k| k.push(Period::Last));
        }
        for (period, n) in policy.rules() {
            let Some(n) = n else { continue };
            let mut last_key = None;
            let mut count = 0;
            for (i, (_, h)) in group.iter().enumerate() {
                if count >= n {
                    break;
                }
                let key = period.key(h, tz);
                if key.is_none() || key != last_key {
                    kept_by[i].push(period);
                    count += 1;
                    last_key = key;
                }
            }
        }
        out.extend(
            group.iter().zip(kept_by).map(|((id, h), kept_by)| (h.created, Decision { id: *id, kept_by })),
        );
    }
    out.sort_by_key(|(created, _)| std::cmp::Reverse(*created));
    out.into_iter().map(|(_, d)| d).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Stats;
    use chrono::{DateTime, Duration, Utc};

    fn snap(n: u8, created: DateTime<Utc>, source: &str) -> (ObjectId, SnapshotHeader) {
        let h = SnapshotHeader {
            version: 2,
            created,
            message: None,
            source: source.into(),
            stats: Stats::default(),
            world_names: vec!["world".into()],
        };
        (ObjectId::of(&[n]), h)
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// Kept snapshots as their creation times.
    fn kept(policy: &Policy, snaps: &[(ObjectId, SnapshotHeader)]) -> Vec<String> {
        let created: BTreeMap<ObjectId, DateTime<Utc>> =
            snaps.iter().map(|(id, h)| (*id, h.created)).collect();
        apply(policy, snaps, &Utc)
            .into_iter()
            .filter(|d| !d.kept_by.is_empty())
            .map(|d| created[&d.id].format("%Y-%m-%d %H:%M").to_string())
            .collect()
    }

    /// One snapshot every 6 hours for 60 days, starting Monday 2026-09-07.
    fn every_six_hours() -> Vec<(ObjectId, SnapshotHeader)> {
        let start = at("2026-09-07T00:00:00Z");
        (0..240).map(|i| snap(i as u8, start + Duration::hours(6 * i), "/srv/mc")).collect()
    }

    #[test]
    fn empty_policy_keeps_everything() {
        let snaps = every_six_hours();
        assert_eq!(kept(&Policy::default(), &snaps).len(), 240);
    }

    #[test]
    fn keep_last() {
        let snaps = every_six_hours();
        let p = Policy { last: Some(3), ..Default::default() };
        assert_eq!(kept(&p, &snaps), ["2026-11-05 18:00", "2026-11-05 12:00", "2026-11-05 06:00"]);
    }

    #[test]
    fn keep_daily_takes_the_newest_of_each_day() {
        let snaps = every_six_hours();
        let p = Policy { daily: Some(3), ..Default::default() };
        assert_eq!(kept(&p, &snaps), ["2026-11-05 18:00", "2026-11-04 18:00", "2026-11-03 18:00"]);
    }

    #[test]
    fn rules_combine() {
        let snaps = every_six_hours();
        let p =
            Policy { last: Some(2), daily: Some(2), weekly: Some(2), monthly: Some(3), ..Default::default() };
        assert_eq!(
            kept(&p, &snaps),
            [
                "2026-11-05 18:00", // last, daily, weekly, monthly
                "2026-11-05 12:00", // last
                "2026-11-04 18:00", // daily
                "2026-11-01 18:00", // weekly (Sunday ends ISO week 44)
                "2026-10-31 18:00", // monthly
                "2026-09-30 18:00", // monthly
            ]
        );
        let d = apply(&p, &snaps, &Utc);
        assert_eq!(d[0].kept_by, [Period::Last, Period::Daily, Period::Weekly, Period::Monthly]);
    }

    #[test]
    fn periods_without_snapshots_dont_count() {
        // Three snapshots, weeks apart: keep-daily 3 keeps all three.
        let snaps: Vec<_> = ["2026-11-01T03:00:00Z", "2026-10-15T03:00:00Z", "2026-09-01T03:00:00Z"]
            .iter()
            .enumerate()
            .map(|(i, t)| snap(i as u8, at(t), "/srv/mc"))
            .collect();
        let p = Policy { daily: Some(3), ..Default::default() };
        assert_eq!(kept(&p, &snaps).len(), 3);
    }

    #[test]
    fn each_source_gets_its_own_policy() {
        let mut snaps = every_six_hours();
        snaps.push(snap(250, at("2026-01-01T00:00:00Z"), "/srv/creative"));
        let p = Policy { last: Some(1), ..Default::default() };
        assert_eq!(kept(&p, &snaps), ["2026-11-05 18:00", "2026-01-01 00:00"]);
    }

    #[test]
    fn local_time_decides_the_day() {
        // 23:30 and 00:30 UTC are the same day at UTC-2.
        let snaps =
            vec![snap(1, at("2026-11-02T00:30:00Z"), "/s"), snap(2, at("2026-11-01T23:30:00Z"), "/s")];
        let p = Policy { daily: Some(5), ..Default::default() };
        assert_eq!(apply(&p, &snaps, &Utc).iter().filter(|d| !d.kept_by.is_empty()).count(), 2);
        let minus2 = chrono::FixedOffset::west_opt(2 * 3600).unwrap();
        assert_eq!(apply(&p, &snaps, &minus2).iter().filter(|d| !d.kept_by.is_empty()).count(), 1);
    }
}
