//! `/index/filter/<movie|series|all>/people.json` and `people/counts.json` (oxyc/den#136): the people credited
//! on the titles a selection matches, filtered by what Wikidata states about them, most prominent first.
//!
//! # Two lists, one grammar
//!
//! The title selection is `sel`, read exactly as `titles.json` reads it, so Search switching from titles to
//! people keeps its `sel` and adds `traits`: `[-]<kind>:<id>` items of the kinds below, normalised, ordered and
//! capped the same way. The traits are a list of their own rather than more kinds in `sel` because they are
//! about a person, not a title: in `sel` a `gender:` would have to mean something to `counts.json` and
//! `titles.json` as well, and there it has no meaning.
//!
//! # What matches
//!
//! A person is credited on a title through its cast or its makers (director, writer, creator). `role` narrows
//! which credits count: `role:cast` counts cast credits alone, two roles (`role:director,role:writer`) count
//! only the titles a person holds both on, a group (`role:director|writer`) those they hold either on, and
//! `-role:cast` only those they are credited on and not cast in (`-role:cast|director`: neither).
//! `credits` is how many matching titles a person's counted credits are on.
//!
//! Every trait takes an OR group as `sel` does: `citizenship:Q30|Q145` is American or British, where
//! `citizenship:Q30,citizenship:Q145` is both. A `born` range stands alone, never in a group.
//!
//! The person traits are stored as Wikidata states them, as the items it names — `gender` (P21) with whatever
//! values it holds, `citizenship` (P27), `occupation` (P106) — and `born` (P569) by decade, or by a range of
//! years (`born:1976-1996`, `born:1976-`, `born:-1996`). Nothing is inferred, and unknown is never a match: a
//! person with no gender on record matches no `gender:`, and no `-gender:` either, since they are not known to
//! lack it — nor any group of values, nor its exclusion. A birth dated only to its century has no decade; a
//! birth dated only to its decade or century is in a range when its whole span is, out of it when none of its
//! span is, and unknown when the span straddles an end. `traitCoverage` says how many of the credited people each applied trait is on record for.
//!
//! # Order
//!
//! `people.json` ranks by `order` (`ORDERS`), `prominence` by default: how popular a person's biggest matching
//! titles are. A title weighs its standing in its own type's popularity order (`Context::within_type`, the
//! share the `all` title order merges the types by), and a person scores their `TOP_TITLES` heaviest. A plain
//! sum would rank on volume — fifteen titles from the middle of the table over five hits — and a sum of
//! squares still does at a ratio of four to one; on the real store both kept prolific supporting players
//! above the stars. The score is a sort key alone, never in the answer: it is read off TMDB's vote counts.
//! Without a popularity order the weights would be TMDB-id order, so prominence falls back to `credits` and
//! says so.
//!
//! A cast credit weighs its title's share by where the title bills the person (oxyc/den-atlas#82): in full
//! among the first `LEADS`, falling off after, and `BIT_PART` below TMDB's first ten — so a few lines in
//! three hits no longer outrank the lead of five. The billing is TMDB's (`billing.rs`), a sort key alone like
//! the vote counts. A credit whose billing is not known — a title with no kept credits, a person with no TMDB
//! id — weighs as it did before, and so does a credit on a title the person also directs, writes or created.
//! The other orders, the counts and `knownFor` read the titles' own weights.

use super::{
    counted_apart, normalise_id, ones, selected_ids, Context, Id, Item, Mode, Request, Scope, Status,
    ENTITY_KINDS, TOP_K,
};
use crate::billing::{Billed, Billing};
use crate::characters::CharacterIndex;
use den_store::{List, PersonDate, PersonTraits, Row};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

const CAST: u8 = 1;
const DIRECTOR: u8 = 2;
const WRITER: u8 = 4;
const CREATOR: u8 = 8;
/// Marks a person as credited on the row being walked by `makers`, which a store without the role lists
/// names its directors, writers and creators in alone: a credit that is not only a cast one.
const MAKER: u8 = 0x20;
/// Marks a person as credited on the row being walked, whatever their roles on it.
const CREDITED: u8 = 0x40;
/// A role no credit carries: asking for one the store cannot answer matches nothing.
const NO_ROLE: u8 = 0x80;
const ROLES: [(&str, u8); 4] =
    [("cast", CAST), ("director", DIRECTOR), ("writer", WRITER), ("creator", CREATOR)];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Trait {
    Gender,
    Born,
    Citizenship,
    Occupation,
    Role,
}

/// One kind `traits` may name.
struct TraitSpec {
    name: &'static str,
    mode: Mode,
    id: Id,
    data: Trait,
    about: &'static str,
}

/// The person kinds first, in the order their bits in `fails` run, then `role`.
const TRAITS: [TraitSpec; 5] = [
    TraitSpec {
        name: "gender",
        mode: Mode::Single,
        id: Id::Qid,
        data: Trait::Gender,
        about: "sex or gender (P21), as the item Wikidata names: any value it holds, not only male and female; \
                a person with none on record matches no gender",
    },
    TraitSpec {
        name: "born",
        mode: Mode::Single,
        id: Id::Decade,
        data: Trait::Born,
        about: "the decade of birth (P569), born:1970 for 1970-1979; a birth dated only to its century has none. \
                Or a range of birth years, both ends inclusive and either left open: born:1976-1996, \
                born:1976-, born:-1996; one range, never in a group, and no other born pick beside it, per \
                request. A birth dated only to its decade or century matches a range when its whole span lies \
                inside it, and is unknown when the span straddles an end. Decades OR: born:1970|1980",
    },
    TraitSpec {
        name: "citizenship",
        mode: Mode::And,
        id: Id::Qid,
        data: Trait::Citizenship,
        about: "a country of citizenship (P27); a person may hold several",
    },
    TraitSpec {
        name: "occupation",
        mode: Mode::And,
        id: Id::Qid,
        data: Trait::Occupation,
        about: "an occupation (P106): actor, film actor, film director, screenwriter, …; a person may hold several",
    },
    TraitSpec {
        name: "role",
        mode: Mode::And,
        id: Id::Lower,
        data: Trait::Role,
        about: "the credit on a matching title: cast, director, writer or creator; two roles count the titles \
                a person holds both on, a group (role:cast|director) those they hold either on, -role:<role> \
                those they are credited on without it",
    },
];

/// `people.json`'s order: `order=<name>`, `prominence` when left out.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(super) enum Order {
    #[default]
    Prominence,
    Credits,
    Name,
    BornAsc,
    BornDesc,
}

const ORDERS: [(&str, Order, &str); 5] = [
    (
        "prominence",
        Order::Prominence,
        "the default: the people whose biggest matching titles are most popular first. Each matching title \
         weighs one minus its rank in its own type's popularity order over that type's size, and a person \
         scores the sum of their 5 heaviest, so a few hits outrank many titles from the middle of the table. \
         A cast credit weighs its title by the person's billing on it: in full among the first 3, less after, a \
         fifth below the tenth, and in full where the billing is not known. \
         Without a popularity order this falls back to credits and names prominence in orderUnavailable",
    ),
    ("credits", Order::Credits, "most matching titles first, then most titles in the corpus"),
    ("name", Order::Name, "by name, folded as search folds names; people with no name last"),
    (
        "born_asc",
        Order::BornAsc,
        "earliest birth first, as Wikidata dates it; people with no birth on record last",
    ),
    ("born_desc", Order::BornDesc, "latest birth first; people with no birth on record last"),
];

impl Order {
    pub(super) fn parse(text: &str) -> Result<Order, String> {
        let text = text.trim().to_ascii_lowercase();
        ORDERS
            .iter()
            .find(|o| o.0 == text)
            .map(|o| o.1)
            .ok_or_else(|| format!("order: {text:?} is not one of {}", ORDERS.map(|o| o.0).join(", ")))
    }

    pub(super) fn name(self) -> &'static str {
        ORDERS.iter().find(|o| o.1 == self).map_or("", |o| o.0)
    }
}

/// The person kinds: the traits a `fails` mask has a bit for.
const PERSON_KINDS: usize = 4;

fn trait_spec(name: &str) -> Option<&'static TraitSpec> {
    TRAITS.iter().find(|t| t.name == name)
}

/// The traits `people/values/<trait>.json` answers for: those whose values are Wikidata items, too many to
/// list whole. A decade or a role is listed whole by `people/counts.json` already.
pub(super) fn values_kind(name: &str) -> Option<&'static str> {
    trait_spec(name).filter(|t| t.id == Id::Qid).map(|t| t.name)
}

/// A trait kind and an id as their canonical pair; an unknown kind is kept, as `sel` keeps one.
pub(super) fn normalise(kind: &str, id: &str, scope: Scope) -> Result<(String, String), String> {
    let kind = kind.trim().to_ascii_lowercase();
    let id = id.trim();
    if kind.is_empty() {
        return Err(format!(":{id}: an empty kind"));
    }
    if id.is_empty() {
        return Err(format!("{kind}: an empty id"));
    }
    let Some(spec) = trait_spec(&kind) else { return Ok((kind, id.to_owned())) };
    if spec.data == Trait::Born && id.contains('-') {
        return Ok((kind, Years::parse(id)?.spelled()));
    }
    let id = normalise_id(&kind, id, spec.id, scope)?;
    Ok((kind, id))
}

/// A request's traits, refused when they cannot be answered as sent: a `born` range beside another positive
/// `born` pick, which could only narrow it — one range says it.
pub(super) fn check(traits: &[Item]) -> Result<(), String> {
    let picks: Vec<&Item> = traits.iter().filter(|i| i.kind == "born" && !i.exclude).collect();
    if picks.len() > 1 && picks.iter().any(|i| i.ids.iter().any(|id| id.contains('-'))) {
        return Err("born: one range per request, and no other born pick beside it".to_owned());
    }
    Ok(())
}

/// Whether a group of these ids is refused as one item: a `born` range stands alone, as it stands alone among
/// the `born` picks (`check`) — one range already says any span of years, and decades OR-ed are `born:1970|1980`.
pub(super) fn one_value(kind: &str, ids: &[String]) -> bool {
    kind == "born" && ids.iter().any(|id| id.contains('-'))
}

/// The earliest birth year a `born` range may name; the latest is next year.
const FIRST_BIRTH_YEAR: i64 = 1800;

/// A `born` range: the birth years it holds, both ends inclusive, either open.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Years {
    from: Option<i64>,
    to: Option<i64>,
}

impl Years {
    /// `<from>-<to>`, either end left out, each a year from `FIRST_BIRTH_YEAR` to next year.
    fn parse(id: &str) -> Result<Years, String> {
        let latest = crate::facts::civil_year(crate::recommend::today() as i64) + 1;
        let (from, to) = id.split_once('-').ok_or_else(|| format!("born: {id:?} is not <year>-<year>"))?;
        let year = |end: &str| -> Result<Option<i64>, String> {
            let end = end.trim();
            if end.is_empty() {
                return Ok(None);
            }
            let year = end
                .parse::<i64>()
                .ok()
                .filter(|_| end.bytes().all(|b| b.is_ascii_digit()))
                .ok_or_else(|| format!("born: {id:?} is not <year>-<year>"))?;
            if !(FIRST_BIRTH_YEAR..=latest).contains(&year) {
                return Err(format!("born: {year} is not a birth year from {FIRST_BIRTH_YEAR} to {latest}"));
            }
            Ok(Some(year))
        };
        match (year(from)?, year(to)?) {
            (None, None) => Err(format!("born: {id:?} names no year")),
            (Some(from), Some(to)) if from > to => Err(format!("born: {id:?} ends before it starts")),
            (from, to) => Ok(Years { from, to }),
        }
    }

    /// As the canonical URL writes it: `1976-1996`, `1976-`, `-1996`.
    fn spelled(self) -> String {
        let end = |year: Option<i64>| year.map_or(String::new(), |y| y.to_string());
        format!("{}-{}", end(self.from), end(self.to))
    }

    /// Whether a birth dated to the years `span` is in the range: `None` when the span straddles an end.
    fn holds(self, (first, last): (i64, i64)) -> Option<bool> {
        let (from, to) = (self.from.unwrap_or(i64::MIN), self.to.unwrap_or(i64::MAX));
        if from <= first && last <= to {
            Some(true)
        } else if last < from || to < first {
            Some(false)
        } else {
            None
        }
    }
}

/// What the people routes read from the store: the traits and the credit lists.
struct Sources<'a> {
    traits: PersonTraits<'a>,
    /// Whether the person kinds answer: the trait sections read (`Ready`), the store has none (`NotOffered`,
    /// a property of the dataset version), or they are there and do not read (`Unavailable`).
    status: Status,
    cast: List<'a, u32>,
    makers: List<'a, u32>,
    directors: List<'a, u32>,
    writers: List<'a, u32>,
    creators: List<'a, u32>,
    /// The roles the credit lists can tell apart.
    roles: u8,
}

/// A person trait the request applies, with the values it names read into the store's terms: entity indexes,
/// decades, or a range of birth years. A value the store does not hold is left out, and an item left with
/// none matches no one.
struct Applied<'r> {
    /// Its kind's position in `TRAITS`.
    kind: usize,
    spec: &'static TraitSpec,
    item: &'r Item,
    targets: Vec<Target>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
    /// An entity index, or a decade.
    Value(i64),
    Years(Years),
}

