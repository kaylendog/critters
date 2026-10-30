//! Wikimedia Commons, via the MediaWiki API's search generator.

use std::collections::HashMap;

use anyhow::Context;
use serde::Deserialize;

use super::{Candidate, Species};

/// MediaWiki API endpoint.
const API: &str = "https://commons.wikimedia.org/w/api.php";
/// Search results per request. Anonymous clients are capped at 50.
const PAGE: usize = 50;
/// Width of the thumbnail rendition we hand out, in pixels.
const THUMB_WIDTH: &str = "1280";

/// Top-level API response.
#[derive(Deserialize)]
struct Response {
    #[serde(rename = "continue")]
    cont: Option<Continue>,
    query: Option<Query>,
}

/// Continuation token for the next page of search results.
#[derive(Deserialize)]
struct Continue {
    gsroffset: usize,
}

/// The `query` block of the response.
#[derive(Deserialize)]
struct Query {
    pages: Vec<Page>,
}

/// One file page, with its image info attached.
#[derive(Deserialize)]
struct Page {
    pageid: u64,
    imageinfo: Option<Vec<ImageInfo>>,
}

/// Rendition URLs and metadata for a file.
#[derive(Deserialize)]
struct ImageInfo {
    thumburl: Option<String>,
    thumbwidth: Option<i32>,
    thumbheight: Option<i32>,
    descriptionurl: String,
    mime: String,
    #[serde(default)]
    extmetadata: HashMap<String, MetaValue>,
}

/// One `extmetadata` entry. Values are strings, often containing HTML.
#[derive(Deserialize)]
struct MetaValue {
    value: String,
}

