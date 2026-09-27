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
        self.groups.get(self.membership(key)?.franchise as usize)
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

    /// The seed's era first, then the other eras in their stable order; release order is retained inside
    /// each era. Every member appears exactly once.
    pub fn members_for(&self, seed: Key) -> Option<Vec<&Member>> {
        let membership = self.membership(seed)?;
        let franchise = self.groups.get(membership.franchise as usize)?;
        let mut eras = vec![membership.era];
        eras.extend((0..franchise.eras.len() as u32).filter(|&era| era != membership.era));
        Some(
            eras.into_iter()
                .flat_map(|era| franchise.members.iter().filter(move |member| member.era == era))
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

/// `/index/franchise/<type>/<id>.json`: the seed's primary franchise, grouped seed-era first while
/// retaining release order inside every era. A known title without a curated primary returns `null` and
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
    let members = indexes.franchises.members_for(seed).unwrap_or_default();
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
            franchises.members_for(movie).unwrap().iter().map(|m| m.key).collect::<Vec<_>>(),
            [movie, series]
        );
        assert_eq!(
            franchises.members_for(series).unwrap().iter().map(|m| m.key).collect::<Vec<_>>(),
            [series, movie]
        );
        assert_eq!(franchises.umbrella(movie), None);
        assert_eq!(franchises.umbrella(series).map(|u| u.name.as_str()), Some("Fixture World"));
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
