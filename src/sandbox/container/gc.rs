//! Deciding which places to reclaim, as a pure function.
//!
//! A place outlives the command that made it, so something has to decide when
//! it goes. Two facts shape the whole of it:
//!
//! - **The record and the real thing can disagree.** `sandbox_instances` is a
//!   record *of* a container, keyed on `(engine, external_id)` and never on the
//!   profile — precisely so a rebuild adds a row instead of overwriting the
//!   previous id into a leak nothing can find again. So the two sides are
//!   reconciled here: a row whose container is gone is forgotten, a container of
//!   friring's whose row is gone is adopted or reclaimed.
//! - **friring touches only what it created.** Every candidate carries
//!   [`LiveContainer::owned`], read from friring's own label, and anything
//!   without it is invisible to every branch below. The label filter on the
//!   engine query says the same thing; this is the half that cannot be got wrong
//!   by a mistyped filter.
//!
//! Nothing here runs a command — the caller executes the plan — which is what
//! makes "would this reap the container a session is using?" a unit test.

use std::collections::BTreeMap;

/// One row of `sandbox_instances`, as garbage collection needs it.
///
/// A copy rather than the storage type: `sandbox` may not reference `storage`
/// (the architecture allowlist), and the coordinator that holds both converts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceRecord {
    pub profile: String,
    pub external_id: String,
    /// Unix millis, refreshed whenever a launch used the place.
    pub last_used_at: u64,
}

/// One container the engine reports, as friring's own query saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveContainer {
    pub id: String,
    /// The profile from friring's label, or `None` when the container carries
    /// none.
    pub profile: Option<String>,
    /// The spec digest from friring's label — what says whether this place still
    /// matches the profile it was built for.
    pub spec: Option<String>,
    /// Whether friring's owner label is on it. Anything else is somebody's
    /// container and is never touched.
    pub owned: bool,
}

/// What to do, in the order a caller should do it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcPlan {
    /// Containers to stop and remove. Every one of them is friring's own.
    pub remove: Vec<String>,
    /// Rows to delete: the container behind them is gone, or is about to be.
    pub forget: Vec<String>,
    /// Containers of friring's that no row describes and that still match a live
    /// profile — a friring that crashed between creating one and recording it.
    /// The caller re-records these rather than leaking them.
    pub adopt: Vec<LiveContainer>,
}

/// Everything the decision needs.
pub struct GcInput<'a> {
    /// Rows recorded for this engine.
    pub records: &'a [InstanceRecord],
    /// What the engine reports right now, already filtered to friring's label.
    pub live: &'a [LiveContainer],
    /// The spec digest each *existing* profile currently resolves to. A profile
    /// absent from this map no longer exists, or could not be resolved — either
    /// way nothing should still be running for it.
    pub current: &'a BTreeMap<String, String>,
    /// Container ids that a live session is running in. Never reclaimed,
    /// whatever else is true of them: pulling a place out from under a running
    /// agent loses its work.
    pub in_use: &'a [String],
    /// Now, in unix millis.
    pub now: u64,
    /// How long a place may sit unused before it is reclaimed. `None` keeps idle
    /// places forever, which is what a manager view's manual-only mode wants.
    pub idle_after_ms: Option<u64>,
}

