//! Korea Media Rating Board self-rating records for streaming services.
//!
//! The public-data gateway requires an operator key. It is used only on the request and never appears in the
//! persistent cache, logs, observation source URL, or error text.

use super::{name_key, now_secs, Backend, Budget, Identity, Lookup, RuntimeRating};
use crate::maturity::Region;
use async_trait::async_trait;
use den_index::MediaType;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

const ENDPOINT: &str = "https://apis.data.go.kr/B551008/irating_v1/ir_search";
const SOURCE: &str = "https://www.data.go.kr/data/15142196/openapi.do";

pub(super) struct Kmrb {
    client: reqwest::Client,
    key: String,
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
                .build()
                .map_err(|error| error.to_string())?,
            key,
        })
    }

    fn request(&self, title: &str) -> reqwest::RequestBuilder {
        self.client.get(ENDPOINT).query(&[
            ("serviceKey", self.key.as_str()),
            ("pageNo", "1"),
            ("numOfRows", "100"),
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
struct Body {
    #[serde(default)]
    items: Items,
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

    async fn lookup(&self, identity: &Identity, budget: &mut Budget) -> Result<Lookup, String> {
        for candidate in &identity.names {
            if !identity.candidate_is_unique(candidate) {
                continue;
            }
            if !budget.take().await {
                return Ok(Lookup::Deferred);
            }
            let response = self
                .request(candidate)
                .send()
                .await
                .map_err(|error| format!("KMRB ir_search request failed: {}", error.without_url()))?;
            if !response.status().is_success() {
                return Err(format!("KMRB ir_search answered {}", response.status()));
            }
            let body = response.bytes().await.map_err(|error| format!("KMRB ir_search body: {error}"))?;
            let response: Response = quick_xml::de::from_reader(body.as_ref())
                .map_err(|error| format!("KMRB ir_search XML: {error}"))?;
            if response.header.result_code == "03" {
                continue;
            }
            if response.header.result_code != "00" {
                return Err(format!(
                    "KMRB ir_search result {}: {}",
                    response.header.result_code, response.header.result_msg
                ));
            }
            let ratings =
                matching(identity, candidate, response.body.map_or_else(Vec::new, |body| body.items.item));
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
        </items></body></response>"#;
        let parsed: Response = quick_xml::de::from_str(xml).unwrap();
        let rows = parsed.body.unwrap().items.item;
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