/// The traits of a request, split.
struct Split<'r> {
    applied: Vec<Applied<'r>>,
    /// Roles a counted credit must hold — one of each item's, and each role item is a mask of the roles it
    /// names, OR-ed — and must not.
    want: Vec<Want>,
    avoid: u8,
    /// Trait kinds named and not applied, and values the kind does not hold.
    ignored: Vec<String>,
    unknown: Vec<String>,
}

/// A positive role item: the roles it names, and whether it is an OR group.
#[derive(Clone, Copy)]
struct Want {
    roles: u8,
    group: bool,
}

impl Split<'_> {
    /// The applied items a person kind's values are counted without (`counted_apart`), as bits over
    /// `applied` — the bits `fails` answers in.
    fn apart(&self, kind: usize) -> u32 {
        self.applied
            .iter()
            .enumerate()
            .filter(|(_, a)| a.kind == kind && counted_apart(a.spec.mode, a.item))
            .fold(0, |apart, (at, _)| apart | 1 << at)
    }

    /// The role masks without the OR groups: what `role` is counted under in `people/counts.json`.
    fn want_apart(&self) -> Vec<u8> {
        self.want.iter().filter(|w| !w.group).map(|w| w.roles).collect()
    }
}

/// Per entity: the matching titles its counted credits are on, and the roles it holds on them.
struct Tally {
    credits: Vec<u32>,
    held: Vec<u8>,
    /// The entities credited at least once, in the order first met.
    touched: Vec<u32>,
    /// Per entity, its position in `touched`, kept with `top`.
    at: Vec<u32>,
    /// Per entity credited, in `touched` order: its `TOP_TITLES` biggest matching titles as (`within_type`
    /// weight, row), largest first; a weight of 0 is an empty place. Empty unless the titles are weighed.
    top: Vec<Top>,
    /// The same, each cast credit weighed by its billing (`billed`): what prominence sums. Empty unless the
    /// titles are weighed by a billing.
    billed: Vec<Top>,
}

/// How many of a person's matching titles their prominence counts: their biggest, so a few hits outrank many
/// titles from the middle of the table (`Order::Prominence`).
const TOP_TITLES: usize = 5;
/// How many of those `people.json` names as what a person is known for.
const KNOWN_FOR: usize = 3;
/// The billing positions a cast credit weighs its title's full share at: the leads.
const LEADS: u32 = 3;
/// What a cast credit weighs, of its title's share, when the title bills the person below its first ten: a
/// bit part, where a lead in a title a fifth as popular weighs the same.
const BIT_PART: f64 = 0.2;

/// The share of its title's weight a cast credit carries at its billing: 1 for the first `LEADS` and for a
/// billing not known, then `LEADS` over the position counted from 1 — ¾ for the fourth, ½ for the sixth,
/// 0.3 for the tenth — and `BIT_PART` below the tenth.
fn billed(billing: Option<Billed>) -> f64 {
    match billing {
        None => 1.0,
        Some(Billed::At(at)) if at < LEADS => 1.0,
        Some(Billed::At(at)) => f64::from(LEADS) / f64::from(at + 1),
        Some(Billed::Below) => BIT_PART,
    }
}

type Top = [(f64, u32); TOP_TITLES];

impl Tally {
    /// A person's biggest matching titles, when the titles were weighed.
    fn top(&self, e: u32) -> Option<&Top> {
        self.top.get(self.at.get(e as usize).map_or(usize::MAX, |&at| at as usize))
    }

    /// A person's prominence: the sum of their biggest matching titles' weights, weighed by their billing
    /// when the titles were; 0 when not weighed.
    fn prominence(&self, e: u32) -> f64 {
        let at = self.at.get(e as usize).map_or(usize::MAX, |&at| at as usize);
        let top = if self.billed.is_empty() { self.top.get(at) } else { self.billed.get(at) };
        top.map_or(0.0, |top| top.iter().map(|t| t.0).sum())
    }
}

/// A title into a person's biggest, if it is one of them; on a tie the one met first stays ahead.
fn keep_top(top: &mut Top, weight: f64, row: u32) {
    if let Some(at) = top.iter().position(|&(w, _)| weight > w) {
        top.copy_within(at..TOP_TITLES - 1, at + 1);
        top[at] = (weight, row);
    }
}

/// The decade a birth falls in, when it is dated finely enough to have one.
fn birth_decade(born: Option<PersonDate>) -> Option<i64> {
    born.filter(|d| d.precision <= 3).map(|d| crate::facts::civil_year(i64::from(d.days)).div_euclid(10) * 10)
}

/// The years a birth may fall in, as far as it is dated: its year, its decade's ten, or its century's hundred.
fn birth_span(born: Option<PersonDate>) -> Option<(i64, i64)> {
    let born = born?;
    let year = crate::facts::civil_year(i64::from(born.days));
    match born.precision {
        0..=2 => Some((year, year)),
        3 => Some((year.div_euclid(10) * 10, year.div_euclid(10) * 10 + 9)),
        // 1901–2000 is the 20th century.
        4 => Some(((year - 1).div_euclid(100) * 100 + 1, (year - 1).div_euclid(100) * 100 + 100)),
        _ => None,
    }
}

/// The century of an astronomical year: 1901–2000 is the 20th, 0 (1 BCE) to -99 the -1st.
fn century(year: i64) -> i64 {
    if year > 0 {
        (year + 99) / 100
    } else {
        -((100 - year) / 100)
    }
}

/// A birth or death as far as Wikidata dates it: a year-precision date is not shown as its 1 January, and a
/// century-precision one has only its century.
fn date_json(date: PersonDate) -> Option<Value> {
    let days = i64::from(date.days);
    let year = crate::facts::civil_year(days);
    let day = || crate::tmdb::civil(days);
    Some(match date.precision {
        0 => json!({ "precision": "day", "date": day(), "year": year }),
        1 => {
            let day = day();
            json!({ "precision": "month", "date": day[..day.len() - 3], "year": year })
        }
        2 => json!({ "precision": "year", "year": year }),
        3 => json!({ "precision": "decade", "year": year.div_euclid(10) * 10 }),
        4 => json!({ "precision": "century", "century": century(year) }),
        _ => return None,
    })
}

