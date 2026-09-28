//! Curated primary franchises from the optional den.store.v3 sections.
//!
//! This is deliberately not the raw Wikidata P179 index in `series.rs`: one is a serving contract whose
//! membership and eras were curated by den-dataset, the other is evidence used by the legacy scorer.

use den_index::MediaType;
use den_store::Store;
use std::collections::HashMap;

pub type Key = (MediaType, u32);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Era {
    pub id: String,
    pub name: String,
    pub order: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub key: Key,
    pub era: u32,
    pub order: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Umbrella {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Franchise {
    pub id: String,
    pub name: String,
    pub confidence: u8,
    pub source: String,
    pub eras: Vec<Era>,
    pub members: Vec<Member>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Membership {
    pub franchise: u32,
    pub era: u32,
}

#[derive(Default)]
pub struct Franchises {
    groups: Vec<Franchise>,
    memberships: HashMap<Key, Membership>,
    umbrellas: HashMap<Key, Umbrella>,
}

impl Franchises {
    pub fn from_store(store: &Store<'_>) -> Result<Self, String> {
        let strings = store.strings().map_err(|e| e.to_string())?;
        let keys = store.per_row::<u64>("keys").map_err(|e| e.to_string())?;
        let table = store.curated_franchises().map_err(|e| e.to_string())?;
        let text =
            |id| strings.get(id).map(str::to_owned).ok_or_else(|| format!("bad franchise string {id}"));
        let key_at = |row: den_store::Row| -> Result<Key, String> {
            let packed = *keys.get(row.0).ok_or_else(|| format!("bad franchise member row {}", row.0))?;
            let media = if packed >> 32 == 1 { MediaType::Tv } else { MediaType::Movie };
            Ok((media, packed as u32))
        };
        let mut out = Self::default();
        for (index, raw) in table.iter().enumerate() {
            let eras: Vec<Era> = raw
                .eras()
                .map(|era| Ok(Era { id: text(era.id)?, name: text(era.name)?, order: era.order }))
                .collect::<Result<_, String>>()?;
            let members: Vec<Member> = raw
                .members()
                .map(|member| Ok(Member { key: key_at(member.row)?, era: member.era, order: member.order }))
                .collect::<Result<_, String>>()?;
            let franchise = u32::try_from(index).map_err(|_| "too many curated franchises".to_owned())?;
            for member in &members {
                out.memberships.insert(member.key, Membership { franchise, era: member.era });
            }
            out.groups.push(Franchise {
                id: text(raw.id)?,
                name: text(raw.name)?,
                confidence: raw.confidence,
                source: text(raw.source)?,
                eras,
                members,
            });
        }
        for row in 0..keys.len() {
            let Some(umbrella) = table.umbrella(den_store::Row(row)) else { continue };
            out.umbrellas.insert(
                key_at(den_store::Row(row))?,
                Umbrella { id: text(umbrella.id)?, name: text(umbrella.name)? },
            );
        }
        Ok(out)
    }

    pub fn len(&self) -> usize {
        self.groups.len()
    }

    pub fn membership(&self, key: Key) -> Option<Membership> {
        self.memberships.get(&key).copied()
    }

    pub fn primary(&self, key: Key) -> Option<&Franchise> {
        self.group(self.membership(key)?.franchise)
    }

    /// A franchise by its index, as `Membership::franchise` names it.
    pub fn group(&self, franchise: u32) -> Option<&Franchise> {
        self.groups.get(franchise as usize)
    }

    pub fn umbrella(&self, key: Key) -> Option<&Umbrella> {
        self.umbrellas.get(&key)
    }

    pub fn shares_primary(&self, seed: Key, candidate: Key) -> bool {
        match (self.membership(seed), self.membership(candidate)) {
            (Some(a), Some(b)) => a.franchise == b.franchise,
            _ => false,
        }
    }

    /// The seed's era first, then the others closest to it: eras of the seed's own kind (animated or live
    /// action) before the rest, each nearest the seed's release year first. Inside each era the newest member
    /// comes first — the latest film of a line is the one a viewer is likelier to be looking for — and every
    /// member appears exactly once. Stable order settles ties and titles with no year.
    ///
    /// The stable era order alone led with whatever era came first. Spider-Man (2002) went from its trilogy to
    /// the 1977–79 films before The Amazing Spider-Man; Beck (1997), a series filed in an era of its own, opened
    /// on the 1976–1994 Martin Beck films rather than the 1997–2022 Beck films it runs alongside.
    pub fn members_for(&self, seed: Key, title: impl Fn(Key) -> TitleFacts) -> Option<Vec<&Member>> {
        let membership = self.membership(seed)?;
        let franchise = self.groups.get(membership.franchise as usize)?;
        let own = title(seed);
        let closeness = |era: u32| {
            let facts: Vec<TitleFacts> =
                franchise.members.iter().filter(|member| member.era == era).map(|m| title(m.key)).collect();
            // An era is the seed's kind when most of it is.
            let same_kind = facts.iter().filter(|f| f.animated == own.animated).count() * 2 >= facts.len();
            let years = facts.iter().filter_map(|f| Some((f.year? - own.year?).abs())).min();
            (!same_kind, years.unwrap_or(i64::MAX))
        };
        let mut others: Vec<u32> =
            (0..franchise.eras.len() as u32).filter(|&era| era != membership.era).collect();
        others.sort_by_cached_key(|&era| closeness(era));
        let mut eras = vec![membership.era];
        eras.extend(others);
        Some(
            eras.into_iter()
                .flat_map(|era| franchise.members.iter().filter(move |member| member.era == era).rev())
                .collect(),
        )
    }
}

pub fn metadata_json(franchises: &Franchises, key: Key) -> Option<serde_json::Value> {
    let membership = franchises.membership(key)?;
    let franchise = franchises.groups.get(membership.franchise as usize)?;
    let era = franchise.eras.get(membership.era as usize)?;
    let mut value = serde_json::json!({
        "id": franchise.id,
        "name": franchise.name,
        "confidence": f64::from(franchise.confidence) / 100.0,
        "source": franchise.source,
        "era": { "id": era.id, "name": era.name, "order": era.order },
    });
    if let Some(umbrella) = franchises.umbrella(key) {
        value["umbrella"] = serde_json::json!({ "id": umbrella.id, "name": umbrella.name });
    }
    Some(value)
}

/// What orders a franchise's eras around a seed (`Franchises::members_for`).
#[derive(Clone, Copy, Debug, Default)]
pub struct TitleFacts {
    pub year: Option<i64>,
    pub animated: bool,
}

/// A title's release year, from its card, and whether it is animated.
pub fn title_facts(indexes: &crate::queries::Indexes, key: Key) -> TitleFacts {
    TitleFacts {
        year: indexes.cards.as_ref().and_then(|cards| cards.get(&key)?.year),
        animated: crate::plotrows::genres(indexes, key).contains(&16),
    }
}

/// `/index/franchise/<type>/<id>.json`: the seed's primary franchise, grouped seed-era first, newest first
/// inside every era (`Franchises::members_for`). A known title without a curated primary returns `null` and
/// an empty row, which lets old stores and ungrouped titles use the same client fallback.
pub fn route_json(indexes: &crate::queries::Indexes, seed: Key) -> serde_json::Value {
    let Some(membership) = indexes.franchises.membership(seed) else {
        return serde_json::json!({
            "franchise": null,
            "seed": {
                "type": if seed.0 == MediaType::Tv { "series" } else { "movie" },
                "id": seed.1,
            },
            "members": [],
            "total": 0,
        });
    };
    let Some(franchise) = indexes.franchises.primary(seed) else { unreachable!("membership names a group") };
    let members = indexes.franchises.members_for(seed, |key| title_facts(indexes, key)).unwrap_or_default();
    let titles: Vec<serde_json::Value> = members
        .iter()
        .filter_map(|member| {
            let era = franchise.eras.get(member.era as usize)?;
            let mut title = indexes
                .cards
                .as_ref()
                .and_then(|cards| cards.get(&member.key))
                .map(|card| crate::plotrows::title_json(indexes, member.key, card))
                .unwrap_or_else(|| {
                    serde_json::json!({
                        "type": if member.key.0 == MediaType::Tv { "series" } else { "movie" },
                        "id": member.key.1,
                    })
                });
            title["era"] = serde_json::json!({ "id": era.id, "name": era.name, "order": era.order });
            title["franchiseOrder"] = serde_json::json!(member.order);
            Some(title)
        })
        .collect();
    let total = titles.len();
    serde_json::json!({
        "franchise": {
            "id": franchise.id,
            "name": franchise.name,
            "confidence": f64::from(franchise.confidence) / 100.0,
            "source": franchise.source,
        },
        "seed": {
            "type": if seed.0 == MediaType::Tv { "series" } else { "movie" },
            "id": seed.1,
            "era": {
                "id": franchise.eras[membership.era as usize].id,
                "name": franchise.eras[membership.era as usize].name,
                "order": franchise.eras[membership.era as usize].order,
            },
        },
        "members": titles,
        "total": total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spec_fixture_is_mixed_release_ordered_and_seed_era_first() {
        let Some(path) = crate::store::spec_fixture() else { return };
        let store = crate::store::MappedStore::open(&path).expect("spec fixture");
        let franchises = Franchises::from_store(&store.view()).expect("curated franchise sections");
        let movie = (MediaType::Movie, 1);
        let series = (MediaType::Tv, 10);
        assert_eq!(franchises.len(), 1);
        assert_eq!(franchises.primary(movie).unwrap().name, "Alpha franchise");
        assert_eq!(
            franchises
                .members_for(movie, |_| TitleFacts::default())
                .unwrap()
                .iter()
                .map(|m| m.key)
                .collect::<Vec<_>>(),
            [movie, series]
        );
        assert_eq!(
            franchises
                .members_for(series, |_| TitleFacts::default())
                .unwrap()
                .iter()
                .map(|m| m.key)
                .collect::<Vec<_>>(),
            [series, movie]
        );
        assert_eq!(franchises.umbrella(movie), None);
        assert_eq!(franchises.umbrella(series).map(|u| u.name.as_str()), Some("Fixture World"));
    }

    /// A franchise from `(era, id, year, animated)` rows, eras in the order given, members in release order.
    fn franchise(rows: &[(u32, u32, i64, bool)]) -> (Franchises, HashMap<Key, TitleFacts>) {
        let eras = rows.iter().map(|r| r.0).max().unwrap() + 1;
        let members: Vec<Member> = rows
            .iter()
            .enumerate()
            .map(|(order, &(era, id, _, _))| Member { key: (MediaType::Movie, id), era, order: order as u32 })
            .collect();
        let facts = rows
            .iter()
            .map(|&(_, id, year, animated)| {
                ((MediaType::Movie, id), TitleFacts { year: Some(year), animated })
            })
            .collect();
        let franchises = Franchises {
            memberships: members.iter().map(|m| (m.key, Membership { franchise: 0, era: m.era })).collect(),
            groups: vec![Franchise {
                id: "f".into(),
                name: "F".into(),
                confidence: 90,
                source: "test".into(),
                eras: (0..eras)
                    .map(|order| Era { id: format!("e{order}"), name: String::new(), order })
                    .collect(),
                members,
            }],
            umbrellas: HashMap::new(),
        };
        (franchises, facts)
    }

    fn row(franchises: &Franchises, facts: &HashMap<Key, TitleFacts>, seed: u32) -> Vec<u32> {
        franchises
            .members_for((MediaType::Movie, seed), |key| facts[&key])
            .unwrap()
            .iter()
            .map(|member| member.key.1)
            .collect()
    }

    /// Spider-Man (2002): its trilogy, then the live-action eras nearest it, the 1977–79 films after those, and
    /// the animated Spider-Verse last; newest first inside each era.
    #[test]
    fn eras_follow_the_seed_by_kind_then_by_nearness_in_time() {
        let (franchises, facts) = franchise(&[
            (0, 1977, 1977, false),
            (0, 1979, 1979, false),
            (1, 2002, 2002, false),
            (1, 2004, 2004, false),
            (2, 2012, 2012, false),
            (3, 2021, 2021, false),
            (4, 2023, 2023, true),
        ]);
        assert_eq!(row(&franchises, &facts, 2002), [2004, 2002, 2012, 2021, 1979, 1977, 2023]);
        // From the animated era, the live-action ones follow nearest first.
        assert_eq!(row(&franchises, &facts, 2023), [2023, 2021, 2012, 2004, 2002, 1979, 1977]);
    }

    /// Beck (1997), a series alone in its era: the films it runs alongside lead, not the older ones.
    #[test]
    fn a_seed_alone_in_its_era_leads_with_the_era_it_runs_alongside() {
        let (franchises, facts) = franchise(&[
            (0, 1976, 1976, false),
            (0, 1994, 1994, false),
            (1, 1998, 1998, false),
            (2, 1997, 1997, false),
        ]);
        assert_eq!(row(&franchises, &facts, 1997), [1997, 1998, 1994, 1976]);
    }

    #[test]
    fn an_umbrella_never_makes_two_primary_franchises_the_same() {
        let a = (MediaType::Movie, 1);
        let b = (MediaType::Movie, 2);
        let umbrella = Umbrella { id: "universe".to_owned(), name: "One universe".to_owned() };
        let franchises = Franchises {
            groups: vec![
                Franchise {
                    id: "a".into(),
                    name: "A".into(),
                    confidence: 90,
                    source: "test".into(),
                    eras: vec![],
                    members: vec![],
                },
                Franchise {
                    id: "b".into(),
                    name: "B".into(),
                    confidence: 90,
                    source: "test".into(),
                    eras: vec![],
                    members: vec![],
                },
            ],
            memberships: HashMap::from([
                (a, Membership { franchise: 0, era: 0 }),
                (b, Membership { franchise: 1, era: 0 }),
            ]),
            umbrellas: HashMap::from([(a, umbrella.clone()), (b, umbrella)]),
        };
        assert!(!franchises.shares_primary(a, b), "umbrella labels are display-only");
    }
}
