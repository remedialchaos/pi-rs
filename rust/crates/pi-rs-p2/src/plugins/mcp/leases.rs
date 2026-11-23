//! The rules of MCP leases, as pure functions over the lease list.
//!
//! A session worker sets the locations it works at for its generation (a newer generation takes
//! the session over, an older one is refused), its calls go through the lease at a location only
//! from the incarnation that set it, and its view's end releases its generation's leases.

use crate::runtime::location::{Location, same_location};
use serde::{Deserialize, Serialize};

/// The project configuration at a location as the worker read it: none, the file's text, or why it could not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ProjectSource {
    None,
    File { text: String },
    Unreadable { error: String },
}

/// A session worker's lease on the servers at one location, from the generation and incarnation that set it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub session: String,
    pub location: Location,
    pub project: ProjectSource,
    pub generation: u64,
    pub incarnation: String,
}

/// A location to lease, with its project configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeasePlace {
    pub location: Location,
    pub project: ProjectSource,
}

/// Who asks: one incarnation of one generation of a session's worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseCaller {
    pub session: String,
    pub generation: u64,
    pub incarnation: String,
}

/// `set_locations`'s outcome: the new lease list, or why the caller may not set it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeasesSet {
    Set { leases: Vec<Lease> },
    Refused { reason: String },
}

/// The lease `call` goes through: held by the caller, none at that location, or another incarnation's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaseHeld {
    Held { lease: Lease },
    None,
    Stale,
}

/// `caller`'s session leases exactly `places` from now on, for `caller`'s generation and incarnation
/// (of a location given twice, the first counts): its leases elsewhere end, those of older
/// generations (a worker that is gone) are taken over. A newer generation's lease refuses.
pub fn set_locations(leases: &[Lease], caller: &LeaseCaller, places: &[LeasePlace]) -> LeasesSet {
    let own = leases
        .iter()
        .filter(|lease| lease.session == caller.session);
    if own
        .clone()
        .any(|lease| lease.generation > caller.generation)
    {
        return LeasesSet::Refused {
            reason: "A newer worker of this session holds its MCP servers".into(),
        };
    }
    let mut wanted: Vec<&LeasePlace> = Vec::new();
    for place in places {
        if !wanted
            .iter()
            .any(|other| same_location(&other.location, &place.location))
        {
            wanted.push(place);
        }
    }
    let mut next: Vec<Lease> = leases
        .iter()
        .filter(|lease| lease.session != caller.session)
        .cloned()
        .collect();
    next.extend(wanted.into_iter().map(|place| Lease {
        session: caller.session.clone(),
        location: place.location.clone(),
        project: place.project.clone(),
        generation: caller.generation,
        incarnation: caller.incarnation.clone(),
    }));
    LeasesSet::Set { leases: next }
}

/// The lease `caller` calls through at `location`.
pub fn held_lease(leases: &[Lease], caller: &LeaseCaller, location: &Location) -> LeaseHeld {
    let Some(lease) = leases.iter().find(|candidate| {
        candidate.session == caller.session && same_location(&candidate.location, location)
    }) else {
        return LeaseHeld::None;
    };
    if lease.incarnation == caller.incarnation && lease.generation == caller.generation {
        LeaseHeld::Held {
            lease: lease.clone(),
        }
    } else {
        LeaseHeld::Stale
    }
}

/// `leases` without those of `session`'s `generation` (its view ended).
pub fn release_generation(leases: &[Lease], session: &str, generation: u64) -> Vec<Lease> {
    leases
        .iter()
        .filter(|lease| lease.session != session || lease.generation != generation)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A location a worker works at.
    fn location(cwd: &str) -> Location {
        Location {
            cwd: cwd.into(),
            env: None,
        }
    }

    /// A caller: one incarnation of one generation of a session's worker.
    fn caller(session: &str, generation: u64, incarnation: &str) -> LeaseCaller {
        LeaseCaller {
            session: session.into(),
            generation,
            incarnation: incarnation.into(),
        }
    }

    /// A place to lease.
    fn place(cwd: &str) -> LeasePlace {
        LeasePlace {
            location: location(cwd),
            project: ProjectSource::None,
        }
    }

    /// A lease of a session's worker.
    fn lease(session: &str, cwd: &str, generation: u64, incarnation: &str) -> Lease {
        Lease {
            session: session.into(),
            location: location(cwd),
            project: ProjectSource::None,
            generation,
            incarnation: incarnation.into(),
        }
    }

    #[test]
    fn a_worker_leases_the_locations_it_works_at() {
        let set = set_locations(
            &[lease("other", "/other", 1, "x")],
            &caller("active", 2, "i"),
            &[place("/work")],
        );
        let LeasesSet::Set { leases } = set else {
            panic!("the leases are set");
        };
        assert_eq!(leases.len(), 2);
        let own = leases
            .iter()
            .find(|lease| lease.session == "active")
            .expect("the caller's lease");
        assert_eq!(own.location, location("/work"));
        assert_eq!(own.generation, 2);
        assert_eq!(own.incarnation, "i");
        // The leases of another session are untouched.
        assert!(leases.iter().any(|lease| lease.session == "other"));
    }

    #[test]
    fn a_newer_generation_refuses_to_set_the_locations() {
        let set = set_locations(
            &[lease("active", "/work", 3, "new")],
            &caller("active", 2, "old"),
            &[place("/work")],
        );
        let LeasesSet::Refused { reason } = set else {
            panic!("the lease is refused");
        };
        assert!(reason.contains("A newer worker"), "{reason}");
    }

    #[test]
    fn an_older_generation_takes_the_session_over() {
        let set = set_locations(
            &[lease("active", "/work", 1, "old")],
            &caller("active", 2, "new"),
            &[place("/work")],
        );
        let LeasesSet::Set { leases } = set else {
            panic!("the leases are set");
        };
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].generation, 2);
        assert_eq!(leases[0].incarnation, "new");
    }

    #[test]
    fn a_location_given_twice_is_leased_once() {
        let set = set_locations(
            &[],
            &caller("active", 1, "i"),
            &[place("/work"), place("/work")],
        );
        let LeasesSet::Set { leases } = set else {
            panic!("the leases are set");
        };
        assert_eq!(leases.len(), 1);
    }

    #[test]
    fn a_call_goes_through_the_lease_of_its_own_incarnation() {
        let leases = vec![lease("active", "/work", 1, "mine")];
        let held = held_lease(&leases, &caller("active", 1, "mine"), &location("/work"));
        assert!(matches!(held, LeaseHeld::Held { .. }));
        let stale = held_lease(&leases, &caller("active", 1, "other"), &location("/work"));
        assert!(matches!(stale, LeaseHeld::Stale));
        let none = held_lease(
            &leases,
            &caller("active", 1, "mine"),
            &location("/elsewhere"),
        );
        assert!(matches!(none, LeaseHeld::None));
    }

    #[test]
    fn the_end_of_a_view_releases_its_generation() {
        let leases = vec![
            lease("active", "/work", 1, "i"),
            lease("active", "/work", 2, "i"),
            lease("other", "/work", 1, "i"),
        ];
        let left = release_generation(&leases, "active", 1);
        assert_eq!(left.len(), 2);
        assert!(
            left.iter()
                .all(|lease| lease.session != "active" || lease.generation != 1)
        );
    }
}