impl<'a> Context<'a> {
    fn sources(&self) -> Sources<'a> {
        let view = &self.view;
        let (traits, status) = match view.person_traits() {
            Ok(traits) if view.section("ent_gender_v").is_ok() => (traits, Status::Ready),
            Ok(traits) => (traits, Status::NotOffered),
            Err(e) => {
                eprintln!("people: the person-trait sections do not read: {e}");
                (PersonTraits::default(), Status::Unavailable)
            }
        };
        let (cast, makers) = (view.list::<u32>("cast_v", "cast_o"), view.list::<u32>("makers_v", "makers_o"));
        let mut roles = if cast.is_ok() { CAST } else { 0 };
        // The three role lists come with the store that splits `makers`, and are empty lists without it.
        let split = view.section("directors_v").is_ok();
        let role = |read: Result<List<'a, u32>, den_store::StoreError>, bit: u8, roles: &mut u8| match read {
            Ok(list) => {
                if split {
                    *roles |= bit;
                }
                list
            }
            Err(e) => {
                eprintln!("people: a role list does not read: {e}");
                List::default()
            }
        };
        let directors = role(view.directors(), DIRECTOR, &mut roles);
        let writers = role(view.writers(), WRITER, &mut roles);
        let creators = role(view.creators(), CREATOR, &mut roles);
        Sources {
            traits,
            status,
            cast: cast.unwrap_or_default(),
            makers: makers.unwrap_or_default(),
            directors,
            writers,
            creators,
            roles,
        }
    }

    fn split_traits<'r>(&self, sources: &Sources<'a>, items: &'r [Item]) -> Split<'r> {
        let mut split = Split {
            applied: Vec::new(),
            want: Vec::new(),
            avoid: 0,
            ignored: Vec::new(),
            unknown: Vec::new(),
        };
        for item in items {
            let spec = trait_spec(&item.kind);
            let ready = spec.is_some_and(|s| s.data == Trait::Role || sources.status == Status::Ready);
            let Some(spec) = spec.filter(|_| ready) else {
                if !split.ignored.contains(&item.kind) {
                    split.ignored.push(item.kind.clone());
                }
                continue;
            };
            if spec.data == Trait::Role {
                let mut roles = 0;
                for id in &item.ids {
                    let bit = ROLES.iter().find(|(name, _)| name == id).map(|&(_, bit)| bit);
                    match bit.filter(|&bit| sources.roles & bit != 0) {
                        Some(bit) => roles |= bit,
                        None => split.unknown.push(item.spelled_value(id)),
                    }
                }
                if item.exclude {
                    split.avoid |= roles;
                } else {
                    // A role item naming nothing the credits hold matches no one.
                    let roles = if roles == 0 { NO_ROLE } else { roles };
                    split.want.push(Want { roles, group: item.group() });
                }
                continue;
            }
            let mut targets = Vec::with_capacity(item.ids.len());
            for id in &item.ids {
                let target = match spec.data {
                    Trait::Born if id.contains('-') => Years::parse(id).ok().map(Target::Years),
                    Trait::Born => id.parse::<i64>().ok().map(Target::Value),
                    _ => self.entity_of(id).map(|e| Target::Value(i64::from(e))),
                };
                match target {
                    Some(target) => targets.push(target),
                    None => split.unknown.push(item.spelled_value(id)),
                }
            }
            let kind = TRAITS.iter().position(|t| t.data == spec.data).unwrap_or(0);
            split.applied.push(Applied { kind, spec, item, targets });
        }
        split
    }

    /// Every person credited on the rows of `base`, counting a title for them when their credits on it hold
    /// a role of each mask of `want` and none of `avoid` — and, with `weigh`, keeping their biggest titles
    /// (`top`), and with a `billing` as well their biggest by billing (`billed`).
    fn tally(
        &self,
        sources: &Sources<'a>,
        base: &[u64],
        want: &[u8],
        avoid: u8,
        weigh: bool,
        billing: Option<&Billing>,
    ) -> Tally {
        let billing = billing.filter(|_| weigh);
        let size = self.view.column::<u32>("ent_qid").map_or(0, <[u32]>::len);
        let mut tally = Tally {
            credits: vec![0; size],
            held: vec![0; size],
            touched: Vec::new(),
            at: if weigh { vec![0; size] } else { Vec::new() },
            top: Vec::new(),
            billed: Vec::new(),
        };
        let mut on_row = vec![0u8; size];
        let mut credited: Vec<u32> = Vec::new();
        let lists = [
            (&sources.cast, CAST),
            (&sources.makers, MAKER),
            (&sources.directors, DIRECTOR),
            (&sources.writers, WRITER),
            (&sources.creators, CREATOR),
        ];
        for row in ones(base) {
            let weight = if weigh { self.within_type(row) } else { 0.0 };
            credited.clear();
            for &(list, role) in &lists {
                for &e in list.get(Row(row)) {
                    let Some(roles) = on_row.get_mut(e as usize) else { continue };
                    if *roles == 0 {
                        credited.push(e);
                    }
                    *roles |= role | CREDITED;
                }
            }
            for &e in &credited {
                let e = e as usize;
                let cast_only = on_row[e] & !CREDITED == CAST;
                let roles = on_row[e] & !(CREDITED | MAKER);
                on_row[e] = 0;
                if want.iter().any(|&mask| roles & mask == 0) || roles & avoid != 0 {
                    continue;
                }
                if tally.credits[e] == 0 {
                    if weigh {
                        tally.at[e] = tally.touched.len() as u32;
                        tally.top.push([(0.0, 0); TOP_TITLES]);
                    }
                    if billing.is_some() {
                        tally.billed.push([(0.0, 0); TOP_TITLES]);
                    }
                    tally.touched.push(e as u32);
                }
                tally.credits[e] += 1;
                tally.held[e] |= roles;
                if weigh {
                    let at = tally.at[e] as usize;
                    keep_top(&mut tally.top[at], weight, row as u32);
                    if let Some(billing) = billing {
                        let share = if cast_only { billed(billing.of(row, e as u32)) } else { 1.0 };
                        keep_top(&mut tally.billed[at], weight * share, row as u32);
                    }
                }
            }
        }
        tally
    }

    /// Whether a person holds any of a trait's values: `None` when the trait is not on record for them.
    fn holds(&self, sources: &Sources<'a>, data: Trait, targets: &[Target], e: u32) -> Option<bool> {
        let named = |value: i64| targets.contains(&Target::Value(value));
        let among =
            |values: &[u32]| (!values.is_empty()).then(|| values.iter().any(|&v| named(i64::from(v))));
        match data {
            Trait::Gender => among(sources.traits.genders(e)),
            Trait::Citizenship => among(sources.traits.citizenships(e)),
            Trait::Occupation => among(sources.traits.occupations(e)),
            // A range stands alone in its item (`one_value`).
            Trait::Born => match targets {
                [Target::Years(years)] => birth_span(sources.traits.born(e)).and_then(|s| years.holds(s)),
                _ => birth_decade(sources.traits.born(e)).map(named),
            },
            Trait::Role => None,
        }
    }

    /// The applied items a person does not satisfy, as bits over `applied`. Unknown satisfies nothing: neither
    /// the value nor its exclusion, and neither any value of a group nor the group's exclusion.
    fn fails(&self, sources: &Sources<'a>, applied: &[Applied<'_>], e: u32) -> u32 {
        applied
            .iter()
            .enumerate()
            .filter(|(_, a)| self.holds(sources, a.spec.data, &a.targets, e) != Some(!a.item.exclude))
            .fold(0, |fails, (at, _)| fails | 1 << at)
    }

    /// Whether every applied item of a person kind is on record for a person: known, whether it matches or
    /// not. A kind with no item applied is on record when the person has any value of it.
    fn on_record(&self, sources: &Sources<'a>, applied: &[Applied<'_>], data: Trait, e: u32) -> bool {
        let mut items = applied.iter().filter(|a| a.spec.data == data).peekable();
        if items.peek().is_none() {
            return self.holds(sources, data, &[], e).is_some();
        }
        items.all(|a| self.holds(sources, data, &a.targets, e).is_some())
    }

    /// The title selection, the traits, and who is credited under them; with `weigh`, how prominently.
    fn people_of<'r>(&self, request: &'r Request, weigh: bool, billing: Option<&Billing>) -> People<'a, 'r> {
        let (applied, ignored) = self.split(&request.items);
        // The titles `total` counts: likely matches included, as `titles.json` lists them.
        let base = self.matched(&applied, None).any;
        let sources = self.sources();
        let split = self.split_traits(&sources, &request.traits);
        let want: Vec<u8> = split.want.iter().map(|w| w.roles).collect();
        let tally = self.tally(&sources, &base, &want, split.avoid, weigh, billing);
        let mut envelope = json!({ "coverage": self.coverage(&applied) });
        let mut degraded = self.envelope(&mut envelope, &applied, ignored);
        if !split.ignored.is_empty() {
            envelope["ignoredTraits"] = json!(split.ignored);
        }
        if !split.unknown.is_empty() {
            envelope["unknownTraits"] = json!(split.unknown);
        }
        if sources.status == Status::Unavailable {
            envelope["traitsUnavailable"] =
                json!(TRAITS[..PERSON_KINDS].iter().map(|t| t.name).collect::<Vec<_>>());
            degraded = true;
        }
        People { sources, split, tally, base, envelope, degraded }
    }

    /// `people/counts.json`: for every value of every trait, the people credited under the selection and the
    /// other traits holding it — a one-pick kind (gender, born) counted without its own pick, and a kind with
    /// an OR group (`citizenship:Q30|Q145`, `role:cast|director`) without its groups, as `counts.json` counts
    /// them.
    pub fn people_counts(&self, request: &Request) -> (Value, bool) {
        let people = self.people_of(request, false, None);
        let (sources, split, tally) = (&people.sources, &people.split, &people.tally);
        let mut values: [HashMap<i64, u32>; PERSON_KINDS] = Default::default();
        let mut roles = [0u32; 4];
        let mut known = [0usize; PERSON_KINDS];
        let mut total = 0usize;
        let apart: [u32; PERSON_KINDS] = std::array::from_fn(|i| split.apart(i));
        // A role group's roles are counted over the credits walked without it: a second walk, only then.
        let role_tally = split
            .want
            .iter()
            .any(|w| w.group)
            .then(|| self.tally(sources, &people.base, &split.want_apart(), split.avoid, false, None));
        if let Some(role_tally) = &role_tally {
            for &e in &role_tally.touched {
                if self.fails(sources, &split.applied, e) == 0 {
                    for (count, &(_, bit)) in roles.iter_mut().zip(&ROLES) {
                        if role_tally.held[e as usize] & bit != 0 {
                            *count += 1;
                        }
                    }
                }
            }
        }
        for &e in &tally.touched {
            let fails = self.fails(sources, &split.applied, e);
            if fails == 0 {
                total += 1;
                if role_tally.is_none() {
                    for (count, &(_, bit)) in roles.iter_mut().zip(&ROLES) {
                        if tally.held[e as usize] & bit != 0 {
                            *count += 1;
                        }
                    }
                }
            }
            if sources.status != Status::Ready {
                continue;
            }
            for (i, spec) in TRAITS[..PERSON_KINDS].iter().enumerate() {
                let alone = fails & !apart[i];
                let (entities, decade): (&[u32], Option<i64>) = match spec.data {
                    Trait::Gender => (sources.traits.genders(e), None),
                    Trait::Citizenship => (sources.traits.citizenships(e), None),
                    Trait::Occupation => (sources.traits.occupations(e), None),
                    Trait::Born => (&[], birth_decade(sources.traits.born(e))),
                    Trait::Role => (&[], None),
                };
                if self.on_record(sources, &split.applied, spec.data, e) {
                    known[i] += 1;
                }
                if alone == 0 {
                    for value in entities.iter().map(|&v| i64::from(v)).chain(decade) {
                        *values[i].entry(value).or_default() += 1;
                    }
                }
            }
        }

        let mut kinds = Map::new();
        if sources.status == Status::Ready {
            for (i, spec) in TRAITS[..PERSON_KINDS].iter().enumerate() {
                kinds.insert(spec.name.to_owned(), self.trait_answer(spec, &values[i], &request.traits));
            }
        }
        let role_values: HashMap<i64, u32> = ROLES
            .iter()
            .zip(roles)
            .filter(|(&(_, bit), _)| sources.roles & bit != 0)
            .map(|(&(_, bit), n)| (i64::from(bit), n))
            .collect();
        kinds.insert(
            "role".to_owned(),
            self.trait_answer(&TRAITS[PERSON_KINDS], &role_values, &request.traits),
        );

        let credited = tally.touched.len();
        let coverage: Map<String, Value> = split
            .applied
            .iter()
            .map(|a| (a.spec.name.to_owned(), json!({ "count": known[a.kind], "denominator": credited })))
            .collect();
        let mut answer = people.envelope;
        answer["total"] = json!(total);
        answer["traits"] = Value::Object(kinds);
        answer["traitCoverage"] = Value::Object(coverage);
        (answer, people.degraded)
    }

    /// One trait kind's object in `people/counts.json`, shaped as `counts.json`'s kinds are: an entity kind its
    /// top `TOP_K` values, labelled; a decade or a role every value; every selected id, even at 0.
    fn trait_answer(&self, spec: &TraitSpec, counted: &HashMap<i64, u32>, items: &[Item]) -> Value {
        let selected: Vec<&Item> = items.iter().filter(|i| i.kind == spec.name).collect();
        let id_of = |value: i64| -> String {
            match spec.data {
                Trait::Born => value.to_string(),
                Trait::Role => ROLES.iter().find(|r| i64::from(r.1) == value).map_or("", |r| r.0).to_owned(),
                _ => self.qid(value as u32),
            }
        };
        let mut ranked: Vec<(i64, u32)> =
            counted.iter().map(|(&v, &n)| (v, n)).filter(|&(_, n)| n > 0).collect();
        ranked.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let listed = spec.id != Id::Qid || ranked.len() <= TOP_K;
        let mut values = Map::new();
        let mut labels = Map::new();
        for &(value, n) in ranked.iter().take(if spec.id == Id::Qid { TOP_K } else { usize::MAX }) {
            let id = id_of(value);
            if spec.id == Id::Qid {
                if let Some(label) = self.label(value as u32) {
                    labels.insert(id.clone(), label.into());
                }
            }
            values.insert(id, n.into());
        }
        for id in selected.iter().flat_map(|item| &item.ids) {
            // A born range is no decade value: it is named in `selected` or `excluded` alone.
            if values.contains_key(id) || (spec.data == Trait::Born && id.contains('-')) {
                continue;
            }
            let value = match spec.data {
                Trait::Born => id.parse().ok(),
                Trait::Role => ROLES.iter().find(|r| r.0 == id).map(|r| i64::from(r.1)),
                _ => self.entity_of(id).map(i64::from),
            };
            let n = value.and_then(|v| counted.get(&v)).copied().unwrap_or(0);
            if spec.id == Id::Qid {
                if let Some(label) = self.entity_of(id).and_then(|e| self.label(e)) {
                    labels.insert(id.clone(), label.into());
                }
            }
            values.insert(id.clone(), n.into());
        }
        let mut answer = json!({ "mode": spec.mode.name(), "complete": listed, "values": values });
        if !labels.is_empty() {
            answer["labels"] = Value::Object(labels);
        }
        let (positive, excluded) = selected_ids(&selected);
        if !positive.is_empty() {
            answer["selected"] = json!(positive);
        }
        if !excluded.is_empty() {
            answer["excluded"] = json!(excluded);
        }
        answer
    }

    /// `people/values/<trait>.json`: one entity trait's values, counted as `people/counts.json` counts them —
    /// the people credited under the selection and the other traits holding each, a one-pick trait (gender)
    /// without its own pick and a trait with an OR group without its groups — but every value rather than the
    /// top `TOP_K`, labelled, most people first, then
    /// by name; with `q`, only those with a name or alias having a word starting `q`, the value it names exactly
    /// first, then those holding it as whole words, then the rest (`match_tier`). So a value past the top
    /// (citizenship:Iceland among the films of the 2020s) can be found by name.
    pub fn people_values(&self, kind: &str, request: &Request) -> (Value, bool) {
        let people = self.people_of(request, false, None);
        let (sources, split, tally) = (&people.sources, &people.split, &people.tally);
        let i = TRAITS.iter().position(|t| t.name == kind).unwrap_or(0);
        let (spec, apart) = (&TRAITS[i], split.apart(i));
        // (entity, match tier), by entity.
        let named: Option<Vec<(u32, u8)>> =
            request.q.as_deref().map(|q| self.filter.names(self.indexes).matching(q));
        let tier = |v: u32| match &named {
            Some(named) => named.binary_search_by_key(&v, |&(e, _)| e).ok().map(|at| named[at].1),
            None => Some(0),
        };
        let mut counted: HashMap<u32, u32> = HashMap::new();
        let mut denominator = 0usize;
        if sources.status == Status::Ready {
            for &e in &tally.touched {
                if self.fails(sources, &split.applied, e) & !apart != 0 {
                    continue;
                }
                denominator += 1;
                let values = match spec.data {
                    Trait::Gender => sources.traits.genders(e),
                    Trait::Citizenship => sources.traits.citizenships(e),
                    Trait::Occupation => sources.traits.occupations(e),
                    Trait::Born | Trait::Role => &[],
                };
                for &v in values {
                    if tier(v).is_some() {
                        *counted.entry(v).or_default() += 1;
                    }
                }
            }
        }
        // (match tier, id, name, people): the value `q` names first, as `values/<kind>.json` orders them.
        let mut found: Vec<(u8, String, String, u32)> = counted
            .into_iter()
            .map(|(v, n)| {
                (tier(v).unwrap_or(0), self.qid(v), self.label(v).unwrap_or_default().to_owned(), n)
            })
            .collect();
        found.sort_by(|a, b| {
            a.0.cmp(&b.0).then(b.3.cmp(&a.3)).then_with(|| a.2.cmp(&b.2)).then_with(|| a.1.cmp(&b.1))
        });
        let complete = found.len() <= request.limit;
        let values: Vec<Value> = found
            .into_iter()
            .take(request.limit)
            .map(|(_, id, name, count)| json!({ "id": id, "name": name, "count": count }))
            .collect();
        let mut answer = people.envelope;
        answer["kind"] = json!(spec.name);
        answer["mode"] = json!(spec.mode.name());
        answer["values"] = json!(values);
        answer["complete"] = json!(complete);
        answer["denominator"] = json!(denominator);
        (answer, people.degraded)
    }

    /// `people.json`: the people credited on the matching titles and holding every trait, in the request's
    /// `order` (`ORDERS`); paged as `titles.json` is. Ties fall to the `credits` order — most matching titles,
    /// then most titles in the whole corpus — then the Q-id; `name` ties go straight to the Q-id. `order` in
    /// the answer names the order used: `prominence` needs a popularity order, and without one the answer is
    /// in `credits` and names the order it could not use in `orderUnavailable`. With a popularity order each
    /// person carries `knownFor`, their biggest matching titles; without one it is left out rather than chosen
    /// by TMDB id.
    pub fn people(&self, request: &Request) -> (Value, bool) {
        let order = match request.order {
            Order::Prominence if !self.popular() => Order::Credits,
            order => order,
        };
        // Weighed whenever there is a popularity order, for `knownFor` whatever the order; by billing for
        // prominence alone.
        let billing =
            self.characters.as_deref().map(CharacterIndex::billing).filter(|_| order == Order::Prominence);
        let people = self.people_of(request, self.popular(), billing);
        let (sources, split, tally) = (&people.sources, &people.split, &people.tally);
        let corpus = ENTITY_KINDS
            .iter()
            .position(|k| k.name == "person")
            .and_then(|i| self.filter.entities[i].as_ref());
        let qids = self.view.column::<u32>("ent_qid").unwrap_or(&[]);
        let mut ranked: Vec<Ranked> = tally
            .touched
            .iter()
            .filter(|&&e| self.fails(sources, &split.applied, e) == 0)
            .map(|&e| Ranked {
                e,
                credits: tally.credits[e as usize],
                titles: corpus.map_or(0, |k| k.titles(e)),
                qid: qids.get(e as usize).copied().unwrap_or(0),
                prominence: tally.prominence(e),
                born: match order {
                    Order::BornAsc | Order::BornDesc => sources.traits.born(e).map(|d| d.days),
                    _ => None,
                },
                name: match order {
                    Order::Name => self.label(e).map(crate::facts::name_key),
                    _ => None,
                },
            })
            .collect();
        let total = ranked.len();
        let compare = |a: &Ranked, b: &Ranked| {
            let tie = || b.credits.cmp(&a.credits).then(b.titles.cmp(&a.titles)).then(a.qid.cmp(&b.qid));
            // Nothing on record sorts last, whichever way the order runs.
            let last = |a: bool, b: bool| a.cmp(&b);
            match order {
                Order::Prominence => b.prominence.total_cmp(&a.prominence).then_with(tie),
                Order::Credits => tie(),
                Order::Name => last(a.name.is_none(), b.name.is_none())
                    .then_with(|| a.name.cmp(&b.name))
                    .then(a.qid.cmp(&b.qid)),
                Order::BornAsc => {
                    last(a.born.is_none(), b.born.is_none()).then(a.born.cmp(&b.born)).then_with(tie)
                }
                Order::BornDesc => {
                    last(a.born.is_none(), b.born.is_none()).then(b.born.cmp(&a.born)).then_with(tie)
                }
            }
        };
        let end = request.skip.saturating_add(request.limit);
        if end < ranked.len() {
            ranked.select_nth_unstable_by(end, compare);
            ranked.truncate(end);
        }
        ranked.sort_unstable_by(compare);
        let mut labels = Map::new();
        let page: Vec<Value> = ranked
            .iter()
            .skip(request.skip)
            .map(|r| {
                let mut person =
                    self.person_json(sources, r.e, r.credits, tally.held[r.e as usize], &mut labels);
                if let Some(top) = tally.top(r.e) {
                    person["knownFor"] = self.known_for(top);
                }
                person
            })
            .collect();
        let mut answer = people.envelope;
        let mut degraded = people.degraded;
        answer["order"] = json!(order.name());
        if order != request.order {
            answer["orderUnavailable"] = json!(request.order.name());
            // A ratings provider that has not filled in yet is a passing failure; a deployment without one
            // answers this way for good.
            degraded |= self.indexes.ratings.is_some();
        }
        answer["people"] = json!(page);
        answer["total"] = json!(total);
        answer["labels"] = Value::Object(labels);
        (answer, degraded)
    }

    /// What a person is known for: their `KNOWN_FOR` biggest matching titles, biggest first, as their cards
    /// name them — every matching title has a card, so none is named by TMDB.
    fn known_for(&self, top: &Top) -> Value {
        let cards = self.indexes.cards.as_ref();
        let titles: Vec<Value> = top
            .iter()
            .filter(|&&(weight, _)| weight > 0.0)
            .take(KNOWN_FOR)
            .filter_map(|&(_, row)| {
                let key = self.filter.keys[row as usize];
                let card = cards?.get(&key)?;
                let kind = if key.0 == den_index::MediaType::Tv { "series" } else { "movie" };
                Some(json!({ "type": kind, "id": key.1, "title": card.title, "year": card.year }))
            })
            .collect();
        json!(titles)
    }

    /// One person as `people.json` lists them; the trait ids they carry are labelled in `labels`.
    fn person_json(
        &self,
        sources: &Sources<'a>,
        e: u32,
        credits: u32,
        held: u8,
        labels: &mut Map<String, Value>,
    ) -> Value {
        let roles: Vec<&str> = ROLES.iter().filter(|r| held & r.1 != 0).map(|r| r.0).collect();
        let mut person =
            json!({ "id": self.qid(e), "name": self.label(e), "credits": credits, "roles": roles });
        if let Some(tmdb) = self.tmdb(e) {
            person["tmdbId"] = json!(tmdb);
        }
        if sources.status != Status::Ready {
            return person;
        }
        for (field, values) in [
            ("gender", sources.traits.genders(e)),
            ("citizenship", sources.traits.citizenships(e)),
            ("occupation", sources.traits.occupations(e)),
        ] {
            if values.is_empty() {
                continue;
            }
            let ids: Vec<String> = values
                .iter()
                .map(|&v| {
                    let id = self.qid(v);
                    if let Some(label) = self.label(v) {
                        labels.insert(id.clone(), label.into());
                    }
                    id
                })
                .collect();
            person[field] = json!(ids);
        }
        for (field, date) in [("born", sources.traits.born(e)), ("died", sources.traits.died(e))] {
            if let Some(date) = date.and_then(date_json) {
                person[field] = date;
            }
        }
        person
    }
}

