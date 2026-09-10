//! Streaming scan of TMDB's daily ID exports — gzipped NDJSON, about 1.3 M lines and 140 MB inflated — into
//! the most popular N titles. It takes decompressed bytes in any chunking and never holds the inflated
//! corpus. Mirrors the tvOS app's `TitleExport`: popularity is read first, so a line that can't beat the
//! titles kept costs nothing more; adult movies are skipped; the original title is the one indexed.

use crate::{MediaType, TitleRecord};
use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

/// Keeps the most popular titles from every export fed through it.
pub struct ExportScanner {
    heap: BinaryHeap<Reverse<ByPopularity>>,
    capacity: usize,
    media_type: MediaType,
    carry: Vec<u8>,
}

impl ExportScanner {
    /// Keep the `capacity` most popular titles across every export fed in. Starts on the movie export.
    pub fn new(capacity: usize) -> Self {
        ExportScanner {
            heap: BinaryHeap::with_capacity(capacity.min(1 << 17)),
            capacity,
            media_type: MediaType::Movie,
            carry: Vec::new(),
        }
    }

    /// Switch to the next export. Movies and series come in separate files, and a last line without a
    /// trailing newline still belongs to the file it came from.
    pub fn start(&mut self, media_type: MediaType) {
        self.flush_line();
        self.media_type = media_type;
    }

    /// Feed decompressed export bytes; lines may straddle calls.
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut rest = bytes;
        while let Some(newline) = rest.iter().position(|&b| b == b'\n') {
            if self.carry.is_empty() {
                self.scan_line(&rest[..newline]);
            } else {
                self.carry.extend_from_slice(&rest[..newline]);
                let mut line = std::mem::take(&mut self.carry);
                self.scan_line(&line);
                line.clear();
                self.carry = line;
            }
            rest = &rest[newline + 1..];
        }
        self.carry.extend_from_slice(rest);
    }

    /// Everything kept, most popular first; ties in a fixed order, so the same exports give the same list.
    pub fn finish(mut self) -> Vec<TitleRecord> {
        self.flush_line();
        let mut records: Vec<TitleRecord> = self.heap.into_iter().map(|Reverse(ByPopularity(r))| r).collect();
        records.sort_by(|a, b| {
            b.popularity
                .total_cmp(&a.popularity)
                .then(a.media_type.cmp(&b.media_type))
                .then(a.tmdb_id.cmp(&b.tmdb_id))
        });
        records
    }

    fn flush_line(&mut self) {
        if !self.carry.is_empty() {
            let line = std::mem::take(&mut self.carry);
            self.scan_line(&line);
        }
    }

    /// One export line into the heap. The order is deliberate: popularity, then the heap check, then the
    /// adult filter, id and title — so a line that can't beat the heap costs only the popularity scan.
    fn scan_line(&mut self, line: &[u8]) {
        if self.capacity == 0 {
            return;
        }
        let Some(at) = find_after(line, b"\"popularity\"") else { return };
        let Some(popularity) = parse_number(skip_colon(line, at)) else { return };
        if self.heap.len() >= self.capacity {
            if let Some(Reverse(weakest)) = self.heap.peek() {
                if popularity <= weakest.0.popularity {
                    return;
                }
            }
        }
        if self.media_type == MediaType::Movie && find_after(line, b"\"adult\":true").is_some() {
            return;
        }
        let Some(at) = find_after(line, b"\"id\":") else { return };
        let Some(tmdb_id) = parse_u32(skip_spaces(line, at)) else { return };
        let key: &[u8] = match self.media_type {
            MediaType::Movie => b"\"original_title\":\"",
            MediaType::Tv => b"\"original_name\":\"",
        };
        let Some(at) = find_after(line, key) else { return };
        let Some(title) = json_string(&line[at..]).filter(|t| !t.is_empty()) else { return };
        let record = TitleRecord { tmdb_id, media_type: self.media_type, title, popularity };
        if self.heap.len() >= self.capacity {
            self.heap.pop();
        }
        self.heap.push(Reverse(ByPopularity(record)));
    }
}

