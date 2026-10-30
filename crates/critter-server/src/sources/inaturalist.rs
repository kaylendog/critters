//! iNaturalist research-grade observations, via the public v1 API.

use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;

use super::{Candidate, Species};

/// Observations endpoint.
const API: &str = "https://api.inaturalist.org/v1/observations";
/// Observations per request. 200 is the API maximum.
const PAGE: usize = 200;
/// Hard cap on pages per fetch, so one species cannot run away.
const MAX_PAGES: usize = 10;
/// Photo licences we ask the API for. The allowlist filters again afterwards.
const PHOTO_LICENSES: &str = "cc0,cc-by,cc-by-sa,cc-by-nc,cc-by-nc-sa";

/// Top-level API response.
#[derive(Deserialize)]
struct Response {
    results: Vec<Observation>,
}

/// One observation. Only the fields we use.
#[derive(Deserialize)]
struct Observation {
    uri: String,
    user: Option<User>,
    #[serde(default)]
    photos: Vec<Photo>,
}

/// The observer.
#[derive(Deserialize)]
struct User {
    login: String,
}

/// One photo attached to an observation.
#[derive(Deserialize)]
struct Photo {
    id: u64,
    /// URL of the 75px `square` rendition. Other sizes share the path.
    url: String,
    license_code: Option<String>,
    attribution: Option<String>,
}

/// Fetch up to `limit` photos of the species, most-favourited observations
/// first.
pub async fn fetch(
    http: &reqwest::Client,
    species: &Species,
    limit: usize,
) -> anyhow::Result<Vec<Candidate>> {
    let taxon = species.inat_taxon.to_string();
    let mut out = Vec::new();

    for page in 1..=MAX_PAGES {
        if out.len() >= limit {
            break;
        }
        let response: Response = http
            .get(API)
            .query(&[
                ("taxon_id", taxon.as_str()),
                ("photo_license", PHOTO_LICENSES),
                ("quality_grade", "research"),
                ("photos", "true"),
                ("order_by", "votes"),
                ("order", "desc"),
                ("per_page", &PAGE.to_string()),
                ("page", &page.to_string()),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("decoding inaturalist response")?;

        let last_page = response.results.len() < PAGE;
        out.extend(response.results.into_iter().filter_map(|obs| candidate(species, obs)));
        if last_page {
            break;
        }
        // iNaturalist asks for no more than one request per second.
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    out.truncate(limit);
    Ok(out)
}

/// Convert one observation into a candidate from its first licensed photo.
/// One photo per observation keeps the pool varied.
fn candidate(species: &Species, obs: Observation) -> Option<Candidate> {
    let photo = obs.photos.into_iter().find(|photo| photo.license_code.is_some())?;
    let code = photo.license_code?;
    let login = obs.user.map(|user| user.login);

    Some(Candidate {
        species: species.slug,
        source: "inaturalist",
        source_id: photo.id.to_string(),
        image_url: photo.url.replacen("/square.", "/large.", 1),
        source_url: obs.uri,
        author: photo.attribution.as_deref().and_then(author_from_attribution).or(login.clone()),
        author_url: login.map(|login| format!("https://www.inaturalist.org/people/{login}")),
        license: normalise_license(&code),
        license_url: None,
        width: None,
        height: None,
    })
}

/// Normalise a `license_code` such as `cc-by-nc` or `cc0`. iNaturalist does
/// not report the version, so most codes stay unversioned.
fn normalise_license(code: &str) -> String {
    match code {
        "cc0" => "CC0-1.0".to_string(),
        other => other.to_ascii_uppercase(),
    }
}

/// Pull the name out of an attribution string like
/// `(c) Jane Doe, some rights reserved (CC BY-NC)`.
fn author_from_attribution(attribution: &str) -> Option<String> {
    let name = attribution.strip_prefix("(c) ")?;
    let name = name.split(", some rights reserved").next()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_candidate_from_observation() {
        let json = r#"{"results":[{"uri":"https://www.inaturalist.org/observations/1","user":{"login":"andrewdubs1"},
          "photos":[{"id":99,"url":"https://inaturalist-open-data.s3.amazonaws.com/photos/99/square.jpg",
            "license_code":"cc-by-nc","attribution":"(c) andrewdubs1, some rights reserved (CC BY-NC)"}]}]}"#;
        let response: Response = serde_json::from_str(json).unwrap();
        let species = &super::super::SPECIES[1];
        let found = candidate(species, response.results.into_iter().next().unwrap()).unwrap();
        assert_eq!(found.source_id, "99");
        assert_eq!(
            found.image_url,
            "https://inaturalist-open-data.s3.amazonaws.com/photos/99/large.jpg"
        );
        assert_eq!(found.license, "CC-BY-NC");
        assert_eq!(found.author.as_deref(), Some("andrewdubs1"));
        assert_eq!(
            found.author_url.as_deref(),
            Some("https://www.inaturalist.org/people/andrewdubs1")
        );
    }

    #[test]
    fn falls_back_to_login_without_attribution_name() {
        assert_eq!(author_from_attribution("no rights reserved"), None);
        assert_eq!(
            author_from_attribution("(c) Jane Doe, some rights reserved (CC BY)"),
            Some("Jane Doe".into())
        );
    }
}