/// A person as `people.json` ranks them: what each order reads.
struct Ranked {
    e: u32,
    credits: u32,
    /// Titles crediting them in the whole corpus.
    titles: usize,
    qid: u32,
    prominence: f64,
    /// Read for the birth orders alone, as for the name order `name`.
    born: Option<i32>,
    name: Option<String>,
}

/// A people request worked out: who is credited, and the envelope every answer carries.
struct People<'a, 'r> {
    sources: Sources<'a>,
    split: Split<'r>,
    tally: Tally,
    /// The matching titles the credits were walked on.
    base: Vec<u64>,
    envelope: Value,
    degraded: bool,
}

/// The trait kinds, as `/index/schema.json`'s `filter.traits` describes them.
pub(super) fn schema() -> Value {
    let kinds: Map<String, Value> = TRAITS
        .iter()
        .map(|t| {
            let listing = if t.id == Id::Qid { "top" } else { "full" };
            let mut about =
                json!({ "mode": t.mode.name(), "id": t.id.format(), "listing": listing, "about": t.about });
            if t.id == Id::Qid {
                about["top"] = json!(TOP_K);
            }
            if t.data == Trait::Role {
                about["values"] = json!(ROLES.iter().map(|r| r.0).collect::<Vec<_>>());
            }
            if t.data == Trait::Born {
                about["range"] = json!({
                    "id": "<from>-<to>: birth years, both inclusive, either end left out (1976-1996, 1976-, \
                           -1996)",
                    "years": format!("{FIRST_BIRTH_YEAR} to next year"),
                    "counted": "people/counts.json counts born by decade whatever is picked; a range is \
                                named in selected or excluded",
                });
            }
            (t.name.to_owned(), about)
        })
        .collect();
    let orders: Map<String, Value> = ORDERS.iter().map(|o| (o.0.to_owned(), json!(o.2))).collect();
    json!({
        "kinds": kinds,
        "orders": orders,
        "defaultOrder": Order::default().name(),
        "orderTies": "most matching titles, then most titles in the corpus, then Q-id; under name, the Q-id",
        "about": "people.json and people/counts.json take the title selection as sel, and the person traits as \
                  traits in the same [-]<kind>:<id> grammar and canonical order, OR groups included \
                  (citizenship:Q30|Q145 is American or British; filter.or). A person matches a trait only \
                  when it is on record for them: unknown matches neither a value nor its exclusion, nor a \
                  group nor its exclusion. Gender, citizenship and occupation are the Wikidata items the store \
                  names, labelled in labels. A value a trait does not hold is named in unknownTraits as \
                  [-]<kind>:<id>, a group's one by one. people/counts.json and people/values/<trait>.json count \
                  a trait with a positive group without its groups, gender and born without any of their \
                  items, and every other trait under them; role:cast|director counts role over the credits \
                  walked without the group",
    })
}

#[cfg(test)]
mod tests {
    use super::super::{Route, Scope};
    use super::*;
    use crate::queries::Indexes;
    use crate::store::fixture::{Entity, Title};
    use den_index::MediaType::Movie;

    const FEMALE: u32 = 200;
    const MALE: u32 = 201;
    const NON_BINARY: u32 = 202;
    const SWEDEN: u32 = 300;
    const US: u32 = 301;
    const ACTOR: u32 = 400;
    const DIRECTOR_JOB: u32 = 401;
    const SCREENWRITER: u32 = 402;

    fn days(year: i64, month: u32, day: u32) -> i32 {
        crate::facts::days_from_civil(year, month, day) as i32
    }

    /// Five people over three films and a series:
    ///
    /// - Ann (101): female, Swedish, an actor, born 29 July 1974. Cast in films 1 (2020) and 3 (1990).
    /// - Bob (102): male, American and Swedish, actor and film director, born in 1980 (year precision). Cast in
    ///   films 1 and 2 (both 2020) and series 4 (2020), and directs film 2.
    /// - Cid (103): non-binary, director and screenwriter, born in the 20th century (century precision), died
    ///   2 March 2019. Directs and writes film 1, directs film 3, creates series 4.
    /// - Dee (104): nothing on record. Cast in film 1.
    /// - Eve (105): male, an actor, born in the 1970s (decade precision). Cast in film 2.
    fn people_store(name: &str) -> Indexes {
        let title = |media, tmdb_id, year, votes| Title {
            media,
            tmdb_id,
            primary_genre: "Drama",
            plot: vec![100, 0, 0],
            premise: vec![100, 0, 0],
            card: Some(("A title", None, Some(year))),
            votes,
            ..Title::default()
        };
        let titles = [
            Title {
                cast: vec![101, 102, 104],
                directors: vec![103],
                writers: vec![103],
                makers: vec![103],
                ..title(0, 1, 2020, 300)
            },
            Title { cast: vec![102, 105], directors: vec![102], makers: vec![102], ..title(0, 2, 2020, 200) },
            Title { cast: vec![101], directors: vec![103], makers: vec![103], ..title(0, 3, 1990, 100) },
            Title { cast: vec![102], creators: vec![103], makers: vec![103], ..title(1, 4, 2020, 50) },
        ];
        let label = |qid, name| Entity { qid, name, ..Entity::default() };
        let entities = [
            Entity {
                qid: 101,
                name: "Ann",
                tmdb: Some(5001),
                genders: vec![FEMALE],
                citizenships: vec![SWEDEN],
                occupations: vec![ACTOR],
                born: Some((days(1974, 7, 29), 0)),
                ..Entity::default()
            },
            Entity {
                qid: 102,
                name: "Bob",
                genders: vec![MALE],
                citizenships: vec![US, SWEDEN],
                occupations: vec![ACTOR, DIRECTOR_JOB],
                born: Some((days(1980, 1, 1), 2)),
                ..Entity::default()
            },
            Entity {
                qid: 103,
                name: "Cid",
                genders: vec![NON_BINARY],
                occupations: vec![DIRECTOR_JOB, SCREENWRITER],
                born: Some((days(1950, 1, 1), 4)),
                died: Some((days(2019, 3, 2), 0)),
                ..Entity::default()
            },
            label(104, "Dee"),
            Entity {
                qid: 105,
                name: "Eve",
                genders: vec![MALE],
                occupations: vec![ACTOR],
                born: Some((days(1970, 1, 1), 3)),
                ..Entity::default()
            },
            label(FEMALE, "female"),
            label(MALE, "male"),
            label(NON_BINARY, "non-binary"),
            Entity { qid: SWEDEN, name: "Sweden", aliases: vec!["Kingdom of Sweden"], ..Entity::default() },
            label(US, "United States"),
            label(ACTOR, "actor"),
            label(DIRECTOR_JOB, "film director"),
            label(SCREENWRITER, "screenwriter"),
        ];
        load(name, &titles, &entities)
    }

