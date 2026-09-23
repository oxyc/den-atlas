//! `/index/filter/<movie|series|all>/people.json` and `people/counts.json` (oxyc/den#136): the people credited
//! on the titles a selection matches, filtered by what Wikidata states about them, most matching titles first.
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
//! only the titles a person holds both on, and `-role:cast` only those they are credited on and not cast in.
//! `credits` is how many matching titles a person's counted credits are on.
//!
//! The person traits are stored as Wikidata states them, as the items it names — `gender` (P21) with whatever
//! values it holds, `citizenship` (P27), `occupation` (P106) — and `born` (P569) by decade. Nothing is
//! inferred, and unknown is never a match: a person with no gender on record matches no `gender:`, and no
//! `-gender:` either, since they are not known to lack it. A birth dated only to its century has no decade.
//! `traitCoverage` says how many of the credited people each applied trait is on record for.

use super::{normalise_id, ones, Context, Id, Item, Mode, Request, Scope, Status, ENTITY_KINDS, TOP_K};
use den_store::{List, PersonDate, PersonTraits, Row};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

const CAST: u8 = 1;
const DIRECTOR: u8 = 2;
const WRITER: u8 = 4;
const CREATOR: u8 = 8;
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
        about: "the decade of birth (P569); a birth dated only to its century has none",
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
                a person holds both on, -role:<role> those they are credited on without it",
    },
];

/// The person kinds: the traits a `fails` mask has a bit for.
const PERSON_KINDS: usize = 4;

fn trait_spec(name: &str) -> Option<&'static TraitSpec> {
    TRAITS.iter().find(|t| t.name == name)
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
    let id = normalise_id(&kind, id, spec.id, scope)?;
    Ok((kind, id))
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

/// A person trait the request applies, with the value it names read into the store's terms: an entity index,
/// or a decade. `None` names nothing the store holds, and matches no one.
struct Applied<'r> {
    bit: u8,
    spec: &'static TraitSpec,
    item: &'r Item,
    target: Option<i64>,
}

/// The traits of a request, split.
struct Split<'r> {
    applied: Vec<Applied<'r>>,
    /// Roles a counted credit must hold, and must not.
    want: u8,
    avoid: u8,
    /// Trait kinds named and not applied, and items whose value the kind does not hold.
    ignored: Vec<String>,
    unknown: Vec<String>,
    /// The person kinds with a positive or excluded item.
    selected: u8,
}

/// Per entity: the matching titles its counted credits are on, and the roles it holds on them.
struct Tally {
    credits: Vec<u32>,
    held: Vec<u8>,
    /// The entities credited at least once, in the order first met.
    touched: Vec<u32>,
}

