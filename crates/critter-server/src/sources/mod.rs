//! Candidate images from upstream sources, normalised into one shape.

pub mod commons;
pub mod inaturalist;
pub mod openverse;

use crate::{App, db, ml::embed};

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
    /// Openverse free-text search. `None` skips Openverse for this species.
    pub openverse_query: Option<&'static str>,
    /// iNaturalist taxon id. `None` for things iNaturalist has no taxon for,
    /// such as dog breeds.
    pub inat_taxon: Option<u32>,
}

/// Every species we serve.
pub const SPECIES: &[Species] = &[
    Species {
        slug: "wolf",
        // The Canis lupus tree includes domestic dogs and dingoes. Leave them out.
        commons_query: r#"deepcategory:"Canis lupus" -deepcategory:"Canis lupus familiaris" -deepcategory:"Dingo""#,
        openverse_query: Some("gray wolf"),
        inat_taxon: Some(42048),
    },
    Species {
        slug: "coyote",
        commons_query: r#"deepcategory:"Canis latrans""#,
        openverse_query: Some("coyote"),
        inat_taxon: Some(42051),
    },
    Species {
        slug: "fox",
        commons_query: r#"deepcategory:"Vulpes vulpes""#,
        openverse_query: Some("red fox"),
        inat_taxon: Some(42069),
    },
    Species {
        slug: "bernese",
        commons_query: r#"deepcategory:"Bernese Mountain Dog""#,
        openverse_query: Some("bernese mountain dog"),
        inat_taxon: None,
    },
    Species {
        slug: "hyena",
        commons_query: r#"deepcategory:"Crocuta crocuta""#,
        openverse_query: Some("spotted hyena"),
        inat_taxon: Some(41886),
    },
    Species {
        slug: "wild-dog",
        commons_query: r#"deepcategory:"Lycaon pictus""#,
        openverse_query: Some("african wild dog"),
        inat_taxon: Some(42093),
    },
    Species {
        slug: "deer",
        // The whole Cervidae tree is 65k files of antlers, heraldry and venison.
        // The curated quality images are all real photographs.
        commons_query: r#"deepcategory:"Quality images of Cervidae""#,
        openverse_query: Some("deer"),
        inat_taxon: Some(42158),
    },
    Species {
        slug: "bearded-dragon",
        // The whole genus, so pet species other than the central bearded dragon
        // count too.
        commons_query: r#"deepcategory:"Pogona""#,
        openverse_query: Some("bearded dragon"),
        inat_taxon: Some(31410),
    },
    Species {
        slug: "leopard-gecko",
        commons_query: r#"deepcategory:"Eublepharis macularius""#,
        openverse_query: Some("leopard gecko"),
        inat_taxon: Some(34675),
    },
    Species {
        slug: "crested-gecko",
        commons_query: r#"deepcategory:"Correlophus ciliatus""#,
        openverse_query: Some("crested gecko"),
        inat_taxon: Some(200209),
    },
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

/// Fetch candidates from every source for the given species, or all species
/// when `only` is empty, and insert the ones with an allowed licence. Images
/// already present are left untouched.
pub async fn ingest(app: &App, per_species: usize, only: &[String]) -> anyhow::Result<()> {
    let allowlist: Vec<String> = app
        .config
        .licenses
        .clone()
        .unwrap_or_else(|| DEFAULT_LICENSES.iter().map(ToString::to_string).collect());

    let selected = SPECIES
        .iter()
        .filter(|species| only.is_empty() || only.iter().any(|slug| slug == species.slug));
    for species in selected {
        tracing::info!(species = species.slug, "fetching");
        // Gather from each source independently, so one source failing does not
        // block the other.
        let mut candidates = Vec::new();
        match commons::fetch(&app.http, species, per_species).await {
            Ok(found) => candidates.extend(found),
            Err(error) => tracing::warn!(species = species.slug, %error, "commons fetch failed"),
        }
        match openverse::fetch(&app.http, species, per_species).await {
            Ok(found) => candidates.extend(found),
            Err(error) => {
                tracing::warn!(species = species.slug, %error, "openverse fetch failed")
            }
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

    // Embed whatever is new, plus anything a previous pass could not fetch.
    // Then score the new arrivals, or they cannot qualify for slots until the
    // next retrain. Rescoring is a no-op without a model.
    let embedded = embed::embed_pending(app).await?;
    if embedded > 0 {
        db::rescore(&app.pool).await?;
    }
    tracing::info!(embedded, "ingest complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_match_shared_types() {
        let slugs: Vec<_> = SPECIES.iter().map(|species| species.slug).collect();
        assert_eq!(slugs, critter_types::SPECIES);
    }

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