    fn load(name: &str, titles: &[Title], entities: &[Entity]) -> Indexes {
        let dir = std::env::temp_dir().join(format!("den-atlas-people-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::store::fixture::write(&dir.join("den-v1.store"), "v1", 3, titles, entities);
        let meta = json!({ "datasetVersion": "v1", "taxonomyVersion": "t02", "embeddingModel": "m", "dims": 3,
                           "quantization": "int8", "storeFile": "den-v1.store" });
        std::fs::write(dir.join("dataset.meta.json"), meta.to_string()).unwrap();
        let ds = crate::dataset::Dataset::load(&dir).expect("the people store loads");
        crate::queries::load_for_tools(&ds).expect("its indexes load")
    }

    fn ask(indexes: &Indexes, scope: impl Into<Scope>, route: Route, query: &str) -> Value {
        let scope = scope.into();
        let request = Request::parse(route, scope, query).unwrap_or_else(|e| panic!("{query}: {e}"));
        let context = Context::new(indexes, scope, None);
        match route {
            Route::People => context.people(&request).0,
            _ => context.people_counts(&request).0,
        }
    }

    fn names(answer: &Value) -> Vec<(String, u64)> {
        answer["people"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p["name"].as_str().unwrap().to_owned(), p["credits"].as_u64().unwrap()))
            .collect()
    }

    fn named(pairs: &[(&str, u64)]) -> Vec<(String, u64)> {
        pairs.iter().map(|&(n, c)| (n.to_owned(), c)).collect()
    }

    /// The issue's example: male actors in 2020 films, most films first.
    #[test]
    fn male_actors_in_2020_films() {
        let indexes = people_store("male-actors");
        let answer = ask(&indexes, Movie, Route::People, "sel=decade:2020&traits=gender:Q201,role:cast");
        assert_eq!(names(&answer), named(&[("Bob", 2), ("Eve", 1)]), "{answer}");
        assert_eq!(answer["total"], 2);
        let bob = &answer["people"][0];
        assert_eq!(bob["id"], "Q102");
        assert_eq!(bob["gender"], json!(["Q201"]));
        assert_eq!(
            bob["roles"],
            json!(["cast", "director"]),
            "film 2 is a cast credit and he directs it too"
        );
        assert_eq!(bob["born"], json!({ "precision": "year", "year": 1980 }), "not 1 January");
        assert_eq!(answer["labels"]["Q201"], "male");
        assert_eq!(answer["labels"]["Q401"], "film director");
        let by_job =
            ask(&indexes, Movie, Route::People, "sel=decade:2020&traits=gender:Q201,occupation:Q400");
        assert_eq!(names(&by_job), named(&[("Bob", 2), ("Eve", 1)]), "the occupation reads the same here");
    }

    /// Unknown is never a match: Dee has no gender on record, so she is neither male nor known not to be.
    #[test]
    fn unknown_matches_neither_a_value_nor_its_exclusion() {
        let indexes = people_store("unknown");
        let everyone = ask(&indexes, Movie, Route::People, "sel=decade:2020");
        assert_eq!(everyone["total"], 5, "Ann, Bob, Cid, Dee and Eve are credited on the 2020 films");
        let not_male = ask(&indexes, Movie, Route::People, "sel=decade:2020&traits=-gender:Q201");
        let mut found: Vec<String> = names(&not_male).into_iter().map(|(n, _)| n).collect();
        found.sort();
        assert_eq!(found, vec!["Ann", "Cid"], "Dee is not known to be anything");
        let counts = ask(&indexes, Movie, Route::PeopleCounts, "sel=decade:2020&traits=gender:Q201");
        assert_eq!(counts["traitCoverage"]["gender"], json!({ "count": 4, "denominator": 5 }));
    }

    /// Gender lists whatever values the data holds, labelled from the store's entities, and counts as one
    /// pick: selecting male leaves the other values readable as alternatives.
    #[test]
    fn gender_counts_every_value_the_data_holds() {
        let indexes = people_store("gender-counts");
        let counts = ask(&indexes, Movie, Route::PeopleCounts, "sel=decade:2020");
        let gender = &counts["traits"]["gender"];
        assert_eq!(gender["values"], json!({ "Q200": 1, "Q201": 2, "Q202": 1 }));
        assert_eq!(gender["labels"], json!({ "Q200": "female", "Q201": "male", "Q202": "non-binary" }));
        assert_eq!(gender["mode"], "single");
        assert_eq!(counts["total"], 5);
        let male = ask(&indexes, Movie, Route::PeopleCounts, "sel=decade:2020&traits=gender:Q201");
        assert_eq!(male["total"], 2);
        assert_eq!(male["traits"]["gender"]["values"], gender["values"], "counted without its own pick");
        assert_eq!(male["traits"]["gender"]["selected"], json!(["Q201"]));
        assert_eq!(male["traits"]["occupation"]["values"], json!({ "Q400": 2, "Q401": 1 }), "under the pick");
        assert_eq!(male["traits"]["born"]["values"], json!({ "1970": 1, "1980": 1 }));
        assert_eq!(male["traits"]["role"]["values"], json!({ "cast": 2, "director": 1 }));
    }

    /// Births by decade: a day, a year and a decade each have one; a century does not.
    #[test]
    fn born_reads_a_decade_only_from_a_fine_enough_date() {
        let indexes = people_store("born");
        let counts = ask(&indexes, Scope::All, Route::PeopleCounts, "");
        assert_eq!(
            counts["traits"]["born"]["values"],
            json!({ "1970": 2, "1980": 1 }),
            "Cid's century has none"
        );
        let seventies = ask(&indexes, Scope::All, Route::People, "traits=born:1975");
        let mut found: Vec<String> = names(&seventies).into_iter().map(|(n, _)| n).collect();
        found.sort();
        assert_eq!(found, vec!["Ann", "Eve"]);
        let everyone = ask(&indexes, Scope::All, Route::People, "");
        let person =
            |name: &str| everyone["people"].as_array().unwrap().iter().find(|p| p["name"] == name).cloned();
        let ann = person("Ann").unwrap();
        assert_eq!(ann["born"], json!({ "precision": "day", "date": "1974-07-29", "year": 1974 }));
        assert_eq!(ann["tmdbId"], 5001);
        assert_eq!(ann["citizenship"], json!(["Q300"]));
        let cid = person("Cid").unwrap();
        assert_eq!(cid["born"], json!({ "precision": "century", "century": 20 }));
        assert_eq!(cid["died"], json!({ "precision": "day", "date": "2019-03-02", "year": 2019 }));
        let dee = person("Dee").unwrap();
        assert!(dee.get("gender").is_none() && dee.get("born").is_none(), "nothing on record, nothing said");
        assert_eq!(dee["tmdbId"], Value::Null);
    }

    /// Roles are read per title: two roles count the titles a person holds both on, and an excluded role the
    /// titles they are credited on without it. Series count under `all`.
    #[test]
    fn roles_are_held_per_title() {
        let indexes = people_store("roles");
        let both = ask(&indexes, Scope::All, Route::People, "traits=role:director,role:writer");
        assert_eq!(names(&both), named(&[("Cid", 1)]), "Bob directs film 2 but writes nothing");
        let crew = ask(&indexes, Movie, Route::People, "sel=decade:2020&traits=-role:cast");
        assert_eq!(names(&crew), named(&[("Cid", 1)]), "Bob directs film 2 and is in its cast");
        let creators = ask(&indexes, Scope::All, Route::People, "traits=role:creator");
        assert_eq!(names(&creators), named(&[("Cid", 1)]));
        let films = ask(&indexes, Movie, Route::People, "traits=role:creator");
        assert_eq!(films["total"], 0, "no film has a creator");
        let grip = ask(&indexes, Movie, Route::People, "traits=role:grip");
        assert_eq!((&grip["total"], &grip["unknownTraits"]), (&0.into(), &json!(["role:grip"])));
    }

    /// `credits`: ranked by matching titles, then titles in the whole corpus, then Q-id; paged as titles.json is.
    #[test]
    fn people_are_ranked_and_paged() {
        let indexes = people_store("paged");
        let all = ask(&indexes, Scope::All, Route::People, "order=credits");
        assert_eq!(
            names(&all),
            named(&[("Bob", 3), ("Cid", 3), ("Ann", 2), ("Dee", 1), ("Eve", 1)]),
            "Bob and Cid tie on three titles each and on the corpus, so the Q-id decides"
        );
        let mut paged = Vec::new();
        for skip in 0..5 {
            let page =
                ask(&indexes, Scope::All, Route::People, &format!("order=credits&skip={skip}&limit=1"));
            assert_eq!(page["total"], 5);
            paged.extend(names(&page));
        }
        assert_eq!(paged, names(&all));
        let both = ask(&indexes, Scope::All, Route::People, "traits=citizenship:Q300,citizenship:Q301");
        assert_eq!(names(&both), named(&[("Bob", 3)]), "citizenship holds several at once");
    }

    fn sorted_names(answer: &Value) -> Vec<String> {
        let mut found: Vec<String> = names(answer).into_iter().map(|(n, _)| n).collect();
        found.sort();
        found
    }

    /// `|` OR-s a trait's values: Swedish or American is Ann and Bob, where Swedish and American is Bob alone.
    /// Unknown is still never a match: Cid, Dee and Eve have no citizenship on record, so they are neither
    /// Swedish or American nor known to be neither.
    #[test]
    fn a_trait_group_is_the_union_and_unknown_matches_neither_side() {
        let indexes = people_store("or-citizenship");
        let either = ask(&indexes, Scope::All, Route::People, "traits=citizenship:Q300|Q301");
        assert_eq!(sorted_names(&either), vec!["Ann", "Bob"]);
        let both = ask(&indexes, Scope::All, Route::People, "traits=citizenship:Q300,citizenship:Q301");
        assert_eq!(sorted_names(&both), vec!["Bob"]);
        let neither = ask(&indexes, Scope::All, Route::People, "traits=-citizenship:Q300|Q301");
        assert_eq!(neither["total"], 0, "no one on record holds another citizenship: {neither}");
        let genders = ask(&indexes, Scope::All, Route::People, "traits=gender:Q200|Q202");
        assert_eq!(sorted_names(&genders), vec!["Ann", "Cid"]);
        let other = ask(&indexes, Scope::All, Route::People, "traits=-gender:Q200|Q202");
        assert_eq!(sorted_names(&other), vec!["Bob", "Eve"], "Dee has no gender on record");
        let decades = ask(&indexes, Scope::All, Route::People, "traits=born:1970|1980");
        assert_eq!(sorted_names(&decades), vec!["Ann", "Bob", "Eve"], "Cid's century has no decade");
        let unknown = ask(&indexes, Scope::All, Route::People, "traits=citizenship:Q300|Q999999");
        assert_eq!(sorted_names(&unknown), vec!["Ann", "Bob"]);
        assert_eq!(unknown["unknownTraits"], json!(["citizenship:Q999999"]));
    }

    /// `role:cast|director` counts a title credited as either; `-role:cast|writer` one credited as neither.
    #[test]
    fn a_role_group_counts_titles_held_in_either_role() {
        let indexes = people_store("or-roles");
        let either = ask(&indexes, Movie, Route::People, "traits=role:cast|director&order=credits");
        assert_eq!(
            names(&either),
            named(&[("Bob", 2), ("Cid", 2), ("Ann", 2), ("Dee", 1), ("Eve", 1)]),
            "Cid directs films 1 and 3"
        );
        let both = ask(&indexes, Movie, Route::People, "traits=role:cast,role:director");
        assert_eq!(names(&both), named(&[("Bob", 1)]), "film 2 alone holds both");
        let neither = ask(&indexes, Movie, Route::People, "traits=-role:cast|writer");
        assert_eq!(names(&neither), named(&[("Cid", 1)]), "Cid directs film 3 and writes only film 1");
        let grip = ask(&indexes, Movie, Route::People, "traits=role:grip|writer");
        assert_eq!((&grip["total"], &grip["unknownTraits"]), (&1.into(), &json!(["role:grip"])));
    }

    /// people/counts.json counts a trait with an OR group without the group — what each value would add — and
    /// every other trait under it; a role group the same, over the credits walked without it.
    #[test]
    fn a_trait_with_a_group_counts_its_values_without_the_group() {
        let indexes = people_store("or-counts");
        let either =
            ask(&indexes, Movie, Route::PeopleCounts, "sel=decade:2020&traits=citizenship:Q300|Q301");
        assert_eq!(either["total"], 2);
        let citizenship = &either["traits"]["citizenship"];
        assert_eq!(
            citizenship["values"],
            json!({ "Q300": 2, "Q301": 1 }),
            "Ann and Bob Swedish, Bob American"
        );
        assert_eq!(citizenship["selected"], json!(["Q300", "Q301"]));
        assert_eq!(either["traits"]["gender"]["values"], json!({ "Q200": 1, "Q201": 1 }), "under the group");
        let both = ask(
            &indexes,
            Movie,
            Route::PeopleCounts,
            "sel=decade:2020&traits=citizenship:Q300,citizenship:Q301",
        );
        assert_eq!(both["traits"]["citizenship"]["values"], json!({ "Q300": 1, "Q301": 1 }), "Bob alone");
        let makers = ask(&indexes, Movie, Route::PeopleCounts, "sel=decade:2020&traits=occupation:Q401|Q402");
        assert_eq!(makers["total"], 2, "Bob and Cid");
        assert_eq!(
            makers["traits"]["occupation"]["values"],
            json!({ "Q400": 3, "Q401": 2, "Q402": 1 }),
            "Ann, Bob and Eve are actors, whatever the group"
        );

        let crew = ask(&indexes, Movie, Route::PeopleCounts, "sel=decade:2020&traits=role:director|writer");
        assert_eq!(crew["total"], 2, "Bob directs film 2, Cid directs and writes film 1");
        assert_eq!(
            crew["traits"]["role"]["values"],
            json!({ "cast": 4, "director": 2, "writer": 1 }),
            "Ann, Bob, Dee and Eve are cast in the 2020 films, whatever the group"
        );
        assert_eq!(crew["traits"]["role"]["selected"], json!(["director", "writer"]));
        assert_eq!(crew["traits"]["gender"]["values"], json!({ "Q201": 1, "Q202": 1 }), "under the group");
    }

    /// Twenty films, film i (TMDB id i + 1) at rank i in the popularity order when `popular`, so it weighs
    /// 1 − i/20; without, every vote count is 0 and the order is by TMDB id alone. Synthetic numbers.
    ///
    /// - Zed (501): cast in films 0–4, the five biggest (weights 1 … 0.8: top five 4.5, sum 4.5). Born 1 May
    ///   1990.
    /// - Amy (502): cast in films 5–19, fifteen from the middle down (0.75 … 0.05: top five 3.25, sum 6).
    ///   Born 1950 (year precision).
    /// - Mo (503): cast in films 0 and 10 (1 + 0.5). No birth on record.
    /// - Ängel (504): cast in film 19 (0.05). Born in the 1970s (decade precision).
    ///
    /// A plain sum would put Amy first on volume; prominence puts Zed's hits first.
    fn orders_store(name: &str, popular: bool) -> Indexes {
        let cast = |i: u32| -> Vec<u32> {
            [(501, i < 5), (502, i >= 5), (503, i == 0 || i == 10), (504, i == 19)]
                .into_iter()
                .filter(|&(_, on)| on)
                .map(|(qid, _)| qid)
                .collect()
        };
        let titles: Vec<Title> = (0..20)
            .map(|i| Title {
                tmdb_id: i + 1,
                primary_genre: "Drama",
                plot: vec![100, 0, 0],
                premise: vec![100, 0, 0],
                card: Some(("A title", None, Some(2020))),
                votes: if popular { 1000 - 10 * i } else { 0 },
                cast: cast(i),
                ..Title::default()
            })
            .collect();
        let entities = [
            Entity { qid: 501, name: "Zed", born: Some((days(1990, 5, 1), 0)), ..Entity::default() },
            Entity { qid: 502, name: "Amy", born: Some((days(1950, 1, 1), 2)), ..Entity::default() },
            Entity { qid: 503, name: "Mo", ..Entity::default() },
            Entity { qid: 504, name: "Ängel", born: Some((days(1970, 1, 1), 3)), ..Entity::default() },
        ];
        load(name, &titles, &entities)
    }

    fn order_of(answer: &Value) -> Vec<String> {
        names(answer).into_iter().map(|(n, _)| n).collect()
    }

    /// Ten 2020 films, film i (TMDB id i + 1) cast with:
    ///
    /// - Fay (601), born 15 March 1976, in film 0; Gus (602), born in the 1970s (decade precision), in film 1;
    ///   Hal (603), no birth on record, in film 2; Ivy (604), born 31 December 1996, in film 3; Jon (605),
    ///   born 1950 (year precision), in film 4; Kim (606), born in the 20th century, in film 5.
    /// - Forty more, P0 … P39 (700 + i), born in 1960 + i (year precision), in film i mod 10.
    fn range_store(name: &str) -> Indexes {
        let titles: Vec<Title> = (0..10u32)
            .map(|i| Title {
                tmdb_id: i + 1,
                primary_genre: "Drama",
                plot: vec![100, 0, 0],
                premise: vec![100, 0, 0],
                card: Some(("A title", None, Some(2020))),
                votes: 1000 - 10 * i,
                cast: [601 + i]
                    .into_iter()
                    .filter(|_| i < 6)
                    .chain((0..40).filter(|p| p % 10 == i).map(|p| 700 + p))
                    .collect(),
                ..Title::default()
            })
            .collect();
        let generated: Vec<String> = (0..40).map(|i| format!("P{i}")).collect();
        let born = |qid, name, born| Entity { qid, name, born, ..Entity::default() };
        let mut entities = vec![
            born(601, "Fay", Some((days(1976, 3, 15), 0))),
            born(602, "Gus", Some((days(1970, 1, 1), 3))),
            born(603, "Hal", None),
            born(604, "Ivy", Some((days(1996, 12, 31), 0))),
            born(605, "Jon", Some((days(1950, 1, 1), 2))),
            born(606, "Kim", Some((days(1950, 1, 1), 4))),
        ];
        for (i, name) in generated.iter().enumerate() {
            entities.push(born(700 + i as u32, name, Some((days(1960 + i as i64, 6, 1), 2))));
        }
        load(name, &titles, &entities)
    }

    fn sorted(answer: &Value) -> Vec<String> {
        let mut found = order_of(answer);
        found.sort();
        found
    }

    /// The generated people born in `years`, and the named ones given, sorted as `sorted` sorts.
    fn born_in(years: std::ops::RangeInclusive<i64>, named: &[&str]) -> Vec<String> {
        let mut expected: Vec<String> = (0..40)
            .filter(|i| years.contains(&(1960 + i)))
            .map(|i| format!("P{i}"))
            .chain(named.iter().map(|n| n.to_string()))
            .collect();
        expected.sort();
        expected
    }

    /// A range holds exact years, both ends inclusive: Fay, born 1976, is in 1976-1996 and not in 1977-1996.
    #[test]
    fn a_born_range_holds_exact_years() {
        let indexes = range_store("born-range");
        let people = |traits: &str| {
            let answer = ask(&indexes, Movie, Route::People, &format!("traits={traits}&limit=100"));
            assert!(answer.get("unknownTraits").is_none(), "{traits}: {answer}");
            sorted(&answer)
        };
        assert_eq!(people("born:1976-1996"), born_in(1976..=1996, &["Fay", "Ivy"]));
        assert_eq!(people("born:1977-1996"), born_in(1977..=1996, &["Ivy"]), "Fay is born the year before");
        assert_eq!(people("born:1976-1995"), born_in(1976..=1995, &["Fay"]), "Ivy is born the year after");
        assert_eq!(people("born:1996-"), born_in(1996..=2100, &["Ivy"]), "an open end");
        assert_eq!(people("born:-1950"), born_in(0..=1950, &["Jon"]), "an open start");
        // A bare decade keeps its meaning: Gus's decade is 1970.
        assert_eq!(people("born:1975"), born_in(1970..=1979, &["Fay", "Gus"]));
    }

    /// A birth dated to its decade or century is in a range its whole span lies inside, out of one it does not
    /// reach, and unknown — neither the range nor its exclusion — when it straddles an end.
    #[test]
    fn a_coarse_birth_is_in_a_range_only_when_its_whole_span_is() {
        let indexes = range_store("born-coarse");
        let people = |traits: &str| {
            sorted(&ask(&indexes, Movie, Route::People, &format!("traits={traits}&limit=100")))
        };
        let has = |traits: &str, name: &str| people(traits).contains(&name.to_owned());
        assert!(!has("born:1976-1996", "Gus"), "the 1970s straddle 1976");
        assert!(!has("-born:1976-1996", "Gus"), "unknown is not out of the range either");
        assert!(has("born:1970-1979", "Gus"), "the whole decade");
        assert!(has("-born:1980-1996", "Gus"), "none of the decade");
        assert!(!has("born:1976-1996", "Kim") && !has("-born:1976-1996", "Kim"), "a century straddles it");
        assert!(has("born:1901-2000", "Kim"), "the whole 20th century");
        assert!(has("born:1800-", "Kim"));
        for traits in ["born:1800-", "-born:1800-", "born:1976-1996", "-born:1976-1996"] {
            assert!(!has(traits, "Hal"), "{traits}: no birth on record matches nothing");
        }
        let mut outside = born_in(1960..=1976, &["Fay", "Jon"]);
        outside.extend(born_in(1997..=1999, &[]));
        outside.sort();
        assert_eq!(
            people("-born:1977-1996"),
            outside,
            "Ivy is in, Gus and Kim straddle 1977, Hal is unknown"
        );
    }

    /// Paged a few at a time, a range lists every match once, whatever the order.
    #[test]
    fn a_born_range_pages_through_every_match_once() {
        let indexes = range_store("born-paged");
        let expected = born_in(1976..=1996, &["Fay", "Ivy"]);
        for order in ["prominence", "credits", "name", "born_asc"] {
            let mut paged = Vec::new();
            for skip in (0..30).step_by(4) {
                let query = format!("traits=born:1976-1996&order={order}&skip={skip}&limit=4");
                let page = ask(&indexes, Movie, Route::People, &query);
                assert_eq!(page["total"], expected.len(), "{query}");
                paged.extend(order_of(&page));
            }
            let mut once = paged.clone();
            once.sort();
            assert_eq!(once, expected, "{order}: every match, each once");
        }
    }

    /// `people/counts.json` under a range: the total and the other kinds over the people in it, `born` by
    /// decade as if the range were not picked, and the range's coverage the people it is known for.
    #[test]
    fn a_born_range_is_counted_as_a_pick() {
        let indexes = range_store("born-counts");
        let all = ask(&indexes, Movie, Route::PeopleCounts, "");
        let counts = ask(&indexes, Movie, Route::PeopleCounts, "traits=born:1976-1996");
        assert_eq!(counts["total"], 23);
        assert_eq!(counts["traits"]["role"]["values"], json!({ "cast": 23 }));
        assert_eq!(counts["traits"]["born"]["values"], all["traits"]["born"]["values"]);
        assert_eq!(counts["traits"]["born"]["selected"], json!(["1976-1996"]));
        assert!(counts["traits"]["born"]["values"].get("1976-1996").is_none(), "a range is no decade");
        assert_eq!(
            counts["traitCoverage"]["born"],
            json!({ "count": 43, "denominator": 46 }),
            "Gus and Kim straddle the range, Hal has no birth"
        );
        let decade = ask(&indexes, Movie, Route::PeopleCounts, "traits=born:1970");
        assert_eq!(
            decade["traitCoverage"]["born"],
            json!({ "count": 44, "denominator": 46 }),
            "a decade pick: Kim's century and Hal have none"
        );
        let excluded = ask(&indexes, Movie, Route::PeopleCounts, "traits=-born:1976-1996");
        assert_eq!(excluded["total"], 43 - 23);
        assert_eq!(excluded["traits"]["born"]["excluded"], json!(["1976-1996"]));
    }

    /// Every order ranks as it says, pages stably, and names itself in `order`.
    #[test]
    fn each_order_ranks_as_it_says() {
        let indexes = orders_store("orders", true);
        for (query, expected) in [
            ("", ["Zed", "Amy", "Mo", "Ängel"]),
            ("order=prominence", ["Zed", "Amy", "Mo", "Ängel"]),
            ("order=credits", ["Amy", "Zed", "Mo", "Ängel"]),
            // Folded as search folds names: Ängel is read as angel, not after z.
            ("order=name", ["Amy", "Ängel", "Mo", "Zed"]),
            // Mo has no birth on record, and is last whichever way the order runs.
            ("order=born_asc", ["Amy", "Ängel", "Zed", "Mo"]),
            ("order=born_desc", ["Zed", "Ängel", "Amy", "Mo"]),
        ] {
            let answer = ask(&indexes, Movie, Route::People, query);
            assert_eq!(order_of(&answer), expected, "{query}");
            let name = query.strip_prefix("order=").unwrap_or("prominence");
            assert_eq!(answer["order"], name, "{query}");
            assert!(answer.get("orderUnavailable").is_none(), "{answer}");
            assert!(answer["people"][0].get("prominence").is_none(), "no score is published");
            let mut paged = Vec::new();
            for skip in 0..4 {
                let sep = if query.is_empty() { "" } else { "&" };
                let page = ask(&indexes, Movie, Route::People, &format!("{query}{sep}skip={skip}&limit=1"));
                paged.extend(order_of(&page));
            }
            assert_eq!(paged, expected, "{query}, a page at a time");
        }
    }

    /// Prominence counts a person's biggest titles, not how many they have: five hits outrank fifteen titles
    /// from the middle of the table, which a plain sum of the weights (6 against 4.5) would put first.
    #[test]
    fn prominence_is_top_heavy() {
        let indexes = orders_store("top-heavy", true);
        let context = Context::new(&indexes, Movie, None);
        let request = Request::parse(Route::People, Movie.into(), "").unwrap();
        let people = context.people_of(&request, true, None);
        let tally = &people.tally;
        let entity = |qid: &str| context.entity_of(qid).unwrap();
        let (zed, amy) = (entity("Q501"), entity("Q502"));
        assert!((tally.prominence(zed) - 4.5).abs() < 1e-9, "{}", tally.prominence(zed));
        assert!((tally.prominence(amy) - 3.25).abs() < 1e-9, "{}", tally.prominence(amy));
        let answer = ask(&indexes, Movie, Route::People, "");
        assert_eq!(order_of(&answer)[..2], ["Zed", "Amy"]);
    }

    /// With no popularity order — no title has a vote count, and the type's order is by TMDB id alone —
    /// prominence is not ranked on that order, which would weigh the lowest ids as hits: the answer is in
    /// credits and says so, asked for or by default.
    #[test]
    fn prominence_without_popularity_falls_back_to_credits() {
        let indexes = orders_store("unpopular", false);
        for query in ["", "order=prominence"] {
            let (answer, degraded) = Context::new(&indexes, Movie, None)
                .people(&Request::parse(Route::People, Movie.into(), query).unwrap());
            assert_eq!(order_of(&answer), ["Amy", "Zed", "Mo", "Ängel"], "{query}: {answer}");
            assert_eq!(
                (&answer["order"], &answer["orderUnavailable"]),
                (&json!("credits"), &json!("prominence"))
            );
            assert!(
                !degraded,
                "no ratings provider here: a property of the deployment, not a passing failure"
            );
        }
        let credits = ask(&indexes, Movie, Route::People, "order=credits");
        assert!(credits.get("orderUnavailable").is_none(), "credits was what it asked for");
        let named = ask(&indexes, Movie, Route::People, "order=name");
        assert_eq!(order_of(&named), ["Amy", "Ängel", "Mo", "Zed"], "the other orders need no popularity");
    }

    /// Ten 2020 films, film i (TMDB id i + 1) at rank i, so it weighs 1 − i/10, and TMDB's credits for the
    /// first five when `billed` (synthetic):
    ///
    /// - Cam (701): cast in films 0–4, billed below their first ten each time — a bit part.
    /// - Lee (702): cast in films 0–4, billed first each time.
    /// - Una (703): cast in films 0–4, with no TMDB id to look for in the billing.
    /// - Dee (704): directs films 0–4 and is billed ninth in them.
    /// - Ned (705): cast in films 5–9, which have no credits kept.
    ///
    /// Without a billing Cam, Lee, Una and Dee tie at 1 + 0.9 + 0.8 + 0.7 + 0.6, Cam first on the Q-id.
    fn billing_store(name: &str, billed: bool) -> Indexes {
        let titles: Vec<Title> = (0..10u32)
            .map(|i| Title {
                tmdb_id: i + 1,
                primary_genre: "Drama",
                plot: vec![100, 0, 0],
                premise: vec![100, 0, 0],
                card: Some(("A title", None, Some(2020))),
                votes: 1000 - 10 * i,
                cast: if i < 5 { vec![701, 702, 703, 704] } else { vec![705] },
                directors: if i < 5 { vec![704] } else { Vec::new() },
                makers: if i < 5 { vec![704] } else { Vec::new() },
                ..Title::default()
            })
            .collect();
        let person = |qid: u32, name: &'static str, tmdb: Option<u32>| Entity {
            qid,
            name,
            tmdb,
            genders: vec![MALE],
            ..Entity::default()
        };
        let entities = [
            person(701, "Cam", Some(9701)),
            person(702, "Lee", Some(9702)),
            person(703, "Una", None),
            person(704, "Dee", Some(9704)),
            person(705, "Ned", Some(9705)),
            label(MALE, "male"),
        ];
        let mut indexes = load(name, &titles, &entities);
        if billed {
            let role = |order: u32, person: u32, character: &str| crate::tmdb::Role {
                order,
                person,
                character: character.into(),
            };
            let credits: HashMap<crate::ratings::Key, crate::tmdb::Credits> = (1..=5)
                .map(|id| {
                    let roles = vec![role(0, 9702, "The Lead"), role(8, 9704, "A Passer-by")];
                    ((0, id), crate::tmdb::Credits { fetched: 1, roles })
                })
                .collect();
            let index = crate::characters::build(&indexes.store.view(), &credits).unwrap();
            assert_eq!(index.billing().titles(), 5);
            indexes.characters = Some(std::sync::Arc::new(crate::characters::Characters::with_index(index)));
        }
        indexes
    }

