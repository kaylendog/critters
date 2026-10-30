//! Candidate images from upstream sources, normalised into one shape.

pub mod commons;
pub mod inaturalist;

use crate::{App, db};

/// An image found upstream that may be inserted into the `images` table.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// Species slug, e.g. `coyote`.
    pub species: &'static str,
    /// Source slug, e.g. `commons`.
    pub source: &'static str,
    /// The source's own identifier for the image. Unique per source.
    pub source_id: String,
    /// Direct URL to a reasonably sized rendition of the image.
    pub image_url: String,
    /// The page the image came from, for attribution.
    pub source_url: String,
    /// Author name, when known.
    pub author: Option<String>,
    /// Author profile URL, when known.
    pub author_url: Option<String>,
    /// Normalised licence code, e.g. `CC-BY-SA-4.0`, `CC0-1.0` or `PD`.
    pub license: String,
    /// Licence deed URL, when the source provides one.
    pub license_url: Option<String>,
    /// Width of `image_url` in pixels, when known.
    pub width: Option<i32>,
    /// Height of `image_url` in pixels, when known.
    pub height: Option<i32>,
}

/// A species we serve, with how to find it on each source.
pub struct Species {
    /// URL slug, e.g. `coyote`.
    pub slug: &'static str,
    /// CirrusSearch query on Wikimedia Commons. `deepcategory:` walks
    /// subcategories.
    pub commons_query: &'static str,
    /// iNaturalist taxon id.
    pub inat_taxon: u32,
}

/// Every species we serve.
pub const SPECIES: &[Species] = &[
    Species {
        slug: "wolf",
        // The Canis lupus tree includes domestic dogs and dingoes. Leave them out.
        commons_query: r#"deepcategory:"Canis lupus" -deepcategory:"Canis lupus familiaris" -deepcategory:"Dingo""#,
        inat_taxon: 42048,
    },
    Species { slug: "coyote", commons_query: r#"deepcategory:"Canis latrans""#, inat_taxon: 42051 },
    Species { slug: "fox", commons_query: r#"deepcategory:"Vulpes vulpes""#, inat_taxon: 42069 },
];

/// Look up a species by its URL slug.
pub fn species(slug: &str) -> Option<&'static Species> {
    SPECIES.iter().find(|species| species.slug == slug)
}

/// Licence families that permit non-commercial redistribution.
pub const DEFAULT_LICENSES: &[&str] =
    &["CC0", "PD", "CC-BY", "CC-BY-SA", "CC-BY-NC", "CC-BY-NC-SA"];

/// Strip the version from a licence code, so `CC-BY-SA-4.0` becomes `CC-BY-SA`.
/// Codes without a version are returned unchanged.
pub fn family(code: &str) -> &str {
    match code.rsplit_once('-') {
        Some((head, version)) if version.chars().all(|c| c.is_ascii_digit() || c == '.') => head,
        _ => code,
    }
}

/// Whether a licence code belongs to one of the allowed families.
pub fn allowed(code: &str, allowlist: &[String]) -> bool {
    allowlist.iter().any(|allowed| allowed.eq_ignore_ascii_case(family(code)))
}

/// Fetch candidates from every source for every species and insert the ones
/// with an allowed licence. Images already present are left untouched.
pub async fn ingest(app: &App, per_species: usize) -> anyhow::Result<()> {
    let allowlist: Vec<String> = app
        .config
        .licenses
        .clone()
        .unwrap_or_else(|| DEFAULT_LICENSES.iter().map(ToString::to_string).collect());

    for species in SPECIES {
        // Gather from each source independently, so one source failing does not
        // block the other.
        let mut candidates = Vec::new();
        match commons::fetch(&app.http, species, per_species).await {
            Ok(found) => candidates.extend(found),
            Err(error) => tracing::warn!(species = species.slug, %error, "commons fetch failed"),
        }
        match inaturalist::fetch(&app.http, species, per_species).await {
            Ok(found) => candidates.extend(found),
            Err(error) => {
                tracing::warn!(species = species.slug, %error, "inaturalist fetch failed")
            }
        }

        // Filter by licence and insert. Rejected codes are logged at debug
        // level so unknown licence strings can be spotted.
        let mut inserted = 0;
        let mut rejected = 0;
        for candidate in candidates {
            if !allowed(&candidate.license, &allowlist) {
                tracing::debug!(
                    license = candidate.license,
                    url = candidate.source_url,
                    "rejected"
                );
                rejected += 1;
                continue;
            }
            if db::insert_image(&app.pool, &candidate).await? {
                inserted += 1;
            }
        }
        tracing::info!(species = species.slug, inserted, rejected, "ingested");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_strips_version_only() {
        assert_eq!(family("CC-BY-SA-4.0"), "CC-BY-SA");
        assert_eq!(family("CC0-1.0"), "CC0");
        assert_eq!(family("CC-BY-NC"), "CC-BY-NC");
        assert_eq!(family("PD"), "PD");
    }

    #[test]
    fn allowlist_matches_by_family() {
        let allowlist: Vec<String> = DEFAULT_LICENSES.iter().map(ToString::to_string).collect();
        assert!(allowed("CC-BY-SA-3.0", &allowlist));
        assert!(allowed("cc0-1.0", &allowlist));
        assert!(!allowed("CC-BY-ND-4.0", &allowlist));
        assert!(!allowed("GFDL-1.2", &allowlist));
    }
}
