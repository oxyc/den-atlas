//! The Den dataset index — derived labels plus int8 vectors — and the questions the Den apps ask of it:
//! label rows, nearest neighbours, taste centroids, More Like This. A port of the tvOS app's
//! `SubgenreIndex`, answering the same way.
//!
//! Portable by construction: no async runtime, no filesystem or network, no global state, deterministic
//! output. The caller reads the dataset blobs and passes the bytes in, so the same crate can run in a
//! server, in a browser as Wasm, or linked into the tvOS app.

mod facets;
mod index;
mod similar;

pub use facets::{FacetIndex, FacetQuery, TitleFacets};
pub use index::{Index, Labels, LoadError, Neighbor, DISPLAY_CONFIDENCE_FLOOR};
pub use similar::more_like_this;

/// The two kinds of title in the index (`"movie"` / `"tv"` in the labels blob).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MediaType {
    Movie,
    Tv,
}

impl MediaType {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "movie" => Some(MediaType::Movie),
            "tv" => Some(MediaType::Tv),
            _ => None,
        }
    }
}
