//! A jurisdiction-aware policy for locally executed Studio model weights.
//!
//! This module does not geolocate people, grant territorial rights, or treat
//! clicking "accept" as a separate MiniMax authorization. It makes the app
//! fail closed for known restricted weights, including local imports, while
//! leaving model-agnostic engines and Apache-2.0 Wan weights unrestricted.
//!
//! Authorization for excluded territories requires an independently verified
//! license from the rights holder. There is deliberately no self-service
//! "I have an exemption" switch.
use crate::generation::{GenerationModelSpec, LicenseGate};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const MINIMAX_H3_LICENSE_URL: &str =
    "https://huggingface.co/OpenVDN/vdn-minimax-h3-edge/blob/main/LICENSE";
const MINIMAX_H3_LICENSE_ID: &str = "minimax-h3-community-2026-08-02";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LicenseRegion {
    Eu,
    Uk,
    Us,
    Kr,
    /// User attests that their use is outside all four excluded territories.
    Other,
}
impl LicenseRegion {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "eu" => Ok(Self::Eu),
            "uk" => Ok(Self::Uk),
            "us" => Ok(Self::Us),
            "kr" => Ok(Self::Kr),
            "other" => Ok(Self::Other),
            _ => Err("Choose EU, UK, US, South Korea, or outside these territories".into()),
        }
    }
    pub fn code(self) -> &'static str {
        match self {
            Self::Eu => "eu",
            Self::Uk => "uk",
            Self::Us => "us",
            Self::Kr => "kr",
            Self::Other => "other",
        }
    }
}

/// Identifies known MiniMax H3 and VDN derivatives by model *provenance*,
/// not by a caller-editable license checkbox. Inspect component repository,
/// component path, model-source repository and local file/directory names.
/// This is not cryptographic model identification: renamed anonymous weights
/// cannot be verified without upstream signed hashes.
pub fn is_minimax_h3(spec: &GenerationModelSpec) -> bool {
    let provenance = format!(
        "{} {} {} {} {}",
        spec.id,
        spec.name,
        spec.family,
        serde_json::to_string(&spec.source).unwrap_or_default(),
        serde_json::to_string(&spec.components).unwrap_or_default(),
    );
    let normalized: String = provenance
        .to_ascii_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    ["minimax-h3", "minimaxh3", "vdn-minimax", "vdn-h3", "vdnh3"]
        .iter()
        .any(|name| normalized.contains(name))
}

/// Official MiniMax source restrictions always override user-editable license
/// metadata. Preserve license gates for unrelated/community models.
pub fn effective_license(spec: &GenerationModelSpec) -> LicenseGate {
    if !is_minimax_h3(spec) {
        return spec.license.clone();
    }
    LicenseGate {
        id: MINIMAX_H3_LICENSE_ID.into(),
        name: "MiniMax H3 Community License (territorial restrictions)".into(),
        url: MINIMAX_H3_LICENSE_URL.into(),
        excluded_territories: vec!["EU".into(), "UK".into(), "US".into(), "KR".into()],
        acceptance_required: true,
    }
}

pub fn acceptance_recorded(
    spec: &GenerationModelSpec,
    accepted: &BTreeSet<String>,
) -> bool {
    let license = effective_license(spec);
    !license.acceptance_required || (!license.id.is_empty() && accepted.contains(&license.id))
}

/// Rights checking is part of every download AND every inference request.
/// A missing jurisdiction blocks restricted weights; selecting an excluded
/// jurisdiction never grants rights, even after acceptance.
pub fn block_reason(
    spec: &GenerationModelSpec,
    region: Option<LicenseRegion>,
    accepted: &BTreeSet<String>,
) -> Option<String> {
    let license = effective_license(spec);
    if !license.excluded_territories.is_empty() {
        let Some(region) = region else {
            return Some("Declare where you will use these model weights before downloading or running them.".into());
        };
        // For MiniMax H3, the contract has exactly four known exclusions.
        if is_minimax_h3(spec) && region != LicenseRegion::Other {
            return Some(format!(
                "MiniMax H3/VDN-H3 community weights cannot be used in this territory without separate written authorization from MiniMax. See {MINIMAX_H3_LICENSE_URL}"
            ));
        }
        // The broad "other" assertion cannot prove compliance with an
        // arbitrary third-party license that excludes other named regions.
        // Fail closed until an explicit policy for that particular model exists.
        if !is_minimax_h3(spec) {
            return Some("This model has territorial restrictions that require independent license verification before use.".into());
        }
    }
    if !acceptance_recorded(spec, accepted) {
        return Some(format!(
            "Accept the {} before downloading or running this model.",
            license.name
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generation::GenerationModelSpec;

    fn spec(id: &str, source: &str) -> GenerationModelSpec {
        let mut model: GenerationModelSpec = serde_json::from_value(serde_json::json!({
            "id":id,"name":"Community video","family":"video",
            "tasks":["text_to_video"], "components":[{
                "slot":"diffusion_model",
                "source":{"kind":"hugging_face","repo":source,"file":"model.safetensors"},
                "sizeBytes":32
            }],
            "source":{"kind":"components"},
            "defaults":{"width":512,"height":512,"steps":8,"cfgScale":4.0,
                "sampleMethod":"euler","flowShift":null,"fps":16,"videoFrames":17,
                "frameGrid":"down_to4n_plus1"},
            "minRamBytes":1024,
            "license":{"id":"","name":"","url":"","excludedTerritories":[],
                "acceptanceRequired":false},
            "extraLaunchArgs":[]
        })).unwrap();
        model.name = "Unbranded".into();
        model
    }

    #[test]
    fn minimax_cannot_be_disguised_with_blank_license_or_model_label() {
        let source = spec("totally-ordinary", "OpenVDN/vdn-minimax-h3-edge");
        assert!(is_minimax_h3(&source));
        let effective = effective_license(&source);
        assert_eq!(effective.id, MINIMAX_H3_LICENSE_ID);
        assert!(effective.acceptance_required);
        assert_eq!(effective.excluded_territories, ["EU","UK","US","KR"]);
    }

    #[test]
    fn restricted_model_is_blocked_in_four_territories_even_after_acceptance() {
        let source = spec("minimax-h3", "OpenVDN/vdn-minimax-h3-edge");
        let accepted = BTreeSet::from([MINIMAX_H3_LICENSE_ID.into()]);
        for region in [None, Some(LicenseRegion::Eu), Some(LicenseRegion::Us),
            Some(LicenseRegion::Uk), Some(LicenseRegion::Kr)] {
            assert!(block_reason(&source, region, &accepted).is_some());
        }
        assert!(block_reason(&source, Some(LicenseRegion::Other), &accepted).is_none());
        assert!(block_reason(&source, Some(LicenseRegion::Other), &BTreeSet::new()).is_some());
    }

    #[test]
    fn wan_global_model_is_not_blocked_by_minimax_policy() {
        let wan = spec("wan22", "Wan-AI/Wan2.2-TI2V-5B");
        assert!(!is_minimax_h3(&wan));
        assert!(block_reason(&wan, None, &BTreeSet::new()).is_none());
    }

    #[test]
    fn independent_territorial_license_fails_closed() {
        let mut other = spec("another-video", "someone/example");
        other.license.excluded_territories = vec!["JP".into()];
        assert!(block_reason(&other, Some(LicenseRegion::Other), &BTreeSet::new()).is_some());
    }

    #[test]
    fn rejects_unknown_region_instead_of_treating_it_as_elsewhere() {
        assert!(LicenseRegion::parse("se").is_err());
        assert_eq!(LicenseRegion::parse("other").unwrap(), LicenseRegion::Other);
    }
}
