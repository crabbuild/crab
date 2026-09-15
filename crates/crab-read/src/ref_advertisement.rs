use crab_metadata::capsule_protocol::RepositoryRoot;
use crab_metadata::manifests::Manifest;

use crate::hidden_refs;

/// A ref advertised from a manifest snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestRefEntry {
    pub sha: String,
    pub ref_name: String,
    pub peeled: Option<String>,
}

/// Ref advertisement derived from a manifest after read-side policy filtering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestRefAdvertisement {
    pub refs: Vec<ManifestRefEntry>,
    pub head_symref: Option<String>,
}

/// Builds the ref advertisement for a manifest using `transfer.hideRefs` rules.
#[must_use]
pub fn manifest_ref_advertisement(
    manifest: &Manifest,
    hidden_ref_patterns: &[String],
) -> ManifestRefAdvertisement {
    advertisement(
        &manifest.refs,
        &manifest.peeled_refs,
        &manifest.head,
        hidden_ref_patterns,
    )
}

/// Builds ref advertisement from the capsule-protocol repository root.
#[must_use]
pub fn root_ref_advertisement(
    root: &RepositoryRoot,
    hidden_ref_patterns: &[String],
) -> ManifestRefAdvertisement {
    advertisement(
        root.refs(),
        root.peeled_refs(),
        root.head(),
        hidden_ref_patterns,
    )
}

/// Builds ref advertisement from one materialized capsule repository view.
#[must_use]
pub fn capsule_ref_advertisement(
    view: &crate::capsule_protocol::CapsuleRepositoryView,
    hidden_ref_patterns: &[String],
) -> ManifestRefAdvertisement {
    advertisement(
        view.refs(),
        view.peeled_refs(),
        view.head(),
        hidden_ref_patterns,
    )
}

fn advertisement(
    refs: &std::collections::BTreeMap<String, String>,
    peeled_refs: &std::collections::BTreeMap<String, String>,
    head: &str,
    hidden_ref_patterns: &[String],
) -> ManifestRefAdvertisement {
    let hidden_refs = hidden_refs::compile(hidden_ref_patterns);
    let refs = refs
        .iter()
        .filter(|(name, _)| !hidden_refs.is_match(name.as_str()))
        .map(|(name, sha)| ManifestRefEntry {
            sha: sha.clone(),
            ref_name: name.clone(),
            peeled: peeled_refs.get(name).cloned(),
        })
        .collect::<Vec<_>>();

    // Preserve the actual symbolic target, including an unborn branch. Hidden
    // targets stay hidden; substituting a visible ref would invent a new HEAD.
    let head_symref = (!hidden_refs.is_match(head)).then(|| head.to_owned());

    ManifestRefAdvertisement { refs, head_symref }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_with_refs(head: &str, pairs: &[(&str, &str)]) -> Manifest {
        let refs = pairs
            .iter()
            .map(|(name, sha)| ((*name).to_owned(), (*sha).to_owned()))
            .collect();
        let mut manifest = Manifest::default_for_repo(head);
        manifest.generation = 1;
        manifest.refs = refs;
        manifest.seal_git_validation();
        manifest
    }

    #[test]
    fn advertisement_filters_hidden_refs_and_head() {
        let manifest = manifest_with_refs(
            "refs/heads/secret",
            &[
                ("refs/heads/main", &"a".repeat(40)),
                ("refs/heads/secret", &"b".repeat(40)),
            ],
        );

        let advertisement =
            manifest_ref_advertisement(&manifest, &["refs/heads/secret".to_owned()]);

        assert_eq!(advertisement.refs.len(), 1);
        assert_eq!(advertisement.refs[0].ref_name, "refs/heads/main");
        assert_eq!(advertisement.head_symref, None);
    }

    #[test]
    fn advertisement_drops_head_when_no_refs_are_visible() {
        let manifest = manifest_with_refs(
            "refs/heads/secret",
            &[("refs/heads/secret", &"b".repeat(40))],
        );

        let advertisement =
            manifest_ref_advertisement(&manifest, &["refs/heads/secret".to_owned()]);

        assert!(advertisement.refs.is_empty());
        assert_eq!(advertisement.head_symref, None);
    }

    #[test]
    fn advertisement_preserves_unborn_head_alongside_tags() {
        let manifest =
            manifest_with_refs("refs/heads/unborn", &[("refs/tags/v1", &"a".repeat(40))]);
        let advertisement = manifest_ref_advertisement(&manifest, &[]);
        assert_eq!(
            advertisement.head_symref.as_deref(),
            Some("refs/heads/unborn")
        );
    }

    #[test]
    fn advertisement_preserves_peeled_tags() {
        let mut manifest = manifest_with_refs(
            "refs/heads/main",
            &[
                ("refs/heads/main", &"a".repeat(40)),
                ("refs/tags/v1", &"b".repeat(40)),
            ],
        );
        manifest
            .peeled_refs
            .insert("refs/tags/v1".to_owned(), "c".repeat(40));

        let advertisement = manifest_ref_advertisement(&manifest, &[]);

        let tag = advertisement
            .refs
            .iter()
            .find(|entry| entry.ref_name == "refs/tags/v1")
            .expect("tag ref advertised");
        assert_eq!(
            tag.peeled.as_deref(),
            Some("cccccccccccccccccccccccccccccccccccccccc")
        );
    }
}