/// Lets a gzip decoder that writes (e.g. `flate2::write::GzDecoder`) decompress straight into the scanner.
impl std::io::Write for ExportScanner {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.feed(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Orders records by popularity alone, for the heap.
struct ByPopularity(TitleRecord);

impl PartialEq for ByPopularity {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for ByPopularity {}
impl PartialOrd for ByPopularity {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for ByPopularity {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.popularity.total_cmp(&other.0.popularity)
    }
}

/// The index just past the first `needle` in `hay`.
fn find_after(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle).map(|i| i + needle.len())
}

fn skip_spaces(line: &[u8], mut at: usize) -> &[u8] {
    while line.get(at) == Some(&b' ') {
        at += 1;
    }
    &line[at.min(line.len())..]
}

fn skip_colon(line: &[u8], at: usize) -> &[u8] {
    let rest = skip_spaces(line, at);
    let rest = rest.strip_prefix(b":").unwrap_or(rest);
    let spaces = rest.iter().take_while(|&&b| b == b' ').count();
    &rest[spaces..]
}

/// A JSON number at the start of `s`.
fn parse_number(s: &[u8]) -> Option<f64> {
    let len = s
        .iter()
        .take_while(|&&b| b.is_ascii_digit() || matches!(b, b'-' | b'+' | b'.' | b'e' | b'E'))
        .count();
    std::str::from_utf8(&s[..len]).ok()?.parse().ok()
}

fn parse_u32(s: &[u8]) -> Option<u32> {
    let len = s.iter().take_while(|b| b.is_ascii_digit()).count();
    std::str::from_utf8(&s[..len]).ok()?.parse().ok()
}

/// A JSON string's value, from just after its opening quote up to the closing one, escapes decoded. `None`
/// when the line ends first.
fn json_string(s: &[u8]) -> Option<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'"' => return Some(String::from_utf8_lossy(&out).into_owned()),
            b'\\' => {
                let escape = *s.get(i + 1)?;
                i += 2;
                match escape {
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'r' => out.push(b'\r'),
                    b'b' => out.push(0x08),
                    b'f' => out.push(0x0C),
                    b'u' => {
                        let high = hex4(s.get(i..i + 4)?)?;
                        i += 4;
                        let mut code = u32::from(high);
                        // A high surrogate pairs with the `\uXXXX` low surrogate right after it.
                        if (0xD800..0xDC00).contains(&high) && s.get(i..i + 2) == Some(b"\\u") {
                            if let Some(low) = s.get(i + 2..i + 6).and_then(hex4) {
                                if (0xDC00..0xE000).contains(&low) {
                                    code = 0x10000 + ((code - 0xD800) << 10) + (u32::from(low) - 0xDC00);
                                    i += 6;
                                }
                            }
                        }
                        let c = char::from_u32(code).unwrap_or('\u{FFFD}');
                        out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                    }
                    // `\"`, `\\`, `\/`, and anything unknown: the character itself.
                    other => out.push(other),
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    None
}

fn hex4(b: &[u8]) -> Option<u16> {
    b.iter().try_fold(0u16, |acc, &c| Some((acc << 4) | (c as char).to_digit(16)? as u16))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn movie(id: u32, title: &str, popularity: f64) -> String {
        format!(
            r#"{{"adult":false,"id":{id},"original_title":"{title}","popularity":{popularity},"video":false}}"#
        )
    }

    fn scan(capacity: usize, movies: &str, series: &str) -> Vec<TitleRecord> {
        let mut scanner = ExportScanner::new(capacity);
        scanner.feed(movies.as_bytes());
        scanner.start(MediaType::Tv);
        scanner.feed(series.as_bytes());
        scanner.finish()
    }

    #[test]
    fn keeps_the_most_popular_across_both_exports() {
        let movies = [movie(1, "Low", 1.0), movie(2, "High", 90.0), movie(3, "Mid", 10.0)].join("\n");
        let series = r#"{"id":7,"original_name":"Show","popularity":50.5}"#;
        let got = scan(3, &movies, series);
        let ids: Vec<(u32, MediaType)> = got.iter().map(|r| (r.tmdb_id, r.media_type)).collect();
        assert_eq!(ids, vec![(2, MediaType::Movie), (7, MediaType::Tv), (3, MediaType::Movie)]);
        assert_eq!(got[1].title, "Show");
    }

    #[test]
    fn a_line_split_across_chunks_is_read_whole() {
        let line = movie(42, "The Matrix", 80.0) + "\n";
        let mut scanner = ExportScanner::new(10);
        let (a, b) = line.as_bytes().split_at(17);
        scanner.feed(a);
        scanner.feed(b);
        let got = scanner.finish();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].title, "The Matrix");
    }

    #[test]
    fn a_final_line_without_a_newline_still_counts_for_its_own_file() {
        let got =
            scan(10, &movie(1, "Last Movie", 5.0), r#"{"id":2,"original_name":"Last Show","popularity":4}"#);
        assert_eq!(got[0].media_type, MediaType::Movie);
        assert_eq!(got[1].media_type, MediaType::Tv);
    }

    #[test]
    fn skips_adult_movies_and_malformed_lines() {
        let movies = [
            r#"{"adult":true,"id":1,"original_title":"Nope","popularity":99}"#.to_owned(),
            r#"{"adult":false,"id":2,"popularity":50,"original_title":"Unterminated"#.to_owned(),
            r#"not json at all"#.to_owned(),
            movie(3, "Kept", 1.0),
        ]
        .join("\n");
        let got = scan(10, &movies, "");
        assert_eq!(got.iter().map(|r| r.tmdb_id).collect::<Vec<_>>(), vec![3]);
    }

    #[test]
    fn decodes_escapes_and_surrogate_pairs() {
        // The clapper board as a JSON surrogate pair, assembled so the source holds the escape, not the glyph.
        let pair = ["\\", "u", "d83c", "\\", "u", "dfac"].concat();
        let line = format!(r#"{{"id":1,"original_title":"Amélie \"Q\" {pair}","popularity":3}}"#);
        assert_eq!(scan(1, &line, "")[0].title, "Amélie \"Q\" \u{1F3AC}");
    }
}
