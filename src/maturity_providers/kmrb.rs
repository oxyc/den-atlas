//! Korea Media Rating Board self-rating records for streaming services.
//!
//! The public-data gateway requires an operator key. It is used only on the request and never appears in the
//! persistent cache, logs, observation source URL, or error text.

use super::{
    name_key, now_secs, read_limited, requested_wait, user_agent, Backend, Budget, Identity, Lookup,
    RuntimeRating,
};
use crate::maturity::Region;
use async_trait::async_trait;
use den_index::MediaType;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

const ENDPOINT: &str = "https://apis.data.go.kr/B551008/irating_v1/ir_search";
const SOURCE: &str = "https://www.data.go.kr/data/15142196/openapi.do";
const PAGE_SIZE: usize = 100;
const MAX_PAGE_BODY: usize = 2 * 1024 * 1024;
const MAX_RESULTS: usize = 10_000;

pub(super) struct Kmrb {
    client: reqwest::Client,
    key: String,
    backoff: crate::util::Backoff,
}

impl Kmrb {
    pub(super) fn from_env() -> Result<Self, String> {
        let key = std::env::var("KMRB_SERVICE_KEY")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "kmrb requires KMRB_SERVICE_KEY from data.go.kr".to_owned())?;
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                // The API key is a query parameter; never forward it to a redirected origin.
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(user_agent())
                .build()
                .map_err(|error| error.to_string())?,
            key,
            backoff: crate::util::Backoff::new(Duration::from_secs(10), Duration::from_secs(3_600)),
        })
    }

    fn request(&self, title: &str, page: usize) -> reqwest::RequestBuilder {
        let page = page.to_string();
        let rows = PAGE_SIZE.to_string();
        self.client.get(ENDPOINT).query(&[
            ("serviceKey", self.key.as_str()),
            ("pageNo", page.as_str()),
            ("numOfRows", rows.as_str()),
            ("title", title),
        ])
    }
}

#[derive(Debug, Deserialize)]
struct Response {
    header: Header,
    body: Option<Body>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Header {
    result_code: String,
    result_msg: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Body {
    #[serde(default)]
    items: Items,
    #[serde(default)]
    total_count: usize,
}

#[derive(Debug, Default, Deserialize)]
struct Items {
    #[serde(default)]
    item: Vec<KmrbRow>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KmrbRow {
    #[serde(default)]
    wrks_nm: String,
    #[serde(default)]
    oriwk_ttl: String,
    #[serde(default)]
    fbctn_yr: String,
    #[serde(default)]
    vid_knd_nm: String,
    #[serde(default)]
    rt_no: String,
    #[serde(default)]
    grd_nm: String,
    #[serde(default)]
    cn_info_indcrt_nm1: String,
    #[serde(default)]
    cn_info_indcrt_nm2: String,
    #[serde(default)]
    cn_info_indcrt_nm3: String,
}

fn text(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty() && value != "-").then_some(value)
}

fn row_media(value: &str) -> Option<MediaType> {
    let folded = value.to_lowercase();
    if folded.contains("영화") || folded.contains("movie") || folded.contains("film") {
        Some(MediaType::Movie)
    } else if folded.contains("시리즈")
        || folded.contains("방송")
        || folded.contains("텔레비전")
        || folded.contains("series")
        || folded.contains("television")
        || folded == "tv"
    {
        Some(MediaType::Tv)
    } else {
        None
    }
}

fn normalized_age(value: &str) -> Option<u8> {
    let value = value.trim();
    let compact = name_key(value);
    if value.contains("전체관람가") || compact == "all" {
        Some(0)
    } else if value.contains("7세") || compact.starts_with('7') {
        Some(7)
    } else if value.contains("12세") || compact.starts_with("12") {
        Some(12)
    } else if value.contains("15세") || compact.starts_with("15") {
        Some(15)
    } else if value.contains("청소년관람불가") || compact.contains("restricted") || compact.starts_with("18")
    {
        Some(18)
    } else {
        None
    }
}

fn matching(identity: &Identity, candidate: &str, rows: Vec<KmrbRow>) -> Vec<RuntimeRating> {
    if !identity.candidate_is_unique(candidate) {
        return Vec::new();
    }
    let wanted = name_key(candidate);
    let fetched_at = now_secs();
    let mut ratings = HashMap::new();
    for row in rows {
        if row_media(&row.vid_knd_nm) != Some(identity.key.0) {
            continue;
        }
        if ![row.wrks_nm.as_str(), row.oriwk_ttl.as_str()]
            .into_iter()
            .filter_map(text)
            .any(|title| name_key(title) == wanted)
        {
            continue;
        }
        if let (Some(expected), Some(actual)) =
            (identity.year, text(&row.fbctn_yr).and_then(|year| year.parse::<i64>().ok()))
        {
            if expected != actual {
                continue;
            }
        }
        let Some(age) = normalized_age(&row.grd_nm) else { continue };
        let Some(certificate) = text(&row.rt_no) else { continue };
        let mut warnings = [row.cn_info_indcrt_nm1, row.cn_info_indcrt_nm2, row.cn_info_indcrt_nm3]
            .into_iter()
            .filter_map(|warning| text(&warning).map(str::to_owned))
            .collect::<Vec<_>>();
        warnings.sort();
        warnings.dedup();
        ratings.insert(
            certificate.to_owned(),
            RuntimeRating {
                authority: "KMRB".to_owned(),
                authority_id: certificate.to_owned(),
                jurisdiction: "KR".to_owned(),
                rating: row.grd_nm,
                normalized_age: age,
                warnings,
                source_url: SOURCE.to_owned(),
                fetched_at,
            },
        );
    }
    let mut ratings = ratings.into_values().collect::<Vec<_>>();
    ratings.sort_by(|a, b| a.authority_id.cmp(&b.authority_id));
    ratings
}

#[async_trait]
impl Backend for Kmrb {
    fn authority(&self) -> &'static str {
        "KMRB"
    }

    fn media_types(&self) -> &'static [MediaType] {
        &[MediaType::Movie, MediaType::Tv]
    }

