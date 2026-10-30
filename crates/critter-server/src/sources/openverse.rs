//! Openverse, the Creative Commons image search run by WordPress. It
//! aggregates Flickr, Wikimedia and others behind one keyless API.

use anyhow::Context;
use serde::Deserialize;

use super::{Candidate, Species};

/// Images search endpoint.
const API: &str = "https://api.openverse.org/v1/images/";
/// Results per request. 20 is the anonymous maximum.
const PAGE: usize = 20;
/// Hard cap on pages per fetch. Anonymous clients get about 100 requests a
/// day, and ingest runs every few hours for every species, so stay small.
// ponytail: register an OAuth client and send a bearer token if 60 images
// per species per ingest is too few.
const MAX_PAGES: usize = 3;
/// Licences to ask for. The allowlist filters again afterwards.
const LICENSES: &str = "by,by-sa,by-nc,by-nc-sa,cc0,pdm";

/// Top-level API response.
#[derive(Deserialize)]
struct Response {
    page_count: usize,
    results: Vec<Image>,
}

/// One result. Only the fields we use.
#[derive(Deserialize)]
struct Image {
    id: String,
    /// Direct URL of the full-size image.
    url: String,
    creator: Option<String>,
    creator_url: Option<String>,
    /// Lowercase licence slug, e.g. `by-nc-sa` or `cc0`.
    license: String,
    license_version: Option<String>,
    license_url: Option<String>,
    /// The page on the original site, e.g. a Flickr photo page.
    foreign_landing_url: String,
    width: Option<i32>,
    height: Option<i32>,
}

/// Fetch up to `limit` photos matching the species' Openverse query.
pub async fn fetch(
    http: &reqwest::Client,
    species: &Species,
    limit: usize,
) -> anyhow::Result<Vec<Candidate>> {
    let Some(text) = species.openverse_query else { return Ok(Vec::new()) };
    let mut out = Vec::new();

    for page in 1..=MAX_PAGES {
        if out.len() >= limit {
            break;
        }
        let response: Response = http
            .get(API)
            .query(&[
                ("q", text),
                ("license", LICENSES),
                ("mature", "false"),
                ("page_size", &PAGE.to_string()),
                ("page", &page.to_string()),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("decoding openverse response")?;

        let last_page = page >= response.page_count;
        out.extend(response.results.into_iter().filter_map(|image| candidate(species, image)));
        if last_page {
            break;
        }
    }

    out.truncate(limit);
    Ok(out)
}

/// Convert one result into a candidate.
fn candidate(species: &Species, image: Image) -> Option<Candidate> {
    Some(Candidate {
        species: species.slug,
        source: "openverse",
        source_id: image.id,
        image_url: image.url,
        source_url: image.foreign_landing_url,
        author: image.creator.filter(|name| !name.is_empty()),
        author_url: image.creator_url.filter(|url| !url.is_empty()),
        license: normalise_license(&image.license, image.license_version.as_deref()),
        license_url: image.license_url,
        width: image.width,
        height: image.height,
    })
}

/// `by-nc-sa` + `4.0` becomes `CC-BY-NC-SA-4.0`. `cc0` and `pdm` map to our
/// public domain codes.
fn normalise_license(slug: &str, version: Option<&str>) -> String {
    match slug {
        "cc0" => "CC0-1.0".to_string(),
        "pdm" => "PD".to_string(),
        other => {
            let family = format!("CC-{}", other.to_ascii_uppercase());
            match version.filter(|v| !v.is_empty()) {
                Some(version) => format!("{family}-{version}"),
                None => family,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_candidate_from_result() {
        let json = r#"{"result_count":1,"page_count":1,"page_size":20,"page":1,"results":[
          {"id":"abc","title":"Bernese","url":"https://live.staticflickr.com/1_b.jpg","creator":"Jane",
           "creator_url":"https://www.flickr.com/people/12@N00","license":"by-nc","license_version":"2.0",
           "license_url":"https://creativecommons.org/licenses/by-nc/2.0/","foreign_landing_url":"https://www.flickr.com/photos/12@N00/1",
           "source":"flickr","provider":"flickr","width":1024,"height":683}]}"#;
        let response: Response = serde_json::from_str(json).unwrap();
        let species = super::super::species("bernese").unwrap();
        let found = candidate(species, response.results.into_iter().next().unwrap()).unwrap();
        assert_eq!(found.source_id, "abc");
        assert_eq!(found.license, "CC-BY-NC-2.0");
        assert_eq!(found.source_url, "https://www.flickr.com/photos/12@N00/1");
        assert_eq!(found.author.as_deref(), Some("Jane"));
    }

    #[test]
    fn normalises_licence_slugs() {
        assert_eq!(normalise_license("by-sa", Some("4.0")), "CC-BY-SA-4.0");
        assert_eq!(normalise_license("by", None), "CC-BY");
        assert_eq!(normalise_license("cc0", Some("1.0")), "CC0-1.0");
        assert_eq!(normalise_license("pdm", None), "PD");
    }
}