    fn label(qid: u32, name: &'static str) -> Entity<'static> {
        Entity { qid, name, ..Entity::default() }
    }

    /// The issue's case: a bit part in the same hits no longer ranks with their lead. Without the billing
    /// the two tie, and the tie goes to the bit part on the Q-id.
    #[test]
    fn a_lead_outranks_a_bit_part_on_the_same_titles() {
        let plain = ask(&billing_store("unbilled", false), Movie, Route::People, "sel=decade:2020");
        assert_eq!(order_of(&plain), ["Cam", "Lee", "Una", "Dee", "Ned"], "{plain}");
        let billed = billing_store("billed", true);
        for query in ["sel=decade:2020", "sel=decade:2020&order=prominence&traits=role:cast"] {
            let answer = ask(&billed, Movie, Route::People, query);
            assert_eq!(order_of(&answer), ["Lee", "Una", "Dee", "Ned", "Cam"], "{query}: {answer}");
            assert!(answer["people"][0].get("prominence").is_none(), "no score is published");
        }
    }

    /// A credit whose billing is not known weighs as it did: Una has no TMDB id, Ned's films have no credits
    /// kept, and Dee directs the films he is billed ninth in. Cam's bit parts weigh a fifth.
    #[test]
    fn unknown_billing_weighs_as_before() {
        let indexes = billing_store("unknown-billing", true);
        let context = Context::new(&indexes, Movie, None);
        let billing = context.characters.as_deref().map(CharacterIndex::billing);
        assert!(billing.is_some());
        let request = Request::parse(Route::People, Movie.into(), "").unwrap();
        let (plain, billed) =
            (context.people_of(&request, true, None), context.people_of(&request, true, billing));
        let entity = |qid: &str| context.entity_of(qid).unwrap();
        for (qid, before, after) in [
            ("Q701", 4.0, 0.8),
            ("Q702", 4.0, 4.0),
            ("Q703", 4.0, 4.0),
            ("Q704", 4.0, 4.0),
            ("Q705", 1.5, 1.5),
        ] {
            let e = entity(qid);
            assert!((plain.tally.prominence(e) - before).abs() < 1e-9, "{qid} {}", plain.tally.prominence(e));
            assert!(
                (billed.tally.prominence(e) - after).abs() < 1e-9,
                "{qid} {}",
                billed.tally.prominence(e)
            );
        }
    }

    /// The billing is read for prominence alone: the other orders, the counts and `knownFor` are what they
    /// were without it, and paging through the billed order gives every person once, in order.
    #[test]
    fn billing_changes_prominence_alone() {
        let (plain, billed) = (billing_store("alone-plain", false), billing_store("alone-billed", true));
        for query in ["order=credits", "order=name", "order=born_asc"] {
            assert_eq!(
                ask(&plain, Movie, Route::People, query)["people"],
                ask(&billed, Movie, Route::People, query)["people"],
                "{query}"
            );
        }
        let known = |indexes: &Indexes| {
            let answer = ask(indexes, Movie, Route::People, "");
            let mut known: Vec<(String, Value)> = answer["people"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| (p["name"].as_str().unwrap().to_owned(), p["knownFor"].clone()))
                .collect();
            known.sort_by(|a, b| a.0.cmp(&b.0));
            known
        };
        assert_eq!(known(&plain), known(&billed));
        assert_eq!(
            ask(&plain, Movie, Route::PeopleCounts, "sel=decade:2020"),
            ask(&billed, Movie, Route::PeopleCounts, "sel=decade:2020")
        );
        let whole = order_of(&ask(&billed, Movie, Route::People, ""));
        let paged: Vec<String> = (0..whole.len())
            .step_by(2)
            .flat_map(|skip| order_of(&ask(&billed, Movie, Route::People, &format!("skip={skip}&limit=2"))))
            .collect();
        assert_eq!(paged, whole);
    }

    #[test]
    fn a_credit_weighs_its_title_by_its_billing() {
        let shares: Vec<f64> = [None, Some(0), Some(2), Some(3), Some(5), Some(9)]
            .into_iter()
            .map(|at| billed(at.map(Billed::At)))
            .collect();
        assert_eq!(shares, [1.0, 1.0, 1.0, 0.75, 0.5, 0.3]);
        assert_eq!(billed(Some(Billed::Below)), BIT_PART);
    }

    /// `people/values/<trait>.json`: every value of an entity trait counted under the selection and the other
    /// traits, found by a word of its name or an alias; a one-pick trait counted without its own pick.
    #[test]
    fn trait_values_are_counted_and_found_by_name() {
        let indexes = people_store("trait-values");
        let values = |kind: &'static str, query: &str| -> (Vec<(String, u64)>, Value) {
            let scope: Scope = Movie.into();
            let request = Request::parse(Route::PeopleValues(kind), scope, query).unwrap();
            let answer = Context::new(&indexes, scope, None).people_values(kind, &request).0;
            let found = answer["values"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| (v["id"].as_str().unwrap().to_owned(), v["count"].as_u64().unwrap()))
                .collect();
            (found, answer)
        };
        let pairs =
            |p: &[(&str, u64)]| -> Vec<(String, u64)> { p.iter().map(|&(i, n)| (i.to_owned(), n)).collect() };

        let (all, answer) = values("citizenship", "sel=decade:2020");
        assert_eq!(all, pairs(&[("Q300", 2), ("Q301", 1)]), "Ann and Bob are Swedish, Bob American too");
        assert_eq!((&answer["complete"], &answer["denominator"]), (&json!(true), &json!(5)));
        assert_eq!(answer["values"][0]["name"], "Sweden");
        assert_eq!(
            values("citizenship", "sel=decade:2020&q=stat").0,
            pairs(&[("Q301", 1)]),
            "a word of the name"
        );
        assert_eq!(values("citizenship", "sel=decade:2020&q=kingdom").0, pairs(&[("Q300", 2)]), "an alias");
        assert_eq!(values("citizenship", "sel=decade:2020&q=actor").0, pairs(&[]), "an occupation's name");
        let (top, answer) = values("citizenship", "sel=decade:2020&limit=1");
        assert_eq!((top, &answer["complete"]), (pairs(&[("Q300", 2)]), &json!(false)));

        // Gender is one pick: counted without its own, as people/counts.json counts it.
        let (genders, _) = values("gender", "sel=decade:2020&traits=gender:Q201");
        assert_eq!(genders, pairs(&[("Q201", 2), ("Q200", 1), ("Q202", 1)]));
        // Occupation holds several: counted under the picks, the gender among them.
        let (jobs, _) = values("occupation", "sel=decade:2020&traits=gender:Q201,occupation:Q400");
        assert_eq!(jobs, pairs(&[("Q400", 2), ("Q401", 1)]), "Bob and Eve act, Bob directs");
        let (crew, _) = values("occupation", "sel=decade:2020&traits=-role:cast&q=screen");
        assert_eq!(crew, pairs(&[("Q402", 1)]), "Cid writes film 1 and is not in its cast");
    }