    fn applies_to(&self, _viewer_region: Option<Region>) -> bool {
        true
    }

    fn retry_in(&self) -> Option<Duration> {
        self.backoff.paused()
    }

    async fn lookup(&self, identity: &Identity, budget: &mut Budget) -> Result<Lookup, String> {
        if self.backoff.paused().is_some() {
            return Ok(Lookup::Deferred);
        }
        for candidate in &identity.names {
            if !identity.candidate_is_unique(candidate) {
                continue;
            }
            let mut rows = Vec::new();
            let mut page = 1;
            loop {
                if self.backoff.paused().is_some() {
                    return Ok(Lookup::Deferred);
                }
                if !budget.take().await {
                    return Ok(Lookup::Deferred);
                }
                let response = self.request(candidate, page).send().await.map_err(|error| {
                    self.backoff.refused(None);
                    format!("KMRB ir_search request failed: {}", error.without_url())
                })?;
                if !response.status().is_success() {
                    let status = response.status();
                    if crate::util::backs_off(status) {
                        self.backoff.refused(requested_wait(response.headers()));
                    } else {
                        self.backoff.answered();
                    }
                    return Err(format!("KMRB ir_search answered {status}"));
                }
                let exhausted_for = crate::util::exhausted_for(response.headers());
                let body = read_limited(response, MAX_PAGE_BODY, "KMRB").await.inspect_err(|_error| {
                    self.backoff.refused(None);
                })?;
                let response: Response = quick_xml::de::from_reader(body.as_ref()).map_err(|error| {
                    self.backoff.refused(None);
                    format!("KMRB ir_search XML: {error}")
                })?;
                self.backoff.answered();
                if let Some(wait) = exhausted_for {
                    self.backoff.refused(Some(wait));
                }
                if response.header.result_code == "03" {
                    break;
                }
                if response.header.result_code != "00" {
                    self.backoff.refused(None);
                    return Err(format!(
                        "KMRB ir_search result {}: {}",
                        response.header.result_code, response.header.result_msg
                    ));
                }
                let body = response.body.unwrap_or(Body { items: Items::default(), total_count: 0 });
                let received = body.items.item.len();
                rows.extend(body.items.item);
                if rows.len() >= MAX_RESULTS {
                    return Err(format!("KMRB ir_search exceeded the {MAX_RESULTS}-row safety limit"));
                }
                if body.total_count > 0 && rows.len() < body.total_count && received == 0 {
                    return Err(format!(
                        "KMRB ir_search stopped after {} of {} rows; refusing a partial result",
                        rows.len(),
                        body.total_count
                    ));
                }
                if (body.total_count > 0 && rows.len() >= body.total_count)
                    || (body.total_count == 0 && received < PAGE_SIZE)
                {
                    break;
                }
                page += 1;
            }
            let ratings = matching(identity, candidate, rows);
            if !ratings.is_empty() {
                return Ok(Lookup::Complete(ratings));
            }
        }
        Ok(Lookup::Complete(Vec::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Arc;

    fn identity(media: MediaType, name: &str) -> Identity {
        let key = (media, 1);
        Identity {
            key,
            year: Some(2024),
            names: vec![name.to_owned()],
            unique: Arc::new(HashMap::from([(name_key(name), HashSet::from([key]))])),
        }
    }

    #[test]
    fn parses_gateway_xml_and_matches_movie_and_tv_records_separately() {
        let xml = r#"<response><header><resultCode>00</resultCode><resultMsg>NORMAL_SERVICE</resultMsg></header><body><items>
          <item><wrksNm>Shared</wrksNm><oriwkTtl>Shared</oriwkTtl><fbctnYr>2024</fbctnYr><vidKndNm>영화</vidKndNm><rtNo>2024-NFLX-1</rtNo><grdNm>12세이상관람가</grdNm><cnInfoIndcrtNm1>폭력성</cnInfoIndcrtNm1></item>
          <item><wrksNm>Shared</wrksNm><fbctnYr>2024</fbctnYr><vidKndNm>시리즈</vidKndNm><rtNo>2024-DISN-2</rtNo><grdNm>15세이상관람가</grdNm></item>
        </items><totalCount>102</totalCount></body></response>"#;
        let parsed: Response = quick_xml::de::from_str(xml).unwrap();
        let body = parsed.body.unwrap();
        assert_eq!(body.total_count, 102);
        let rows = body.items.item;
        let movie = matching(&identity(MediaType::Movie, "Shared"), "Shared", rows.clone());
        let tv = matching(&identity(MediaType::Tv, "Shared"), "Shared", rows);
        assert_eq!(movie[0].normalized_age, 12);
        assert_eq!(movie[0].warnings, ["폭력성"]);
        assert_eq!(tv[0].normalized_age, 15);
    }

    #[test]
    fn maps_korean_rating_names() {
        assert_eq!(normalized_age("전체관람가"), Some(0));
        assert_eq!(normalized_age("7세이상관람가"), Some(7));
        assert_eq!(normalized_age("12세이상관람가"), Some(12));
        assert_eq!(normalized_age("15세이상관람가"), Some(15));
        assert_eq!(normalized_age("청소년관람불가"), Some(18));
    }
}