/// Fetch up to `limit` bitmap images matching the species' Commons query.
pub async fn fetch(
    http: &reqwest::Client,
    species: &Species,
    limit: usize,
) -> anyhow::Result<Vec<Candidate>> {
    let search = format!("{} filetype:bitmap", species.commons_query);
    let mut out = Vec::new();
    let mut offset = 0;

    // Page through search results until we have enough or they run out.
    while out.len() < limit {
        let response: Response = http
            .get(API)
            .query(&[
                ("action", "query"),
                ("format", "json"),
                ("formatversion", "2"),
                ("generator", "search"),
                ("gsrsearch", search.as_str()),
                ("gsrnamespace", "6"),
                ("gsrlimit", &PAGE.to_string()),
                ("gsroffset", &offset.to_string()),
                ("prop", "imageinfo"),
                ("iiprop", "url|size|mime|extmetadata"),
                ("iiurlwidth", THUMB_WIDTH),
                ("iiextmetadatafilter", "License|LicenseUrl|Artist"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("decoding commons response")?;

        let pages = response.query.map(|query| query.pages).unwrap_or_default();
        out.extend(pages.into_iter().filter_map(|page| candidate(species, page)));
        match response.cont {
            Some(cont) => offset = cont.gsroffset,
            None => break,
        }
    }

    out.truncate(limit);
    Ok(out)
}

/// Convert one file page into a candidate. Returns `None` for non-bitmap
/// files and files without licence metadata.
fn candidate(species: &Species, page: Page) -> Option<Candidate> {
    let info = page.imageinfo?.into_iter().next()?;
    if !info.mime.starts_with("image/") || info.mime == "image/svg+xml" {
        return None;
    }

    let meta = |key: &str| {
        info.extmetadata
            .get(key)
            .map(|entry| entry.value.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let license = normalise_license(&meta("License")?);
    let (author, author_url) = match meta("Artist") {
        Some(html) => {
            let (text, link) = html_text_and_link(&html);
            (Some(text).filter(|text| !text.is_empty()), link)
        }
        None => (None, None),
    };

    Some(Candidate {
        species: species.slug,
        source: "commons",
        source_id: page.pageid.to_string(),
        // Thumbnail URLs carry `?utm_source=...` tracking parameters. Drop them.
        image_url: info.thumburl?.split('?').next()?.to_string(),
        source_url: info.descriptionurl,
        author,
        author_url,
        license,
        license_url: meta("LicenseUrl"),
        width: info.thumbwidth,
        height: info.thumbheight,
    })
}

/// Normalise the `License` field. Commons gives values like `cc-by-sa-4.0`,
/// `cc0`, `pd`, or a multi-licence list such as `cc-by-sa-3.0,2.5,2.0,1.0`.
/// The first entry wins.
fn normalise_license(raw: &str) -> String {
    let first = raw.split(',').next().unwrap_or(raw).trim();
    match first {
        "pd" => "PD".to_string(),
        "cc0" => "CC0-1.0".to_string(),
        other => other.to_ascii_uppercase(),
    }
}

/// Pull the visible text and the first `href` out of a snippet of HTML. The
/// `Artist` field is usually a link, e.g.
/// `<a href="//commons.wikimedia.org/wiki/User:X">X</a>`. Protocol-relative
/// and site-relative links are made absolute.
fn html_text_and_link(html: &str) -> (String, Option<String>) {
    let link =
        html.split_once("href=\"").and_then(|(_, rest)| rest.split_once('"')).map(|(url, _)| {
            match url {
                url if url.starts_with("//") => format!("https:{url}"),
                url if url.starts_with('/') => format!("https://commons.wikimedia.org{url}"),
                url => url.to_string(),
            }
        });

    // Drop everything between angle brackets, then decode the handful of
    // entities MediaWiki emits and collapse whitespace.
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    let text = text
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    (text, link)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_author_text_and_absolute_link() {
        let html =
            r#"<a href="//commons.wikimedia.org/wiki/User:Uoaei1" title="User:Uoaei1">Uoaei1</a>"#;
        assert_eq!(
            html_text_and_link(html),
            (
                "Uoaei1".to_string(),
                Some("https://commons.wikimedia.org/wiki/User:Uoaei1".to_string())
            )
        );
        assert_eq!(html_text_and_link("Jane &amp; Co"), ("Jane & Co".to_string(), None));
        let relative = r#"<a href="/w/index.php?title=User:Divl&amp;action=edit">Divl</a>"#;
        assert_eq!(
            html_text_and_link(relative).1.as_deref(),
            Some("https://commons.wikimedia.org/w/index.php?title=User:Divl&amp;action=edit")
        );
    }

    #[test]
    fn normalises_license_codes() {
        assert_eq!(normalise_license("cc-by-sa-4.0"), "CC-BY-SA-4.0");
        assert_eq!(normalise_license("cc-by-sa-3.0,2.5,2.0,1.0"), "CC-BY-SA-3.0");
        assert_eq!(normalise_license("pd"), "PD");
        assert_eq!(normalise_license("cc0"), "CC0-1.0");
    }

    #[test]
    fn skips_audio_and_keeps_bitmaps() {
        let json = r#"{"query":{"pages":[
          {"pageid":1,"imageinfo":[{"descriptionurl":"https://commons.wikimedia.org/wiki/File:A.ogg","mime":"application/ogg","thumburl":"x",
            "extmetadata":{"License":{"value":"pd"}}}]},
          {"pageid":2,"imageinfo":[{"descriptionurl":"https://commons.wikimedia.org/wiki/File:B.jpg","mime":"image/jpeg",
            "thumburl":"https://upload.wikimedia.org/B.jpg?utm_source=commons.wikimedia.org","thumbwidth":1280,"thumbheight":853,
            "extmetadata":{"License":{"value":"cc-by-4.0"},"LicenseUrl":{"value":"https://creativecommons.org/licenses/by/4.0"},
              "Artist":{"value":"<a href=\"//commons.wikimedia.org/wiki/User:A\">Alexis</a>"}}}]}
        ]}}"#;
        let response: Response = serde_json::from_str(json).unwrap();
        let species = &super::super::SPECIES[2];
        let found: Vec<_> = response
            .query
            .unwrap()
            .pages
            .into_iter()
            .filter_map(|p| candidate(species, p))
            .collect();
        assert_eq!(found.len(), 1);
        let fox = &found[0];
        assert_eq!(fox.source_id, "2");
        assert_eq!(fox.image_url, "https://upload.wikimedia.org/B.jpg");
        assert_eq!(fox.license, "CC-BY-4.0");
        assert_eq!(fox.author.as_deref(), Some("Alexis"));
        assert_eq!(fox.author_url.as_deref(), Some("https://commons.wikimedia.org/wiki/User:A"));
        assert_eq!((fox.width, fox.height), (Some(1280), Some(853)));
    }
}
