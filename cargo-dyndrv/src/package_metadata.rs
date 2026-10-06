//! Preserve package environment text from the same validated Cargo payload.
use cargo_metadata::{PackageId, camino::Utf8PathBuf};
use serde::{Deserialize, de::Error as _};

#[derive(Debug)]
pub(crate) struct Metadata {
    pub workspace_root: Utf8PathBuf,
    pub target_directory: Utf8PathBuf,
    pub workspace_members: Vec<PackageId>,
    pub packages: Vec<PackageRecord>,
}

#[derive(Debug)]
pub(crate) struct PackageRecord {
    pub package: cargo_metadata::Package,
    rust_version: Option<String>,
}

impl PackageRecord {
    pub(crate) fn rust_version_env(&self) -> &str {
        self.rust_version.as_deref().unwrap_or_default()
    }
}

#[derive(Deserialize)]
struct RawMetadata {
    packages: Vec<RawPackage>,
}

#[derive(Deserialize)]
struct RawPackage {
    id: PackageId,
    #[serde(default)]
    rust_version: Option<String>,
}

impl Metadata {
    pub(crate) fn from_slice(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        // Validate the complete original schema first. In particular, parsing
        // through Value would silently discard duplicate known JSON fields.
        let typed = serde_json::from_slice(bytes)?;
        let raw = serde_json::from_slice(bytes)?;
        Self::pair(typed, raw)
    }

    fn pair(typed: cargo_metadata::Metadata, raw: RawMetadata) -> Result<Self, serde_json::Error> {
        if typed.packages.len() != raw.packages.len()
            || typed
                .packages
                .iter()
                .zip(&raw.packages)
                .any(|(package, raw)| package.id != raw.id)
        {
            return Err(serde_json::Error::custom(
                "package metadata identity/cardinality mismatch",
            ));
        }
        let packages = typed
            .packages
            .into_iter()
            .zip(raw.packages)
            .map(|(package, raw)| PackageRecord {
                package,
                rust_version: raw.rust_version,
            })
            .collect();
        Ok(Self {
            workspace_root: typed.workspace_root,
            target_directory: typed.target_directory,
            workspace_members: typed.workspace_members,
            packages,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document() -> serde_json::Value {
        json!({"packages":[
            {"name":"short","version":"0.1.0","id":"short","manifest_path":"/fixture/short/Cargo.toml","targets":[],"dependencies":[],"features":{},"rust_version":"1.85"},
            {"name":"patch","version":"0.1.0","id":"patch","manifest_path":"/fixture/patch/Cargo.toml","targets":[],"dependencies":[],"features":{},"rust_version":"1.85.0"}
        ],"workspace_root":"/fixture","target_directory":"/fixture/target","workspace_members":["short","patch"],"workspace_default_members":["short"],"version":1,"resolve":null,"metadata":{"unrelated":42}})
    }

    #[test]
    fn exact_spelling_and_typed_fields_follow_package_identity() {
        let mut doc = document();
        for _ in 0..2 {
            let bytes = serde_json::to_vec(&doc).unwrap();
            let typed: cargo_metadata::Metadata = serde_json::from_slice(&bytes).unwrap();
            let actual = Metadata::from_slice(&bytes).unwrap();
            assert_eq!(actual.workspace_root, typed.workspace_root);
            assert_eq!(actual.target_directory, typed.target_directory);
            assert_eq!(actual.workspace_members, typed.workspace_members);
            assert_eq!(actual.packages.len(), typed.packages.len());
            for (record, original) in actual.packages.iter().zip(&typed.packages) {
                assert_eq!(&record.package, original);
                let expected = if record.package.id.repr == "short" {
                    "1.85"
                } else {
                    "1.85.0"
                };
                assert_eq!(record.rust_version_env(), expected);
            }
            assert_eq!(
                actual.packages[0].package.rust_version,
                actual.packages[1].package.rust_version
            );
            doc["packages"].as_array_mut().unwrap().reverse();
        }
    }

    #[test]
    fn absent_null_and_patch_values_keep_original_parser_validation() {
        for (value, expected) in [
            (Some(json!(null)), ""),
            (None, ""),
            (Some(json!("1.85.1")), "1.85.1"),
        ] {
            let mut doc = document();
            let package = doc["packages"][0].as_object_mut().unwrap();
            match value {
                Some(value) => {
                    package.insert("rust_version".into(), value);
                }
                None => {
                    package.remove("rust_version");
                }
            }
            assert_eq!(
                Metadata::from_slice(&serde_json::to_vec(&doc).unwrap())
                    .unwrap()
                    .packages[0]
                    .rust_version_env(),
                expected
            );
        }
        for value in [
            json!(42),
            json!([]),
            json!(""),
            json!("invalid"),
            json!("1.85-alpha"),
            json!("1.85.0+build"),
        ] {
            let mut doc = document();
            doc["packages"][0]["rust_version"] = value;
            let bytes = serde_json::to_vec(&doc).unwrap();
            let original = serde_json::from_slice::<cargo_metadata::Metadata>(&bytes).unwrap_err();
            assert_eq!(
                Metadata::from_slice(&bytes).unwrap_err().to_string(),
                original.to_string()
            );
        }
        let bytes = serde_json::to_string(&document()).unwrap().replace(
            "\"rust_version\":\"1.85\"",
            "\"rust_version\":\"1.85\",\"rust_version\":\"1.85.0\"",
        );
        let original = serde_json::from_str::<cargo_metadata::Metadata>(&bytes).unwrap_err();
        assert_eq!(
            Metadata::from_slice(bytes.as_bytes())
                .unwrap_err()
                .to_string(),
            original.to_string()
        );
    }

    #[test]
    fn cardinality_and_positional_identity_must_match_before_pairing() {
        for mutation in ["missing", "extra", "wrong-id", "reordered"] {
            let doc = document();
            let typed = serde_json::from_value(doc.clone()).unwrap();
            let mut raw: RawMetadata = serde_json::from_value(doc).unwrap();
            match mutation {
                "missing" => {
                    raw.packages.pop();
                }
                "extra" => raw.packages.push(RawPackage {
                    id: PackageId {
                        repr: "extra".into(),
                    },
                    rust_version: None,
                }),
                "wrong-id" => raw.packages[0].id.repr = "wrong".into(),
                "reordered" => raw.packages.reverse(),
                _ => unreachable!(),
            }
            assert!(
                Metadata::pair(typed, raw)
                    .unwrap_err()
                    .to_string()
                    .contains("identity/cardinality")
            );
        }
    }
}
