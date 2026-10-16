//! The reviewed building blocks of a Kid's Home.
//!
//! This is deliberately data, not client code. Atlas owns the phrases and exact filters so Web and TV can
//! render the same rows, and each source remains one of Atlas's ordinary policy-bound, pageable endpoints.

use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
enum Source {
    Query { query: &'static str, minimum_score: f32 },
    Franchise { r#type: &'static str, id: u32 },
    Filter { r#type: &'static str, selection: &'static [&'static str] },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Row {
    id: &'static str,
    title: &'static str,
    group: &'static str,
    source: Source,
}

const ROWS: &[Row] = &[
    Row {
        id: "dogs",
        title: "Dogs",
        group: "themes",
        source: Source::Query { query: "family film about a dog", minimum_score: 0.405 },
    },
    Row {
        id: "cats",
        title: "Cats",
        group: "themes",
        source: Source::Query { query: "family film about a cat", minimum_score: 0.4001 },
    },
    Row {
        id: "horses",
        title: "Horses",
        group: "themes",
        source: Source::Query { query: "family film about horses", minimum_score: 0.4063 },
    },
    Row {
        id: "dinosaurs",
        title: "Dinosaurs",
        group: "themes",
        source: Source::Query { query: "family film about dinosaurs", minimum_score: 0.4063 },
    },
    Row {
        id: "under-the-sea",
        title: "Under the Sea",
        group: "themes",
        source: Source::Query { query: "family film about the sea", minimum_score: 0.401 },
    },
    Row {
        id: "fairy-tales",
        title: "Princesses and Fairy Tales",
        group: "themes",
        source: Source::Query { query: "princesses and fairy tales", minimum_score: 0.09 },
    },
    Row {
        id: "moomins",
        title: "Moomins",
        group: "worlds",
        source: Source::Query { query: "Moomins", minimum_score: 0.69 },
    },
    Row {
        id: "paddington",
        title: "Paddington",
        group: "worlds",
        source: Source::Query { query: "Paddington", minimum_score: 0.93 },
    },
    Row {
        id: "winnie-the-pooh",
        title: "Winnie the Pooh",
        group: "worlds",
        source: Source::Franchise { r#type: "movie", id: 250480 },
    },
    Row {
        id: "barbie",
        title: "Barbie",
        group: "worlds",
        source: Source::Franchise { r#type: "movie", id: 21705 },
    },
    Row {
        id: "pokemon",
        title: "Pokémon",
        group: "worlds",
        source: Source::Franchise { r#type: "series", id: 60572 },
    },
    Row {
        id: "cars",
        title: "Cars",
        group: "worlds",
        source: Source::Franchise { r#type: "movie", id: 920 },
    },
    Row {
        id: "thomas-and-friends",
        title: "Thomas & Friends",
        group: "worlds",
        source: Source::Query { query: "Thomas and Friends", minimum_score: 0.79 },
    },
    Row {
        id: "pixar",
        title: "Pixar",
        group: "studios",
        source: Source::Filter { r#type: "movie", selection: &["studio:Q127552"] },
    },
    Row {
        id: "disney-animation",
        title: "Disney Animation",
        group: "studios",
        source: Source::Filter { r#type: "movie", selection: &["studio:Q1047410"] },
    },
    Row {
        id: "studio-ghibli",
        title: "Studio Ghibli",
        group: "studios",
        source: Source::Filter { r#type: "movie", selection: &["studio:Q182950"] },
    },
    Row {
        id: "aardman",
        title: "Aardman",
        group: "studios",
        source: Source::Filter { r#type: "movie", selection: &["studio:Q301092"] },
    },
    Row {
        id: "dreamworks-animation",
        title: "DreamWorks Animation",
        group: "studios",
        source: Source::Filter { r#type: "movie", selection: &["studio:Q500088"] },
    },
    Row {
        id: "animated-1990s",
        title: "Animated '90s Classics",
        group: "classics",
        source: Source::Filter { r#type: "movie", selection: &["genre:16", "decade:1990"] },
    },
    Row {
        id: "animated-1980s",
        title: "Animated '80s Classics",
        group: "classics",
        source: Source::Filter { r#type: "movie", selection: &["genre:16", "decade:1980"] },
    },
    Row {
        id: "short-and-sweet",
        title: "Short and Sweet",
        group: "little-ones",
        source: Source::Filter { r#type: "all", selection: &["runtime:under-90"] },
    },
    Row {
        id: "puppets",
        title: "Puppets",
        group: "little-ones",
        source: Source::Filter {
            r#type: "all",
            selection: &["genre:16", "runtime:under-90", "technique:puppetry"],
        },
    },
    Row {
        id: "stop-motion",
        title: "Stop Motion",
        group: "little-ones",
        source: Source::Filter {
            r#type: "all",
            selection: &["genre:16", "runtime:under-90", "technique:stop_motion"],
        },
    },
];

pub fn json() -> serde_json::Value {
    serde_json::json!({ "rows": ROWS })
}
