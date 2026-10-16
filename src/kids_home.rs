//! The reviewed building blocks of a Kid's Home.
//!
//! This is deliberately data, not client code. Atlas owns the phrases and exact filters so Web and TV can
//! render the same rows, and each source remains one of Atlas's ordinary policy-bound, pageable endpoints.

use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
enum Source {
    Query {
        query: &'static str,
        minimum_score: f32,
        #[serde(skip_serializing_if = "<[TitleRef]>::is_empty")]
        affinity: &'static [TitleRef],
    },
    Franchise {
        r#type: &'static str,
        id: u32,
    },
    Filter {
        r#type: &'static str,
        selection: &'static [&'static str],
        #[serde(skip_serializing_if = "<[TitleRef]>::is_empty")]
        affinity: &'static [TitleRef],
    },
}

#[derive(Serialize)]
struct TitleRef {
    r#type: &'static str,
    id: u32,
}

impl Source {
    const fn query(query: &'static str, minimum_score: f32) -> Self {
        Self::Query { query, minimum_score, affinity: &[] }
    }

    const fn query_with(query: &'static str, minimum_score: f32, affinity: &'static [TitleRef]) -> Self {
        Self::Query { query, minimum_score, affinity }
    }

    const fn filter(r#type: &'static str, selection: &'static [&'static str]) -> Self {
        Self::Filter { r#type, selection, affinity: &[] }
    }

    const fn filter_with(
        r#type: &'static str,
        selection: &'static [&'static str],
        affinity: &'static [TitleRef],
    ) -> Self {
        Self::Filter { r#type, selection, affinity }
    }
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
        id: "bluey",
        title: "Bluey Time!",
        group: "favorite-worlds",
        source: Source::query_with(
            "Bluey",
            0.90,
            &[TitleRef { r#type: "series", id: 82728 }, TitleRef { r#type: "series", id: 256953 }],
        ),
    },
    Row {
        id: "moomins",
        title: "Welcome to Moominvalley",
        group: "favorite-worlds",
        source: Source::query("Moomins", 0.69),
    },
    Row {
        id: "mickey-and-friends",
        title: "Mickey & Friends",
        group: "favorite-worlds",
        source: Source::filter_with(
            "all",
            &["character:mickey-mouse|donald-duck"],
            &[
                TitleRef { r#type: "series", id: 46879 },
                TitleRef { r#type: "series", id: 3934 },
                TitleRef { r#type: "movie", id: 21385 },
            ],
        ),
    },
    Row {
        id: "donald-duck",
        title: "Donald Duck",
        group: "favorite-worlds",
        source: Source::filter_with(
            "all",
            &["character:donald-duck"],
            &[
                TitleRef { r#type: "series", id: 720 },
                TitleRef { r#type: "series", id: 72350 },
                TitleRef { r#type: "movie", id: 74654 },
            ],
        ),
    },
    Row {
        id: "paddington",
        title: "Marmalade with Paddington",
        group: "favorite-worlds",
        source: Source::query("Paddington", 0.93),
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
        source: Source::query("Thomas and Friends", 0.79),
    },
    Row {
        id: "toy-story",
        title: "To Infinity and Beyond!",
        group: "favorite-worlds",
        source: Source::Franchise { r#type: "movie", id: 862 },
    },
    Row {
        id: "how-to-train-your-dragon",
        title: "Dragons & Vikings",
        group: "favorite-worlds",
        source: Source::Franchise { r#type: "movie", id: 10191 },
    },
    Row {
        id: "shrek",
        title: "Welcome to Shrek's Swamp",
        group: "favorite-worlds",
        source: Source::Franchise { r#type: "movie", id: 808 },
    },
    Row {
        id: "ice-age",
        title: "Ice Age Adventures",
        group: "favorite-worlds",
        source: Source::Franchise { r#type: "movie", id: 425 },
    },
    Row {
        id: "despicable-me",
        title: "Minions on the Loose",
        group: "favorite-worlds",
        source: Source::Franchise { r#type: "movie", id: 20352 },
    },
    Row {
        id: "sonic",
        title: "Sonic Speed Zone",
        group: "favorite-worlds",
        source: Source::Franchise { r#type: "movie", id: 454626 },
    },
    Row {
        id: "scooby-doo",
        title: "Scooby-Doo Mysteries",
        group: "favorite-worlds",
        source: Source::Franchise { r#type: "movie", id: 9637 },
    },
    Row {
        id: "harry-potter",
        title: "The Wizarding World",
        group: "favorite-worlds",
        source: Source::Franchise { r#type: "movie", id: 671 },
    },
    Row {
        id: "game-worlds",
        title: "Game Worlds",
        group: "favorite-worlds",
        source: Source::filter("all", &["source:game"]),
    },
    Row {
        id: "ducktales",
        title: "DuckTales Adventures",
        group: "favorite-worlds",
        source: Source::query_with(
            "DuckTales",
            0.70,
            &[
                TitleRef { r#type: "series", id: 720 },
                TitleRef { r#type: "series", id: 72350 },
                TitleRef { r#type: "movie", id: 10837 },
            ],
        ),
    },
    Row {
        id: "looney-tunes",
        title: "Looney Tunes",
        group: "favorite-worlds",
        source: Source::query_with(
            "Looney Tunes classic cartoons",
            0.60,
            &[
                TitleRef { r#type: "series", id: 102321 },
                TitleRef { r#type: "series", id: 65763 },
                TitleRef { r#type: "series", id: 670 },
            ],
        ),
    },
    Row {
        id: "my-little-pony",
        title: "My Little Pony",
        group: "favorite-worlds",
        source: Source::query_with(
            "My Little Pony",
            0.90,
            &[
                TitleRef { r#type: "series", id: 33765 },
                TitleRef { r#type: "series", id: 345 },
                TitleRef { r#type: "movie", id: 335360 },
            ],
        ),
    },
    Row {
        id: "sesame-street",
        title: "Sunny Days on Sesame Street",
        group: "little-ones",
        source: Source::query_with(
            "Sesame Street",
            0.70,
            &[TitleRef { r#type: "series", id: 502 }, TitleRef { r#type: "movie", id: 36536 }],
        ),
    },
    Row {
        id: "peppa-pig",
        title: "Peppa Pig",
        group: "little-ones",
        source: Source::query_with("Peppa Pig", 0.50, &[TitleRef { r#type: "series", id: 12225 }]),
    },
    Row {
        id: "paw-patrol",
        title: "PAW Patrol to the Rescue",
        group: "little-ones",
        source: Source::query_with(
            "Paw Patrol",
            0.80,
            &[TitleRef { r#type: "series", id: 57532 }, TitleRef { r#type: "movie", id: 675445 }],
        ),
    },
    Row {
        id: "curious-george",
        title: "Curious George",
        group: "little-ones",
        source: Source::query_with(
            "Curious George",
            0.80,
            &[TitleRef { r#type: "movie", id: 9975 }, TitleRef { r#type: "series", id: 656 }],
        ),
    },
    Row {
        id: "daniel-tiger",
        title: "Daniel Tiger's Neighborhood",
        group: "little-ones",
        source: Source::query_with("Daniel Tiger", 0.80, &[TitleRef { r#type: "series", id: 40050 }]),
    },
    Row {
        id: "muppet-babies",
        title: "Muppet Babies",
        group: "little-ones",
        source: Source::query_with(
            "Muppet Babies",
            0.80,
            &[TitleRef { r#type: "series", id: 77410 }, TitleRef { r#type: "series", id: 565 }],
        ),
    },
    Row {
        id: "gentle-preschool",
        title: "Gentle Stories for Little Ones",
        group: "little-ones",
        source: Source::query("gentle preschool television", 0.22),
    },
    Row {
        id: "animal-adventures",
        title: "Animal Adventures",
        group: "big-adventures",
        source: Source::query("family adventure with talking animals", 0.4132),
    },
    Row {
        id: "dinosaur-adventures",
        title: "Dino Roars!",
        group: "big-adventures",
        source: Source::query("family film about dinosaurs", 0.4463),
    },
    Row {
        id: "ocean-explorers",
        title: "Ocean Explorers",
        group: "big-adventures",
        source: Source::query("family adventure at sea with sea creatures", 0.5),
    },
    Row {
        id: "fairy-tales",
        title: "Once Upon a Time",
        group: "big-adventures",
        source: Source::query("princesses magic and fairy tales", 0.09),
    },
    Row {
        id: "space-adventures",
        title: "Blast Off!",
        group: "big-adventures",
        source: Source::query("family science fiction adventure in space", 0.49),
    },
    Row {
        id: "mystery-club",
        title: "Mystery Club",
        group: "big-adventures",
        source: Source::query("kids detective mystery adventure", 0.4049),
    },
    Row {
        id: "robot-heroes",
        title: "Robot Heroes",
        group: "big-adventures",
        source: Source::query("family animation about robots", 0.51),
    },
    Row {
        id: "into-the-wild",
        title: "Into the Wild",
        group: "big-adventures",
        source: Source::filter("all", &["setting:wilderness"]),
    },
    Row {
        id: "sing-along",
        title: "Sing Along!",
        group: "fun-and-games",
        source: Source::filter("all", &["genre:10402"]),
    },
    Row {
        id: "game-on",
        title: "Game On!",
        group: "fun-and-games",
        source: Source::query("family sports comedy", 0.4345),
    },
    Row {
        id: "pixar",
        title: "Pixar Favorites",
        group: "movie-magic",
        source: Source::filter("movie", &["studio:Q127552"]),
    },
    Row {
        id: "disney-animation",
        title: "Disney Magic",
        group: "movie-magic",
        source: Source::filter("movie", &["studio:Q1047410"]),
    },
    Row {
        id: "disney-classics",
        title: "Disney Classics",
        group: "time-machine",
        source: Source::filter("movie", &["studio:Q1047410", "decade:1930|1940|1950|1960|1970"]),
    },
    Row {
        id: "studio-ghibli",
        title: "Studio Ghibli Dreams",
        group: "movie-magic",
        source: Source::filter("movie", &["studio:Q182950"]),
    },
    Row {
        id: "aardman",
        title: "Aardman Adventures",
        group: "movie-magic",
        source: Source::filter("movie", &["studio:Q301092"]),
    },
    Row {
        id: "dreamworks-animation",
        title: "DreamWorks Heroes",
        group: "movie-magic",
        source: Source::filter("movie", &["studio:Q500088"]),
    },
    Row {
        id: "drawn-by-hand",
        title: "Drawn by Hand",
        group: "movie-magic",
        source: Source::filter("all", &["audience:made_for_children", "genre:16", "technique:hand_drawn"]),
    },
    Row {
        id: "classic-animation",
        title: "Classic Animation",
        group: "time-machine",
        source: Source::filter("all", &["genre:16", "decade:1920|1930|1940|1950|1960|1970"]),
    },
    Row {
        id: "animated-1990s",
        title: "Back to the '90s",
        group: "time-machine",
        source: Source::filter("movie", &["genre:16", "decade:1990"]),
    },
    Row {
        id: "animated-1980s",
        title: "Back to the '80s",
        group: "time-machine",
        source: Source::filter("movie", &["genre:16", "decade:1980"]),
    },
    Row {
        id: "short-and-sweet",
        title: "Quick Adventures",
        group: "cozy-corner",
        source: Source::filter("all", &["audience:made_for_children", "runtime:under-90"]),
    },
    Row {
        id: "stop-motion",
        title: "Stop-Motion Magic",
        group: "cozy-corner",
        source: Source::filter(
            "all",
            &["audience:made_for_children", "genre:16", "runtime:under-90", "technique:stop_motion"],
        ),
    },
];

pub fn json() -> serde_json::Value {
    serde_json::json!({ "rows": ROWS })
}
