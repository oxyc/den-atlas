//! Fuzzy, typo-tolerant title search over TMDB's daily ID exports — the index the Den apps query through
//! den-atlas.
//!
//! Portable by construction: no async runtime, no filesystem or network, no global state, deterministic
//! output. The caller downloads and decompresses the exports and feeds the bytes in, so the same crate can
//! run in a server, in a browser as Wasm, or linked into the tvOS app.

mod export;
mod fold;
mod index;

pub use export::ExportScanner;
pub use fold::fold;
pub use index::{Hit, TitleIndex, DEFAULT_MIN_COVERAGE};

/// The two kinds of title TMDB exports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MediaType {
    Movie,
    Tv,
}

impl MediaType {
    /// The Stremio type name.
    pub fn stremio_type(self) -> &'static str {
        match self {
            MediaType::Movie => "movie",
            MediaType::Tv => "series",
        }
    }
}

/// One searchable title — the fields an export line carries.
#[derive(Clone, Debug, PartialEq)]
pub struct TitleRecord {
    pub tmdb_id: u32,
    pub media_type: MediaType,
    pub title: String,
    pub popularity: f64,
}