/// The decade a birth falls in, when it is dated finely enough to have one.
fn birth_decade(born: Option<PersonDate>) -> Option<i64> {
    born.filter(|d| d.precision <= 3).map(|d| crate::facts::civil_year(i64::from(d.days)).div_euclid(10) * 10)
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
            want: 0,
            avoid: 0,
            ignored: Vec::new(),
            unknown: Vec::new(),
            selected: 0,
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
            let target = match spec.data {
                Trait::Role => {
                    let bit = ROLES.iter().find(|(name, _)| *name == item.id).map(|&(_, bit)| bit);
                    match bit.filter(|&bit| sources.roles & bit != 0) {
                        Some(bit) if item.exclude => split.avoid |= bit,
                        Some(bit) => split.want |= bit,
                        None => {
                            split.unknown.push(item.spelled());
                            if !item.exclude {
                                split.want |= NO_ROLE;
                            }
                        }
                    }
                    continue;
                }
                Trait::Born => item.id.parse::<i64>().ok(),
                Trait::Gender | Trait::Citizenship | Trait::Occupation => {
                    self.entity_of(&item.id).map(i64::from)
                }
            };
            if target.is_none() {
                split.unknown.push(item.spelled());
            }
            let bit = 1 << TRAITS.iter().position(|t| t.data == spec.data).unwrap_or(0);
            split.selected |= bit;
            split.applied.push(Applied { bit, spec, item, target });
        }
        split
    }

    /// Every person credited on the rows of `base`, counting a title for them when their credits on it hold
    /// every role of `want` and none of `avoid`.
    fn tally(&self, sources: &Sources<'a>, base: &[u64], want: u8, avoid: u8) -> Tally {
        let size = self.view.column::<u32>("ent_qid").map_or(0, <[u32]>::len);
        let mut tally = Tally { credits: vec![0; size], held: vec![0; size], touched: Vec::new() };
        let mut on_row = vec![0u8; size];
        let mut credited: Vec<u32> = Vec::new();
        let lists = [
            (&sources.cast, CAST),
            (&sources.makers, 0),
            (&sources.directors, DIRECTOR),
            (&sources.writers, WRITER),
            (&sources.creators, CREATOR),
        ];
        for row in ones(base) {
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
                let roles = on_row[e] & !CREDITED;
                on_row[e] = 0;
                if roles & want != want || roles & avoid != 0 {
                    continue;
                }
                if tally.credits[e] == 0 {
                    tally.touched.push(e as u32);
                }
                tally.credits[e] += 1;
                tally.held[e] |= roles;
            }
        }
        tally
    }

    /// Whether a person holds a trait's value: `None` when the trait is not on record for them.
    fn holds(&self, sources: &Sources<'a>, data: Trait, target: Option<i64>, e: u32) -> Option<bool> {
        let among = |values: &[u32]| {
            (!values.is_empty()).then(|| target.is_some_and(|t| values.iter().any(|&v| i64::from(v) == t)))
        };
        match data {
            Trait::Gender => among(sources.traits.genders(e)),
            Trait::Citizenship => among(sources.traits.citizenships(e)),
            Trait::Occupation => among(sources.traits.occupations(e)),
            Trait::Born => birth_decade(sources.traits.born(e)).map(|decade| Some(decade) == target),
            Trait::Role => None,
        }
    }

    /// The person kinds whose items a person does not satisfy, as bits. Unknown satisfies nothing: neither
    /// the value nor its exclusion.
    fn fails(&self, sources: &Sources<'a>, applied: &[Applied<'_>], e: u32) -> u8 {
        applied
            .iter()
            .filter(|a| self.holds(sources, a.spec.data, a.target, e) != Some(!a.item.exclude))
            .fold(0, |fails, a| fails | a.bit)
    }

    /// The title selection, the traits, and who is credited under them.
    fn people_of<'r>(&self, request: &'r Request) -> People<'a, 'r> {
        let (applied, ignored) = self.split(&request.items);
        let base = self.matched(&applied, None);
        let sources = self.sources();
        let split = self.split_traits(&sources, &request.traits);
        let tally = self.tally(&sources, &base, split.want, split.avoid);
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
        People { sources, split, tally, envelope, degraded }
    }

    /// `people/counts.json`: for every value of every trait, the people credited under the selection and the
    /// other traits holding it — a one-pick kind (gender, born) counted without its own pick, as
    /// `counts.json` counts a one-pick kind.
    pub fn people_counts(&self, request: &Request) -> (Value, bool) {
        let people = self.people_of(request);
        let (sources, split, tally) = (&people.sources, &people.split, &people.tally);
        let mut values: [HashMap<i64, u32>; PERSON_KINDS] = Default::default();
        let mut roles = [0u32; 4];
        let mut known = [0usize; PERSON_KINDS];
        let mut total = 0usize;
        for &e in &tally.touched {
            let fails = self.fails(sources, &split.applied, e);
            if fails == 0 {
                total += 1;
                for (count, &(_, bit)) in roles.iter_mut().zip(&ROLES) {
                    if tally.held[e as usize] & bit != 0 {
                        *count += 1;
                    }
                }
            }
            if sources.status != Status::Ready {
                continue;
            }
            for (i, spec) in TRAITS[..PERSON_KINDS].iter().enumerate() {
                let own = 1u8 << i;
                let alone =
                    if spec.mode == Mode::Single && split.selected & own != 0 { fails & !own } else { fails };
                let (entities, decade): (&[u32], Option<i64>) = match spec.data {
                    Trait::Gender => (sources.traits.genders(e), None),
                    Trait::Citizenship => (sources.traits.citizenships(e), None),
                    Trait::Occupation => (sources.traits.occupations(e), None),
                    Trait::Born => (&[], birth_decade(sources.traits.born(e))),
                    Trait::Role => (&[], None),
                };
                if !entities.is_empty() || decade.is_some() {
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
            .map(|a| {
                let i = a.bit.trailing_zeros() as usize;
                (a.spec.name.to_owned(), json!({ "count": known[i], "denominator": credited }))
            })
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
        for item in &selected {
            if values.contains_key(&item.id) {
                continue;
            }
            let value = match spec.data {
                Trait::Born => item.id.parse().ok(),
                Trait::Role => ROLES.iter().find(|r| r.0 == item.id).map(|r| i64::from(r.1)),
                _ => self.entity_of(&item.id).map(i64::from),
            };
            let n = value.and_then(|v| counted.get(&v)).copied().unwrap_or(0);
            if spec.id == Id::Qid {
                if let Some(label) = self.entity_of(&item.id).and_then(|e| self.label(e)) {
                    labels.insert(item.id.clone(), label.into());
                }
            }
            values.insert(item.id.clone(), n.into());
        }
        let mut answer = json!({ "mode": spec.mode.name(), "complete": listed, "values": values });
        if !labels.is_empty() {
            answer["labels"] = Value::Object(labels);
        }
        let ids = |exclude: bool| -> Vec<&str> {
            selected.iter().filter(|i| i.exclude == exclude).map(|i| i.id.as_str()).collect()
        };
        if !ids(false).is_empty() {
            answer["selected"] = json!(ids(false));
        }
        if !ids(true).is_empty() {
            answer["excluded"] = json!(ids(true));
        }
        answer
    }

    /// `people.json`: the people credited on the matching titles and holding every trait, most matching titles
    /// first, then most titles in the whole corpus, then by Q-id; paged as `titles.json` is.
    pub fn people(&self, request: &Request) -> (Value, bool) {
        let people = self.people_of(request);
        let (sources, split, tally) = (&people.sources, &people.split, &people.tally);
        let corpus = ENTITY_KINDS
            .iter()
            .position(|k| k.name == "person")
            .and_then(|i| self.filter.entities[i].as_ref());
        let qids = self.view.column::<u32>("ent_qid").unwrap_or(&[]);
        // (credits, titles in the corpus, Q-id, entity)
        let mut ranked: Vec<(u32, usize, u32, u32)> = tally
            .touched
            .iter()
            .filter(|&&e| self.fails(sources, &split.applied, e) == 0)
            .map(|&e| {
                let titles = corpus.map_or(0, |k| k.titles(e));
                (tally.credits[e as usize], titles, qids.get(e as usize).copied().unwrap_or(0), e)
            })
            .collect();
        let total = ranked.len();
        let order = |a: &(u32, usize, u32, u32), b: &(u32, usize, u32, u32)| {
            b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2))
        };
        let end = request.skip.saturating_add(request.limit);
        if end < ranked.len() {
            ranked.select_nth_unstable_by(end, order);
            ranked.truncate(end);
        }
        ranked.sort_unstable_by(order);
        let mut labels = Map::new();
        let page: Vec<Value> = ranked
            .iter()
            .skip(request.skip)
            .map(|&(credits, _, _, e)| {
                self.person_json(sources, e, credits, tally.held[e as usize], &mut labels)
            })
            .collect();
        let mut answer = people.envelope;
        answer["people"] = json!(page);
        answer["total"] = json!(total);
        answer["labels"] = Value::Object(labels);
        (answer, people.degraded)
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

/// A people request worked out: who is credited, and the envelope every answer carries.
struct People<'a, 'r> {
    sources: Sources<'a>,
    split: Split<'r>,
    tally: Tally,
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
            (t.name.to_owned(), about)
        })
        .collect();
    json!({
        "kinds": kinds,
        "about": "people.json and people/counts.json take the title selection as sel, and the person traits as \
                  traits in the same [-]<kind>:<id> grammar and canonical order. A person matches a trait only \
                  when it is on record for them: unknown matches neither a value nor its exclusion. Gender, \
                  citizenship and occupation are the Wikidata items the store names, labelled in labels",
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
            label(SWEDEN, "Sweden"),
            label(US, "United States"),
            label(ACTOR, "actor"),
            label(DIRECTOR_JOB, "film director"),
            label(SCREENWRITER, "screenwriter"),
        ];
        let dir = std::env::temp_dir().join(format!("den-atlas-people-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::store::fixture::write(&dir.join("den-v1.store"), "v1", 3, &titles, &entities);
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

    /// Ranked by matching titles, then titles in the whole corpus, then Q-id; paged as titles.json is.
    #[test]
    fn people_are_ranked_and_paged() {
        let indexes = people_store("paged");
        let all = ask(&indexes, Scope::All, Route::People, "");
        assert_eq!(
            names(&all),
            named(&[("Bob", 3), ("Cid", 3), ("Ann", 2), ("Dee", 1), ("Eve", 1)]),
            "Bob and Cid tie on three titles each and on the corpus, so the Q-id decides"
        );
        let mut paged = Vec::new();
        for skip in 0..5 {
            let page = ask(&indexes, Scope::All, Route::People, &format!("skip={skip}&limit=1"));
            assert_eq!(page["total"], 5);
            paged.extend(names(&page));
        }
        assert_eq!(paged, names(&all));
        let both = ask(&indexes, Scope::All, Route::People, "traits=citizenship:Q300,citizenship:Q301");
        assert_eq!(names(&both), named(&[("Bob", 3)]), "citizenship holds several at once");
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
        ] {
            let label = if matches!(route, Route::People) { "people" } else { "people/counts" };
            let answer = time(label, scope, route, query);
            if matches!(route, Route::People) {
                show(&answer);
            } else {
                eprintln!("  gender {}", answer["traits"]["gender"]["values"]);
            }
        }
    }
}
