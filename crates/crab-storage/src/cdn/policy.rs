use crate::StorageProviderKind;
use sha2::{Digest as _, Sha256};
use std::time::Duration;

use super::qualification_enabled;

const RELEASE_SCOPES: &str = include_str!("qualified-scopes.txt");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReadLimits {
    full_bytes: u64,
    ranged_object_bytes: u64,
    pub(crate) range_bytes: u64,
    pub(crate) timeout: Duration,
}

impl ReadLimits {
    pub(crate) const FIXTURE: Self = Self {
        full_bytes: u64::MAX,
        ranged_object_bytes: u64::MAX,
        range_bytes: u64::MAX,
        timeout: Duration::MAX,
    };

    pub(super) fn parse(value: &str) -> Option<Self> {
        let columns: Vec<_> = value.split(' ').collect();
        let [full, ranged_object, range, millis] = columns.as_slice() else {
            return None;
        };
        fn number(value: &str) -> Option<u64> {
            if value.len() > 1 && value.starts_with('0') {
                return None;
            }
            value
                .bytes()
                .all(|byte| byte.is_ascii_digit())
                .then(|| value.parse().ok())?
        }
        let full_bytes = number(full)?;
        let ranged_object_bytes = number(ranged_object)?;
        let range_bytes = number(range)?;
        let millis = u32::try_from(number(millis)?).ok()?;
        if millis == 0
            || (full_bytes == 0 && range_bytes == 0)
            || (ranged_object_bytes == 0) != (range_bytes == 0)
            || range_bytes > ranged_object_bytes
        {
            return None;
        }
        Some(Self {
            full_bytes,
            ranged_object_bytes,
            range_bytes,
            timeout: Duration::from_millis(u64::from(millis)),
        })
    }

    pub(crate) fn admits(self, size: u64, requested_range_bytes: Option<u64>) -> bool {
        match requested_range_bytes {
            Some(bytes) => {
                self.range_bytes > 0
                    && size <= self.ranged_object_bytes
                    && bytes <= self.range_bytes
            }
            None => self.full_bytes > 0 && size <= self.full_bytes,
        }
    }
}

/// Digest of the compiled policy with platform-independent line endings.
#[must_use]
pub fn release_policy_sha256() -> String {
    format!("{:x}", Sha256::digest(RELEASE_SCOPES.replace("\r\n", "\n")))
}

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
        !self.read_limits().is_empty()
    }

    pub(crate) fn read_limits(self) -> Vec<(CdnReadFamily, ReadLimits)> {
        self.select_limits(RELEASE_SCOPES, qualification_enabled())
    }

    fn select_limits(self, scopes: &str, fixture_build: bool) -> Vec<(CdnReadFamily, ReadLimits)> {
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
            .filter_map(|family| {
                let mut rows = scopes.lines().filter_map(|row| {
                    let (scope, limits) = row.split_once(' ').unwrap_or((row, ""));
                    scope
                        .split('/')
                        .eq([
                            edge,
                            self.visibility.label(),
                            self.consumer.label(),
                            family.label(),
                            self.profile.label(),
                        ])
                        .then_some(limits)
                });
                let limits = match rows.next() {
                    Some(row) if rows.next().is_none() => ReadLimits::parse(row)?,
                    None if fixture_build => ReadLimits::FIXTURE,
                    _ => return None,
                };
                Some((family, limits))
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
        let scopes = "cloudfront/private/cli/git/direct 10 64 8 100\ncloudfront/private/cli/xorb/direct/extra 10 64 8 100";
        let limits = approved.select_limits(scopes, false);
        let [(CdnReadFamily::Git, limits)] = limits.as_slice() else {
            panic!("expected one exact release row")
        };
        assert!(limits.admits(10, None));
        assert!(!limits.admits(11, None));
        assert!(limits.admits(64, Some(8)));
        assert!(!limits.admits(65, Some(8)));
        assert!(!limits.admits(64, Some(9)));
        assert_eq!(limits.timeout, Duration::from_millis(100));
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
            assert!(unrelated.select_limits(scopes, false).is_empty());
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
            public.select_limits("", true),
            [
                (CdnReadFamily::Git, ReadLimits::FIXTURE),
                (CdnReadFamily::Capsule, ReadLimits::FIXTURE),
                (CdnReadFamily::Lfs, ReadLimits::FIXTURE),
            ]
        );
        assert!(
            CdnPolicy {
                profile: CdnProfile::Managed,
                ..public
            }
            .select_limits("", true)
            .is_empty()
        );
    }

    #[test]
    fn malformed_or_duplicate_release_rows_are_disabled() {
        let policy = CdnPolicy {
            provider: StorageProviderKind::S3,
            visibility: CdnVisibility::Private,
            consumer: CdnConsumer::Cli,
            profile: CdnProfile::Direct,
        };
        let scope = "cloudfront/private/cli/git/direct";
        for limits in [
            "",
            "0 0 0 100",
            "0 64 0 100",
            "0 8 64 100",
            "6 0 0 0",
            "06 0 0 100",
            "6 0 0 4294967296",
            "6 0 0 +100",
            "6 0 0 100 extra",
        ] {
            assert!(
                policy
                    .select_limits(&format!("{scope} {limits}"), false)
                    .is_empty(),
                "{limits}"
            );
        }
        let duplicate = format!("{scope} 6 0 0 100\n{scope}");
        assert!(policy.select_limits(&duplicate, false).is_empty());
    }
}