    /// `knownFor`: a person's three biggest matching titles by their standing in their own type — a series at
    /// the top of the series beside a film at the top of the films — counting only the credits the role asks
    /// for, and left out when there is no popularity order to tell the biggest.
    #[test]
    fn known_for_is_the_biggest_matching_titles() {
        let indexes = people_store("known-for");
        let known = |answer: &Value, name: &str| -> Vec<(String, u64)> {
            let person = answer["people"].as_array().unwrap().iter().find(|p| p["name"] == name).unwrap();
            person["knownFor"]
                .as_array()
                .unwrap_or_else(|| panic!("{person}"))
                .iter()
                .map(|t| (t["type"].as_str().unwrap().to_owned(), t["id"].as_u64().unwrap()))
                .collect()
        };
        let pair = |kind: &str, id: u64| (kind.to_owned(), id);
        let all = ask(&indexes, Scope::All, Route::People, "order=name");
        assert_eq!(
            known(&all, "Bob"),
            [pair("movie", 1), pair("series", 4), pair("movie", 2)],
            "film 1 tops the films and series 4 the series; film 2 is the films' second"
        );
        let bob = all["people"].as_array().unwrap().iter().find(|p| p["name"] == "Bob").unwrap();
        assert_eq!(bob["knownFor"][0], json!({ "type": "movie", "id": 1, "title": "A title", "year": 2020 }));
        let directing = ask(&indexes, Scope::All, Route::People, "traits=role:director");
        assert_eq!(known(&directing, "Bob"), [pair("movie", 2)], "he directs film 2 alone");
        let nineties = ask(&indexes, Movie, Route::People, "sel=decade:1990");
        assert_eq!(known(&nineties, "Ann"), [pair("movie", 3)], "the selection's titles alone");

        let unpopular = orders_store("known-for-unpopular", false);
        let answer = ask(&unpopular, Movie, Route::People, "");
        assert!(answer["people"][0].get("knownFor").is_none(), "no order to tell the biggest by: {answer}");
        let popular = orders_store("known-for-popular", true);
        let answer = ask(&popular, Movie, Route::People, "order=credits");
        assert_eq!(known(&answer, "Amy"), [pair("movie", 6), pair("movie", 7), pair("movie", 8)]);
    }

