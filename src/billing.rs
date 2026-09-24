//! Where each title bills the people in its cast, from TMDB's credits (oxyc/den-atlas#82): the sort signal
//! people's prominence weighs a cast credit by, so a lead outranks a bit part on the same titles.
//!
//! Wikidata was measured first and holds too little: a series ordinal (P1545) on the cast of 2.1% of the
//! 2,000 most-voted films and 0.8% of the 500 most-voted series, and the order its statements are listed in
//! puts the lead first on 18 of 23 well-known titles, with Keanu Reeves fifth in *The Matrix*. TMDB's
//! `order` is the billing, so the credits `tmdb.rs` already keeps for the character links are read again
//! here: a plain sort key inside atlas, under the rules at the top of `tmdb.rs`, never in an answer.
//!
//! Only the first `characters::BILLED` positions are kept, and only named roles, so a person the title's
//! kept credits do not name is billed below them (`Billed::Below`). A title with no kept credits, and a
//! person with no TMDB id to look for, have no billing at all: their credits weigh as they did before.

use crate::ratings::Key;
use crate::tmdb::Credits;
use std::collections::HashMap;

/// Where a title bills one of its cast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Billed {
    /// TMDB's billing position, from 0.
    At(u32),
    /// The title is billed, and this person is not among its first `characters::BILLED`.
    Below,
}

/// Each store row's billed people, by entity.
#[derive(Default)]
pub struct Billing {
    /// Row `r`'s billed people are `billed[starts[r]..starts[r + 1]]`; an empty span is a title with no
    /// billing.
    starts: Vec<u32>,
    /// (entity, position), by entity within a row.
    billed: Vec<(u32, u32)>,
    /// Per entity, whether it has a TMDB person id: only then can a billed title fail to name it.
    on_tmdb: Vec<bool>,
}

impl Billing {
    /// Where row `row` bills entity `entity`; `None` when that is not known.
    pub fn of(&self, row: usize, entity: u32) -> Option<Billed> {
        let (&from, &to) = (self.starts.get(row)?, self.starts.get(row + 1)?);
        let billed = &self.billed[from as usize..to as usize];
        if billed.is_empty() || !self.on_tmdb.get(entity as usize).copied().unwrap_or(false) {
            return None;
        }
        Some(match billed.binary_search_by_key(&entity, |&(e, _)| e) {
            Ok(at) => Billed::At(billed[at].1),
            Err(_) => Billed::Below,
        })
    }

    /// How many rows have a billing.
    pub fn titles(&self) -> usize {
        self.starts.windows(2).filter(|w| w[1] > w[0]).count()
    }

    pub(crate) fn bytes(&self) -> usize {
        self.starts.len() * std::mem::size_of::<u32>()
            + self.billed.len() * std::mem::size_of::<(u32, u32)>()
            + self.on_tmdb.len()
    }
}

/// The billing of every store row TMDB's kept credits name, joined onto the store by its `keys` column and
/// onto its people by `ent_tmdb`.
pub fn build(view: &den_store::Store<'_>, credits: &HashMap<Key, Credits>) -> Result<Billing, String> {
    let keys = view.per_row::<u64>("keys").map_err(|e| e.to_string())?;
    let tmdb = view.column::<u32>("ent_tmdb").map_err(|e| e.to_string())?;
    let mut entities: HashMap<u32, Vec<u32>> = HashMap::new();
    for (entity, &person) in (0u32..).zip(tmdb) {
        if person != den_store::NONE_U32 {
            entities.entry(person).or_default().push(entity);
        }
    }
    let mut starts = Vec::with_capacity(keys.len() + 1);
    let mut billed: Vec<(u32, u32)> = Vec::new();
    starts.push(0);
    for &packed in keys {
        let roles = credits.get(&(u8::from(packed >> 32 == 1), packed as u32)).map_or(&[][..], |c| &c.roles);
        let mut row: Vec<(u32, u32)> = roles
            .iter()
            .flat_map(|role| entities.get(&role.person).into_iter().flatten().map(|&e| (e, role.order)))
            .collect();
        // A person with several roles on a title is billed once, at their best position.
        row.sort_unstable();
        row.dedup_by_key(|&mut (entity, _)| entity);
        billed.extend(row);
        starts.push(u32::try_from(billed.len()).map_err(|_| "too many billed credits".to_owned())?);
    }
    Ok(Billing { starts, billed, on_tmdb: tmdb.iter().map(|&p| p != den_store::NONE_U32).collect() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of_rows(rows: &[&[(u32, u32)]], entities: usize) -> Billing {
        let mut billing = Billing { starts: vec![0], on_tmdb: vec![true; entities], ..Billing::default() };
        for row in rows {
            let mut row = row.to_vec();
            row.sort_unstable();
            billing.billed.extend(row);
            billing.starts.push(billing.billed.len() as u32);
        }
        billing
    }

    #[test]
    fn a_billed_title_names_its_cast_and_places_the_rest_below() {
        let billing = of_rows(&[&[(4, 0), (2, 7)], &[]], 5);
        assert_eq!(billing.of(0, 4), Some(Billed::At(0)));
        assert_eq!(billing.of(0, 2), Some(Billed::At(7)));
        assert_eq!(billing.of(0, 3), Some(Billed::Below));
        assert_eq!(billing.of(1, 4), None, "a title with no billing bills no one");
        assert_eq!(billing.of(2, 4), None, "past the end");
        assert_eq!(billing.titles(), 1);
    }

    #[test]
    fn a_person_without_a_tmdb_id_is_not_known_to_be_billed_below() {
        let mut billing = of_rows(&[&[(0, 0)]], 2);
        billing.on_tmdb[1] = false;
        assert_eq!(billing.of(0, 1), None);
    }
}
