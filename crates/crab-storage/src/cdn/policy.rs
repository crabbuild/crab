use crate::StorageProviderKind;

use super::qualification_enabled;

const RELEASE_SCOPES: &str = include_str!("qualified-scopes.txt");

/// Product read boundary that requested a CDN route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CdnConsumer {
    Cli,
    Sdk,
    Desktop,
    Mount,
    Server,
    S3Gateway,
}

impl CdnConsumer {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Sdk => "sdk",
            Self::Desktop => "desktop",
            Self::Mount => "mount",
            Self::Server => "server",
            Self::S3Gateway => "s3-gateway",
        }
    }
}

/// Authorization path that selected the physical repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CdnProfile {
    Direct,
    Managed,
}

impl CdnProfile {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Managed => "managed",
        }
    }
}

/// Visibility of the approved export.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CdnVisibility {
    Public,
    Private,
}

impl CdnVisibility {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Public => "public",
        }
    }
}

/// Immutable key families distinguished by release evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CdnReadFamily {
    Git,
    Capsule,
    Lfs,
    Xorb,
    ChunkPage,
    Shard,
}

impl CdnReadFamily {
    const ALL: [Self; 6] = [
        Self::Git,
        Self::Capsule,
        Self::Lfs,
        Self::Xorb,
        Self::ChunkPage,
        Self::Shard,
    ];

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Capsule => "capsule",
            Self::Lfs => "lfs",
            Self::Xorb => "xorb",
            Self::ChunkPage => "chunk-page",
            Self::Shard => "shard",
        }
    }

    pub(crate) const fn global(self) -> bool {
        matches!(self, Self::Xorb | Self::ChunkPage | Self::Shard)
    }
}

/// The shared dimensions of a candidate CDN release row.
#[derive(Clone, Copy)]
pub struct CdnPolicy {
    pub provider: StorageProviderKind,
    pub visibility: CdnVisibility,
    pub consumer: CdnConsumer,
    pub profile: CdnProfile,
}

impl CdnPolicy {
    /// Whether this combination can request any viewer grant in this build.
    #[must_use]
    pub fn enabled(self) -> bool {
        !self.families().is_empty()
    }

    pub(crate) fn families(self) -> Vec<CdnReadFamily> {
        self.select_families(RELEASE_SCOPES, qualification_enabled())
    }

    fn select_families(self, scopes: &str, fixture_build: bool) -> Vec<CdnReadFamily> {
        let edge = match self.provider {
            StorageProviderKind::S3 => "cloudfront",
            StorageProviderKind::Gcs => "cloud-cdn",
            StorageProviderKind::Azure => "front-door",
            StorageProviderKind::Local => return Vec::new(),
        };
        if self.visibility == CdnVisibility::Public && self.profile == CdnProfile::Managed {
            return Vec::new();
        }
        CdnReadFamily::ALL
            .into_iter()
            .filter(|family| self.visibility == CdnVisibility::Private || !family.global())
            .filter(|family| {
                fixture_build
                    || scopes.lines().any(|row| {
                        row.split('/').eq([
                            edge,
                            self.visibility.label(),
                            self.consumer.label(),
                            family.label(),
                            self.profile.label(),
                        ])
                    })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_row_is_exact_in_every_dimension() {
        let approved = CdnPolicy {
            provider: StorageProviderKind::S3,
            visibility: CdnVisibility::Private,
            consumer: CdnConsumer::Cli,
            profile: CdnProfile::Direct,
        };
        let scopes = "cloudfront/private/cli/git/direct\ncloudfront/private/cli/xorb/direct/extra";
        assert_eq!(
            approved.select_families(scopes, false),
            [CdnReadFamily::Git]
        );
        for unrelated in [
            CdnPolicy {
                provider: StorageProviderKind::Gcs,
                ..approved
            },
            CdnPolicy {
                visibility: CdnVisibility::Public,
                ..approved
            },
            CdnPolicy {
                consumer: CdnConsumer::Mount,
                ..approved
            },
            CdnPolicy {
                profile: CdnProfile::Managed,
                ..approved
            },
        ] {
            assert!(unrelated.select_families(scopes, false).is_empty());
        }
    }

    #[test]
    fn fixture_build_preserves_public_scope_limits() {
        let public = CdnPolicy {
            provider: StorageProviderKind::Azure,
            visibility: CdnVisibility::Public,
            consumer: CdnConsumer::Desktop,
            profile: CdnProfile::Direct,
        };
        assert_eq!(
            public.select_families("", true),
            [
                CdnReadFamily::Git,
                CdnReadFamily::Capsule,
                CdnReadFamily::Lfs
            ]
        );
        assert!(
            CdnPolicy {
                profile: CdnProfile::Managed,
                ..public
            }
            .select_families("", true)
            .is_empty()
        );
    }
}
