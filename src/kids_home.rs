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
        id: "moomins",
        title: "Welcome to Moominvalley",
        group: "favorite-worlds",
        source: Source::Query { query: "Moomins", minimum_score: 0.69 },
    },
    Row {
        id: "paddington",
        title: "Marmalade with Paddington",
        group: "favorite-worlds",
        source: Source::Query { query: "Paddington", minimum_score: 0.93 },
    },
    Row {
        id: "winnie-the-pooh",
        title: "The Hundred Acre Wood",
        group: "favorite-worlds",
        source: Source::Franchise { r#type: "movie", id: 250480 },
    },
    Row {
        id: "barbie",
        title: "Barbie Adventures",
        group: "favorite-worlds",
        source: Source::Franchise { r#type: "movie", id: 21705 },
    },
    Row {
        id: "pokemon",
        title: "Pokémon Adventures",
        group: "favorite-worlds",
        source: Source::Franchise { r#type: "series", id: 60572 },
    },
    Row {
        id: "cars",
        title: "Race into Radiator Springs",
        group: "favorite-worlds",
        source: Source::Franchise { r#type: "movie", id: 920 },
    },
    Row {
        id: "thomas-and-friends",
        title: "All Aboard with Thomas",
        group: "favorite-worlds",
        source: Source::Query { query: "Thomas and Friends", minimum_score: 0.79 },
    },
    Row {
        id: "animal-adventures",
        title: "Animal Adventures",
        group: "big-adventures",
        source: Source::Query { query: "family adventure with talking animals", minimum_score: 0.4132 },
    },
    Row {
        id: "dinosaur-adventures",
        title: "Dino Roars!",
        group: "big-adventures",
        source: Source::Query { query: "family film about dinosaurs", minimum_score: 0.4463 },
    },
    Row {
        id: "ocean-explorers",
        title: "Ocean Explorers",
        group: "big-adventures",
        source: Source::Query { query: "family adventure at sea with sea creatures", minimum_score: 0.5 },
    },
    Row {
        id: "fairy-tales",
        title: "Once Upon a Time",
        group: "big-adventures",
        source: Source::Query { query: "princesses magic and fairy tales", minimum_score: 0.09 },
    },
    Row {
        id: "space-adventures",
        title: "Blast Off!",
        group: "big-adventures",
        source: Source::Query { query: "family science fiction adventure in space", minimum_score: 0.49 },
    },
    Row {
        id: "mystery-club",
        title: "Mystery Club",
        group: "big-adventures",
        source: Source::Query { query: "kids detective mystery adventure", minimum_score: 0.4049 },
    },
    Row {
        id: "robot-heroes",
        title: "Robot Heroes",
        group: "big-adventures",
        source: Source::Query { query: "family animation about robots", minimum_score: 0.51 },
    },
    Row {
        id: "pixar",
        title: "Pixar Favorites",
        group: "movie-magic",
        source: Source::Filter { r#type: "movie", selection: &["studio:Q127552"] },
    },
    Row {
        id: "disney-animation",
        title: "Disney Magic",
        group: "movie-magic",
        source: Source::Filter { r#type: "movie", selection: &["studio:Q1047410"] },
    },
    Row {
        id: "studio-ghibli",
        title: "Studio Ghibli Dreams",
        group: "movie-magic",
        source: Source::Filter { r#type: "movie", selection: &["studio:Q182950"] },
    },
    Row {
        id: "aardman",
        title: "Aardman Adventures",
        group: "movie-magic",
        source: Source::Filter { r#type: "movie", selection: &["studio:Q301092"] },
    },
    Row {
        id: "dreamworks-animation",
        title: "DreamWorks Heroes",
        group: "movie-magic",
        source: Source::Filter { r#type: "movie", selection: &["studio:Q500088"] },
    },
    Row {
        id: "drawn-by-hand",
        title: "Drawn by Hand",
        group: "movie-magic",
        source: Source::Filter {
            r#type: "all",
            selection: &["audience:made_for_children", "genre:16", "technique:hand_drawn"],
        },
    },
    Row {
        id: "animated-1990s",
        title: "Back to the '90s",
        group: "time-machine",
        source: Source::Filter { r#type: "movie", selection: &["genre:16", "decade:1990"] },
    },
    Row {
        id: "animated-1980s",
        title: "Back to the '80s",
        group: "time-machine",
        source: Source::Filter { r#type: "movie", selection: &["genre:16", "decade:1980"] },
    },
    Row {
        id: "short-and-sweet",
        title: "Quick Adventures",
        group: "cozy-corner",
        source: Source::Filter {
            r#type: "all",
            selection: &["audience:made_for_children", "runtime:under-90"],
        },
    },
    Row {
        id: "stop-motion",
        title: "Stop-Motion Magic",
        group: "cozy-corner",
        source: Source::Filter {
            r#type: "all",
            selection: &[
                "audience:made_for_children",
                "genre:16",
                "runtime:under-90",
                "technique:stop_motion",
            ],
        },
    },
];

pub fn json() -> serde_json::Value {
    serde_json::json!({ "rows": ROWS })
}