/// Decide what to reclaim.
///
/// The rules, in the order they are applied per container:
///
/// 1. Not friring's → invisible. Nothing below can name it.
/// 2. In use by a session → kept, whatever else is true.
/// 3. Its profile is gone → removed, and its row forgotten.
/// 4. Its spec no longer matches the profile's → superseded by a rebuild →
///    removed and forgotten.
/// 5. Idle longer than `idle_after_ms` → removed and forgotten.
/// 6. Otherwise kept, and adopted if no row describes it.
///
/// A row whose container the engine does not report is forgotten on its own: the
/// place is already gone, and keeping the row would make a later rebuild look
/// like a leak.
pub fn gc_plan(input: GcInput<'_>) -> GcPlan {
    let mut plan = GcPlan::default();
    let known: Vec<&str> = input.live.iter().map(|c| c.id.as_str()).collect();

    for record in input.records {
        if !known.contains(&record.external_id.as_str()) {
            plan.forget.push(record.external_id.clone());
        }
    }

    for container in input.live {
        if !container.owned {
            continue;
        }
        if input.in_use.iter().any(|id| id == &container.id) {
            continue;
        }
        let record = input.records.iter().find(|r| r.external_id == container.id);
        let profile = container
            .profile
            .clone()
            .or_else(|| record.map(|r| r.profile.clone()));
        let expected = profile.as_ref().and_then(|name| input.current.get(name));

        let reclaim = match (expected, &container.spec) {
            // The profile is gone: nothing will ever use this place again.
            (None, _) => true,
            // A rebuild replaced it. Keeping it costs disk and a stale row; the
            // row for the *new* container is a separate one, which is why the
            // table is keyed on the container rather than on the profile.
            (Some(current), Some(spec)) => current != spec,
            // Ours, for a live profile, but carrying no spec label: an older
            // friring's, or a label that was stripped. Not reclaimed on those
            // grounds — only idleness can take it.
            (Some(_), None) => false,
        };
        let idle = match (input.idle_after_ms, record) {
            (Some(limit), Some(record)) => input.now.saturating_sub(record.last_used_at) > limit,
            _ => false,
        };

        if reclaim || idle {
            plan.remove.push(container.id.clone());
            if record.is_some() {
                plan.forget.push(container.id.clone());
            }
        } else if record.is_none() {
            plan.adopt.push(container.clone());
        }
    }

    plan.forget.sort();
    plan.forget.dedup();
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(profile: &str, id: &str, last_used_at: u64) -> InstanceRecord {
        InstanceRecord {
            profile: profile.to_string(),
            external_id: id.to_string(),
            last_used_at,
        }
    }

    fn live(id: &str, profile: &str, spec: &str) -> LiveContainer {
        LiveContainer {
            id: id.to_string(),
            profile: Some(profile.to_string()),
            spec: Some(spec.to_string()),
            owned: true,
        }
    }

    fn current(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn plan(
        records: &[InstanceRecord],
        containers: &[LiveContainer],
        current: &BTreeMap<String, String>,
        in_use: &[String],
    ) -> GcPlan {
        gc_plan(GcInput {
            records,
            live: containers,
            current,
            in_use,
            now: 10_000,
            idle_after_ms: None,
        })
    }

    #[test]
    fn a_place_still_matching_its_profile_is_left_alone() {
        let records = [record("dev", "c1", 9_000)];
        let containers = [live("c1", "dev", "aaaa")];
        let plan = plan(&records, &containers, &current(&[("dev", "aaaa")]), &[]);
        assert_eq!(plan, GcPlan::default());
    }

    /// The case the `(engine, external_id)` key exists for: a rebuild leaves the
    /// previous container behind, and it has to still be findable.
    #[test]
    fn a_superseded_place_is_removed_and_the_new_one_kept() {
        let records = [record("dev", "old", 9_000), record("dev", "new", 9_900)];
        let containers = [live("old", "dev", "aaaa"), live("new", "dev", "bbbb")];
        let plan = plan(&records, &containers, &current(&[("dev", "bbbb")]), &[]);
        assert_eq!(plan.remove, ["old"]);
        assert_eq!(plan.forget, ["old"]);
        assert!(plan.adopt.is_empty());
    }

    #[test]
    fn a_place_a_session_is_using_survives_every_other_rule() {
        let records = [record("dev", "old", 0)];
        let containers = [live("old", "dev", "aaaa")];
        // Superseded, idle since the epoch, and its profile still exists: the
        // only thing that matters is that an agent is running in it.
        let plan = gc_plan(GcInput {
            records: &records,
            live: &containers,
            current: &current(&[("dev", "bbbb")]),
            in_use: &["old".to_string()],
            now: 10_000,
            idle_after_ms: Some(1),
        });
        assert_eq!(plan, GcPlan::default());
    }

    #[test]
    fn a_deleted_profiles_place_goes_and_so_does_its_row() {
        let records = [record("gone", "c1", 9_000)];
        let containers = [live("c1", "gone", "aaaa")];
        let plan = plan(&records, &containers, &current(&[("dev", "aaaa")]), &[]);
        assert_eq!(plan.remove, ["c1"]);
        assert_eq!(plan.forget, ["c1"]);
    }

    #[test]
    fn a_row_whose_container_is_gone_is_forgotten_without_a_removal() {
        let records = [record("dev", "vanished", 9_000)];
        let plan = plan(&records, &[], &current(&[("dev", "aaaa")]), &[]);
        assert!(plan.remove.is_empty());
        assert_eq!(plan.forget, ["vanished"]);
    }

    /// A friring that died between `run` and the row it was about to write.
    #[test]
    fn an_unrecorded_place_of_ours_is_adopted_rather_than_leaked() {
        let containers = [live("orphan", "dev", "aaaa")];
        let plan = plan(&[], &containers, &current(&[("dev", "aaaa")]), &[]);
        assert!(plan.remove.is_empty());
        assert_eq!(plan.adopt, containers);

        // The same orphan for a profile that no longer resolves is reclaimed,
        // and there is no row to forget.
        let plan = plan_for_empty_profiles(&containers);
        assert_eq!(plan.remove, ["orphan"]);
        assert!(plan.forget.is_empty());
    }

    fn plan_for_empty_profiles(containers: &[LiveContainer]) -> GcPlan {
        plan(&[], containers, &BTreeMap::new(), &[])
    }

    #[test]
    fn nothing_friring_did_not_create_is_ever_named() {
        let foreign = LiveContainer {
            id: "someone-elses".to_string(),
            profile: None,
            spec: None,
            owned: false,
        };
        let plan = gc_plan(GcInput {
            records: &[],
            live: &[foreign],
            current: &BTreeMap::new(),
            in_use: &[],
            now: 10_000,
            idle_after_ms: Some(1),
        });
        assert_eq!(plan, GcPlan::default());
    }

    #[test]
    fn an_idle_place_is_reclaimed_only_once_it_is_past_the_limit() {
        let records = [record("dev", "c1", 5_000)];
        let containers = [live("c1", "dev", "aaaa")];
        let current = current(&[("dev", "aaaa")]);
        let idle = |limit: u64| {
            gc_plan(GcInput {
                records: &records,
                live: &containers,
                current: &current,
                in_use: &[],
                now: 10_000,
                idle_after_ms: Some(limit),
            })
        };
        assert!(idle(6_000).remove.is_empty());
        assert_eq!(idle(4_000).remove, ["c1"]);
        // Unused places are kept forever when nothing sets a limit.
        assert!(plan(&records, &containers, &current, &[]).remove.is_empty());
    }
}