    /// A store with no trait sections answers the credits and roles, and names the person traits it cannot
    /// apply — a property of the dataset version, so the answer is not degraded.
    #[test]
    fn a_store_without_traits_ignores_them() {
        let dir = std::env::temp_dir().join(format!("den-atlas-people-plain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let indexes = crate::queries::load_for_tools(&crate::queries::write_fixture(&dir)).unwrap();
        let request = Request::parse(Route::People, Movie.into(), "traits=gender:Q1,role:cast").unwrap();
        let (answer, degraded) = Context::new(&indexes, Movie, None).people(&request);
        assert!(!degraded);
        assert_eq!(answer["ignoredTraits"], json!(["gender"]));
        assert_eq!(names(&answer), named(&[("Lead Actor", 1), ("Q3", 1)]), "movie 1's cast");
        let counts = ask(&indexes, Movie, Route::PeopleCounts, "");
        assert!(counts["traits"].get("gender").is_none(), "{counts}");
        assert_eq!(counts["traits"]["role"]["values"], json!({ "cast": 2 }), "makers are not split here");
    }

    /// The people orders over the REAL corpus: the first ten in `credits` and in `prominence`, and what
    /// prominence costs. Opt-in: `DEN_STORE` names a store whose directory holds its `dataset.meta.json`, and
    /// `CACHE_DIR` a directory of kept TMDB numbers (`tmdb-votes.tsv`) for the popularity order; without it
    /// the answers fall back to `credits`.
    #[test]
    fn real_corpus_people_orders() {
        let Ok(store) = std::env::var("DEN_STORE") else {
            eprintln!("SKIP: set DEN_STORE to a real den-<ver>.store to measure this");
            return;
        };
        let dir = std::path::Path::new(&store).parent().expect("the store sits in a dataset directory");
        let ds = crate::dataset::Dataset::load(dir).expect("the dataset loads");
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let tmdb = std::env::var("CACHE_DIR").ok().map(|kept| {
            let tmdb = crate::tmdb::Tmdb::new(ds.mapped.clone(), Some(kept.into()), None, 0).unwrap();
            eprintln!("{}", runtime.block_on(tmdb.load()));
            tmdb
        });
        let (indexes, _) = runtime
            .block_on(
                crate::queries::IndexQueries::new(&ds)
                    .with_ratings(tmdb.as_ref().map(|t| t.ratings()))
                    .get(|| ()),
            )
            .expect("the indexes load");
        for (scope, query) in [
            (Scope::from(Movie), "sel=decade:2020&traits=gender:Q6581097,role:cast"),
            (Scope::All, "traits=citizenship:Q34,role:director"),
            (Scope::All, "traits=role:director"),
        ] {
            let context = Context::new(&indexes, scope, None);
            for order in ["credits", "prominence"] {
                let request =
                    Request::parse(Route::People, scope, &format!("{query}&order={order}")).unwrap();
                let started = std::time::Instant::now();
                let (answer, _) = context.people(&request);
                let took = started.elapsed();
                let first: Vec<String> = answer["people"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .take(10)
                    .map(|p| format!("{} ({})", p["name"].as_str().unwrap_or("?"), p["credits"]))
                    .collect();
                eprintln!("{query} order={order} → {} in {took:?}: {}", answer["order"], first.join(" | "));
                eprintln!("  first knownFor: {}", answer["people"][0]["knownFor"]);
            }
        }
        for (kind, query) in [
            ("citizenship", "sel=decade:2020&q=iceland"),
            ("citizenship", "sel=decade:2020&traits=role:director"),
            ("occupation", "traits=gender:Q6581072&q=compos"),
        ] {
            let scope: Scope = Movie.into();
            let request = Request::parse(Route::PeopleValues(kind), scope, query).unwrap();
            let started = std::time::Instant::now();
            let answer = Context::new(&indexes, scope, None).people_values(kind, &request).0;
            eprintln!(
                "people/values/{kind} {query} in {:?}: {} (complete {})",
                started.elapsed(),
                answer["values"],
                answer["complete"]
            );
        }
    }

    /// A born range over the REAL corpus against the per-decade requests it replaces: the male cast of 2020s
    /// films born 1976–1996, paged through whole, beside one 100-person page per decade filtered to the years.
    /// Opt-in: `DEN_STORE` names a store whose directory holds its `dataset.meta.json`.
    #[test]
    fn real_corpus_born_range() {
        let Ok(store) = std::env::var("DEN_STORE") else {
            eprintln!("SKIP: set DEN_STORE to a real den-<ver>.store to measure this");
            return;
        };
        let dir = std::path::Path::new(&store).parent().expect("the store sits in a dataset directory");
        let ds = crate::dataset::Dataset::load(dir).expect("the dataset loads");
        let indexes = crate::queries::load_for_tools(&ds).expect("the indexes load");
        let base = "sel=decade:2020&traits=born:1976-1996,gender:Q6581097,role:cast";
        let context = Context::new(&indexes, Movie, None);
        let run =
            |query: &str| context.people(&Request::parse(Route::People, Movie.into(), query).unwrap()).0;
        run(base);
        let started = std::time::Instant::now();
        let first = run(&format!("{base}&limit=100"));
        let one = started.elapsed();
        let total = first["total"].as_u64().unwrap() as usize;
        let started = std::time::Instant::now();
        let mut ids = std::collections::HashSet::new();
        for skip in (0..total).step_by(100) {
            let page = run(&format!("{base}&skip={skip}&limit=100"));
            assert_eq!(page["total"], total);
            for p in page["people"].as_array().unwrap() {
                assert!(ids.insert(p["id"].as_str().unwrap().to_owned()), "{} twice", p["id"]);
            }
        }
        let whole = started.elapsed();
        assert_eq!(ids.len(), total, "every match once");
        eprintln!(
            "range: total {total}; first page of 100 in {one:?}; all {} pages in {whole:?}",
            total.div_ceil(100)
        );

        let started = std::time::Instant::now();
        let (mut kept, mut decade_totals) = (0, Vec::new());
        for decade in [1970, 1980, 1990] {
            let query = format!("sel=decade:2020&traits=born:{decade},gender:Q6581097,role:cast&limit=100");
            let page = run(&query);
            decade_totals.push(page["total"].as_u64().unwrap());
            kept += page["people"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|p| p["born"]["year"].as_i64().is_some_and(|y| (1976..=1996).contains(&y)))
                .count();
        }
        eprintln!(
            "per decade: totals {decade_totals:?}; three 100-person pages in {:?} keep {kept} of the {total}",
            started.elapsed()
        );
    }

    /// Prominence before and after the billing (oxyc/den-atlas#82) over the REAL corpus: the first ten of each
    /// list as a Markdown table, names only, and the first hundred paged through against the whole. Opt-in:
    /// `DEN_STORE` names a store whose directory holds its `dataset.meta.json`, and `CACHE_DIR` the kept TMDB
    /// numbers — `tmdb-votes.tsv` for the popularity order and `tmdb-credits.tsv` for the billing.
    #[test]
    fn real_corpus_billing_before_after() {
        let (Ok(store), Ok(kept)) = (std::env::var("DEN_STORE"), std::env::var("CACHE_DIR")) else {
            eprintln!("SKIP: set DEN_STORE to a real den-<ver>.store and CACHE_DIR to its kept TMDB numbers");
            return;
        };
        let dir = std::path::Path::new(&store).parent().expect("the store sits in a dataset directory");
        let ds = crate::dataset::Dataset::load(dir).expect("the dataset loads");
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let tmdb = crate::tmdb::Tmdb::new(ds.mapped.clone(), Some(kept.into()), None, 0).unwrap();
        eprintln!("{}", runtime.block_on(tmdb.load()));
        let (indexes, _) = runtime
            .block_on(
                crate::queries::IndexQueries::new(&ds)
                    .with_ratings(Some(tmdb.ratings()))
                    .with_characters(Some(tmdb.characters()))
                    .get(|| ()),
            )
            .expect("the indexes load");
        for (title, scope, query) in [
            (
                "Male cast, 2020s films",
                Scope::from(Movie),
                "sel=decade:2020&traits=gender:Q6581097,role:cast",
            ),
            (
                "Swedish directors born in the 1970s",
                Scope::All,
                "traits=citizenship:Q34,role:director,born:1970",
            ),
            (
                "Actresses in horror films",
                Scope::from(Movie),
                "sel=genre:27&traits=gender:Q6581072,role:cast",
            ),
            (
                "Actresses, 1990s films",
                Scope::from(Movie),
                "sel=decade:1990&traits=gender:Q6581072,role:cast",
            ),
            ("Series cast", Scope::from(den_index::MediaType::Tv), "traits=role:cast"),
            ("Swedish cast", Scope::All, "traits=citizenship:Q34,role:cast"),
        ] {
            let mut context = Context::new(&indexes, scope, None);
            let run = |context: &Context, query: &str| {
                let answer = context.people(&Request::parse(Route::People, scope, query).unwrap()).0;
                assert_eq!(answer["order"], "prominence", "{query}: a popularity order is needed");
                order_of(&answer)
            };
            let after = run(&context, &format!("{query}&limit=100"));
            let paged: Vec<String> = (0..100)
                .step_by(20)
                .flat_map(|skip| run(&context, &format!("{query}&skip={skip}&limit=20")))
                .collect();
            assert_eq!(paged, after, "{query}: paged");
            context.characters = None;
            let before = run(&context, &format!("{query}&limit=10"));
            eprintln!("\n{title} (`{query}`)\n\n| # | before | after |\n|---|---|---|");
            for (at, (was, now)) in before.iter().zip(&after).enumerate() {
                eprintln!("| {} | {was} | {now} |", at + 1);
            }
        }
    }

    /// The people routes over the REAL corpus, and what they cost. Opt-in: `DEN_STORE` names a store whose
    /// directory holds its `dataset.meta.json`. "Cold" is the first answer after the indexes load, which maps
    /// the credit and trait sections' pages in; "warm" is the mean of the next twenty.
    #[test]
    fn real_corpus_people_and_timing() {
        let Ok(store) = std::env::var("DEN_STORE") else {
            eprintln!("SKIP: set DEN_STORE to a real den-<ver>.store to measure this");
            return;
        };
        let dir = std::path::Path::new(&store).parent().expect("the store sits in a dataset directory");
        let ds = crate::dataset::Dataset::load(dir).expect("the dataset loads");
        let started = std::time::Instant::now();
        let indexes = crate::queries::load_for_tools(&ds).expect("the indexes load");
        eprintln!("indexes loaded in {:?}", started.elapsed());
        let time = |label: &str, scope: Scope, route: Route, query: &str| -> Value {
            let request = Request::parse(route, scope, query).unwrap();
            let run = || {
                let context = Context::new(&indexes, scope, None);
                match route {
                    Route::People => context.people(&request).0,
                    _ => context.people_counts(&request).0,
                }
            };
            let first = std::time::Instant::now();
            let answer = run();
            let cold = first.elapsed();
            let rounds = 20;
            let warm = std::time::Instant::now();
            for _ in 0..rounds {
                run();
            }
            eprintln!("{label} {query:?}: cold {cold:?}, warm {:?}", warm.elapsed() / rounds);
            answer
        };
        let show = |answer: &Value| {
            let people: Vec<String> = answer["people"]
                .as_array()
                .unwrap()
                .iter()
                .take(12)
                .map(|p| format!("{} {} ({})", p["id"].as_str().unwrap(), p["name"], p["credits"]))
                .collect();
            eprintln!("  total {}, first {people:?}", answer["total"]);
        };
        // The issue's example first, while nothing is mapped in: male actors in 2020 films.
        let example = "sel=decade:2020&traits=gender:Q6581097,role:cast";
        let male_actors = time("people movie", Movie.into(), Route::People, example);
        show(&male_actors);
        eprintln!("  first person: {}", male_actors["people"][0]);
        assert!(male_actors["total"].as_u64().unwrap() > 100, "{male_actors}");
        let counts = time("people/counts movie", Movie.into(), Route::PeopleCounts, example);
        for kind in ["gender", "born", "citizenship", "occupation", "role"] {
            let answer = &counts["traits"][kind];
            let values: Vec<String> = answer["values"]
                .as_object()
                .unwrap()
                .iter()
                .take(12)
                .map(|(id, n)| {
                    let label = answer["labels"][id].as_str().map(|l| format!(" {l}")).unwrap_or_default();
                    format!("{id}{label}={n}")
                })
                .collect();
            eprintln!("  {kind} ({}): {}", answer["mode"], values.join(", "));
        }
        eprintln!("  total {}, traitCoverage {}", counts["total"], counts["traitCoverage"]);
        let by_job = time(
            "people movie",
            Movie.into(),
            Route::People,
            "sel=decade:2020&traits=gender:Q6581097,occupation:Q33999",
        );
        show(&by_job);
        for (scope, route, query) in [
            (Scope::All, Route::People, ""),
            (Scope::All, Route::PeopleCounts, ""),
            (Scope::All, Route::People, "traits=born:1970,citizenship:Q34,role:director"),
            (Movie.into(), Route::People, "sel=genre:27&traits=gender:Q6581072,role:cast"),
            (
                Movie.into(),
                Route::People,
                "sel=decade:2020&traits=gender:Q6581097,role:cast&skip=100&limit=100",
            ),
            // OR groups beside their AND forms: American or British, against dual citizens.
            (Scope::All, Route::People, "traits=citizenship:Q30|Q145"),
            (Scope::All, Route::People, "traits=citizenship:Q145,citizenship:Q30"),
            (Scope::All, Route::People, "traits=-citizenship:Q145|Q30"),
            (Scope::All, Route::PeopleCounts, "traits=citizenship:Q145|Q30"),
            (Movie.into(), Route::People, "sel=decade:2020&traits=role:cast|director"),
            (Movie.into(), Route::People, "sel=decade:2020&traits=role:cast,role:director"),
            (Movie.into(), Route::PeopleCounts, "sel=decade:2020&traits=role:cast|director"),
        ] {
            let label = if matches!(route, Route::People) { "people" } else { "people/counts" };
            let answer = time(label, scope, route, query);
            if matches!(route, Route::People) {
                show(&answer);
            } else {
                eprintln!("  total {}, gender {}", answer["total"], answer["traits"]["gender"]["values"]);
                eprintln!("  role {}", answer["traits"]["role"]["values"]);
            }
        }
    }
}
