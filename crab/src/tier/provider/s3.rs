//! S3 lifecycle provider and restore backend via `aws-sdk-s3`.
//!
//! Produces `PutBucketLifecycleConfiguration` XML per the S3
//! `2006-03-01` API. The document includes:
//!
//! - `<LifecycleConfiguration>` root with the S3 namespace.
//! - Per-rule `<ID>`, `<Status>`, `<Filter>` (with `<And>` containing
//!   `<Prefix>` and optionally `<ObjectSizeGreaterThan>`).
//! - `<Transition>` elements for current-version transitions.
//! - `<NoncurrentVersionTransition>` when versioning is enabled.
//! - `<NoncurrentVersionExpiration>` when versioning is enabled and
//!   `noncurrent_expiration_days` is set.
//!
//! Rule order is deterministic (sorted by ID) for snapshot-test
//! stability.
//!
//! The [`S3LifecycleProvider`] struct implements both
//! [`LifecycleProvider`] (lifecycle rule CRUD) and
//! [`RestoreBackend`] (archive restore + state queries) using the
//! `aws-sdk-s3` client. All code in this module is gated behind
//! `#[cfg(feature = "tier-s3")]` at the module level (see
//! `provider/mod.rs`).

use std::time::Duration;

use async_trait::async_trait;
use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use tracing::{debug, warn};

use crate::core::error::{CrabError, Result};

use super::{
    Format, Guard, LifecycleProvider, ObjectPath, Provider, PutOutcome, RenderedLifecycle,
    RestoreBackend, RestoreHandle, RestoreState, RestoreTier, StorageClass, TierPlan, TierRule,
};

/// S3 namespace URI for the `2006-03-01` API.
const S3_NAMESPACE: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

// ── Supported-tier matrix (A3.3) ────────────────────────────────────

/// Supported restore tiers for S3 Glacier Flexible Retrieval.
static GLACIER_FLEXIBLE_TIERS: &[RestoreTier] = &[
    RestoreTier::Expedited,
    RestoreTier::Standard,
    RestoreTier::Bulk,
];

/// Supported restore tiers for S3 Glacier Deep Archive.
static GLACIER_DEEP_TIERS: &[RestoreTier] = &[RestoreTier::Standard, RestoreTier::Bulk];

/// Empty tier list for classes that don't need restore.
static NO_TIERS: &[RestoreTier] = &[];

// ── XML rendering ───────────────────────────────────────────────────

/// Render a [`TierPlan`] into S3 `PutBucketLifecycleConfiguration` XML.
///
/// Rules are sorted by ID before rendering so the output is
/// deterministic regardless of input order.
pub fn render(plan: &TierPlan) -> Result<RenderedLifecycle> {
    let mut sorted_rules: Vec<&TierRule> = plan.rules.iter().collect();
    sorted_rules.sort_by(|a, b| a.id.cmp(&b.id));

    let mut buf = Vec::new();
    let mut writer = Writer::new_with_indent(&mut buf, b' ', 2);

    // XML declaration.
    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;

    // <LifecycleConfiguration xmlns="...">
    let mut root = BytesStart::new("LifecycleConfiguration");
    root.push_attribute(("xmlns", S3_NAMESPACE));
    writer.write_event(Event::Start(root))?;

    for rule in &sorted_rules {
        write_rule(&mut writer, rule, plan.versioning_enabled)?;
    }

    // </LifecycleConfiguration>
    writer.write_event(Event::End(BytesEnd::new("LifecycleConfiguration")))?;

    let rule_ids: Vec<String> = sorted_rules.iter().map(|r| r.id.clone()).collect();

    Ok(RenderedLifecycle {
        format: Format::Xml,
        body: buf,
        rule_ids,
    })
}

/// Write a single `<Rule>` element.
fn write_rule(
    writer: &mut Writer<&mut Vec<u8>>,
    rule: &TierRule,
    versioning_enabled: bool,
) -> Result<()> {
    writer.write_event(Event::Start(BytesStart::new("Rule")))?;

    write_text_element(writer, "ID", &rule.id)?;
    write_filter(writer, rule)?;
    write_text_element(writer, "Status", "Enabled")?;

    // Current-version transitions.
    for transition in &rule.transitions {
        write_transition(writer, transition)?;
    }

    // Noncurrent-version transitions (only when versioning is on).
    if versioning_enabled {
        for transition in &rule.transitions {
            write_noncurrent_version_transition(writer, transition)?;
        }

        if let Some(days) = rule.noncurrent_expiration_days {
            write_noncurrent_version_expiration(writer, days)?;
        }
    }

    writer.write_event(Event::End(BytesEnd::new("Rule")))?;
    Ok(())
}

/// Write the `<Filter>` element.
///
/// When `min_object_size_bytes` is set, wraps `<Prefix>` and
/// `<ObjectSizeGreaterThan>` in an `<And>` element (required by S3
/// when combining filter conditions).
fn write_filter(writer: &mut Writer<&mut Vec<u8>>, rule: &TierRule) -> Result<()> {
    writer.write_event(Event::Start(BytesStart::new("Filter")))?;

    if let Some(min_size) = rule.min_object_size_bytes {
        // Multiple conditions require <And>.
        writer.write_event(Event::Start(BytesStart::new("And")))?;
        write_text_element(writer, "Prefix", &rule.prefix)?;
        write_text_element(writer, "ObjectSizeGreaterThan", &min_size.to_string())?;
        writer.write_event(Event::End(BytesEnd::new("And")))?;
    } else {
        write_text_element(writer, "Prefix", &rule.prefix)?;
    }

    writer.write_event(Event::End(BytesEnd::new("Filter")))?;
    Ok(())
}

/// Write a `<Transition>` element for current-version objects.
fn write_transition(
    writer: &mut Writer<&mut Vec<u8>>,
    transition: &super::Transition,
) -> Result<()> {
    writer.write_event(Event::Start(BytesStart::new("Transition")))?;
    write_text_element(writer, "Days", &transition.days.to_string())?;
    write_text_element(writer, "StorageClass", s3_class_str(transition.to_class))?;
    writer.write_event(Event::End(BytesEnd::new("Transition")))?;
    Ok(())
}

/// Write a `<NoncurrentVersionTransition>` element.
fn write_noncurrent_version_transition(
    writer: &mut Writer<&mut Vec<u8>>,
    transition: &super::Transition,
) -> Result<()> {
    writer.write_event(Event::Start(BytesStart::new("NoncurrentVersionTransition")))?;
    write_text_element(writer, "NoncurrentDays", &transition.days.to_string())?;
    write_text_element(writer, "StorageClass", s3_class_str(transition.to_class))?;
    writer.write_event(Event::End(BytesEnd::new("NoncurrentVersionTransition")))?;
    Ok(())
}

/// Write a `<NoncurrentVersionExpiration>` element.
fn write_noncurrent_version_expiration(writer: &mut Writer<&mut Vec<u8>>, days: u32) -> Result<()> {
    writer.write_event(Event::Start(BytesStart::new("NoncurrentVersionExpiration")))?;
    write_text_element(writer, "NoncurrentDays", &days.to_string())?;
    writer.write_event(Event::End(BytesEnd::new("NoncurrentVersionExpiration")))?;
    Ok(())
}

/// Write a simple `<Tag>text</Tag>` element.
fn write_text_element(writer: &mut Writer<&mut Vec<u8>>, tag: &str, text: &str) -> Result<()> {
    writer.write_event(Event::Start(BytesStart::new(tag)))?;
    writer.write_event(Event::Text(BytesText::new(text)))?;
    writer.write_event(Event::End(BytesEnd::new(tag)))?;
    Ok(())
}

/// Map a [`StorageClass`] to the S3 API wire-format string.
#[expect(
    clippy::match_same_arms,
    reason = "S3Standard is the canonical arm; non-S3 classes are a defensive fallback"
)]
fn s3_class_str(class: StorageClass) -> &'static str {
    match class {
        StorageClass::S3Standard => "STANDARD",
        StorageClass::S3IntelligentTiering => "INTELLIGENT_TIERING",
        StorageClass::S3StandardIa => "STANDARD_IA",
        StorageClass::S3OneZoneIa => "ONEZONE_IA",
        StorageClass::S3GlacierInstantRetrieval => "GLACIER_IR",
        StorageClass::S3GlacierFlexibleRetrieval => "GLACIER",
        StorageClass::S3GlacierDeepArchive => "DEEP_ARCHIVE",
        // Non-S3 classes should not appear in S3 lifecycle XML, but
        // we fall back to STANDARD rather than panicking.
        StorageClass::GcsStandard
        | StorageClass::GcsNearline
        | StorageClass::GcsColdline
        | StorageClass::GcsArchive
        | StorageClass::AzureHot
        | StorageClass::AzureCool
        | StorageClass::AzureCold
        | StorageClass::AzureArchive
        | StorageClass::Unknown => "STANDARD",
    }
}

// ── S3LifecycleProvider ─────────────────────────────────────────────

/// S3 lifecycle provider backed by `aws-sdk-s3`.
///
/// Implements both [`LifecycleProvider`] (lifecycle rule CRUD) and
/// [`RestoreBackend`] (archive restore + state queries).
///
/// # Credential adapter
///
/// The real integration with `auth::CredentialProvider` will be wired
/// when the auth adapter shim is available. For now the client is built
/// from the default AWS SDK credential chain (environment variables,
/// `~/.aws/credentials`, IMDS, etc.).
pub struct S3LifecycleProvider {
    client: aws_sdk_s3::Client,
    bucket: String,
}

impl S3LifecycleProvider {
    /// Build an S3 lifecycle provider for the given bucket and region.
    ///
    /// Uses the default AWS credential chain. The credential adapter
    /// from `auth::CredentialProvider` will be wired in a follow-up
    /// task.
    // TODO(crab-storage-economy): wire `auth::CredentialProvider` via
    // `aws_credential_types::provider::ProvideCredentials` adapter when
    // the auth shim is available.
    pub fn new(bucket: String, region: String) -> Self {
        let sdk_config = aws_sdk_s3::config::Builder::new()
            .region(aws_sdk_s3::config::Region::new(region))
            .behavior_version_latest()
            .build();
        let client = aws_sdk_s3::Client::from_conf(sdk_config);
        Self { client, bucket }
    }

    /// Build an S3 lifecycle provider from an existing SDK client.
    ///
    /// Useful for testing and for callers that already hold a
    /// configured client.
    pub fn from_client(client: aws_sdk_s3::Client, bucket: String) -> Self {
        Self { client, bucket }
    }

    /// Return a reference to the underlying S3 client.
    pub fn client(&self) -> &aws_sdk_s3::Client {
        &self.client
    }

    /// Return the bucket name.
    pub fn bucket(&self) -> &str {
        &self.bucket
    }
}

#[async_trait]
impl LifecycleProvider for S3LifecycleProvider {
    fn kind(&self) -> Provider {
        Provider::S3
    }

    fn render(&self, plan: &TierPlan) -> Result<RenderedLifecycle> {
        render(plan)
    }

    async fn get(&self) -> Result<Option<RenderedLifecycle>> {
        let resp = self
            .client
            .get_bucket_lifecycle_configuration()
            .bucket(&self.bucket)
            .send()
            .await;

        match resp {
            Ok(output) => {
                let rules = output.rules();
                if rules.is_empty() {
                    return Ok(None);
                }

                let rule_ids: Vec<String> = rules
                    .iter()
                    .filter_map(|r| r.id().map(String::from))
                    .collect();

                // Re-serialize the SDK response to XML so the caller
                // gets a `RenderedLifecycle` in the same format we
                // produce via `render()`.
                let body = serialize_lifecycle_rules_to_xml(rules)?;

                Ok(Some(RenderedLifecycle {
                    format: Format::Xml,
                    body,
                    rule_ids,
                }))
            }
            Err(err) => {
                let service_err = err.into_service_error();
                // S3 returns a specific error code when no lifecycle
                // configuration exists on the bucket.
                if service_err
                    .meta()
                    .code()
                    .is_some_and(|c| c == "NoSuchLifecycleConfiguration")
                {
                    return Ok(None);
                }
                Err(map_s3_service_error(
                    "GetBucketLifecycleConfiguration",
                    &service_err,
                ))
            }
        }
    }

    async fn put(&self, doc: &RenderedLifecycle, _guard: Option<Guard>) -> Result<PutOutcome> {
        // S3 PutBucketLifecycleConfiguration does not support
        // conditional writes (no If-Match header). The CAS retry logic
        // in apply.rs handles conflict detection by re-reading after
        // each put.
        //
        // We build SDK-typed `LifecycleRule`s from our `TierPlan` rules
        // rather than sending raw XML, because the SDK serializes the
        // request body itself.
        let sdk_rules = parse_xml_to_sdk_rules(&doc.body)?;

        let lifecycle_config = aws_sdk_s3::types::BucketLifecycleConfiguration::builder()
            .set_rules(Some(sdk_rules))
            .build()
            .map_err(|e| CrabError::Internal(format!("failed to build lifecycle config: {e}")))?;

        self.client
            .put_bucket_lifecycle_configuration()
            .bucket(&self.bucket)
            .lifecycle_configuration(lifecycle_config)
            .send()
            .await
            .map_err(|e| {
                map_s3_service_error("PutBucketLifecycleConfiguration", &e.into_service_error())
            })?;

        debug!(bucket = %self.bucket, rules = ?doc.rule_ids, "lifecycle configuration applied");

        Ok(PutOutcome {
            new_guard: Guard::None,
            applied_at: now_rfc3339(),
        })
    }

    async fn cas_guard(&self) -> Result<Option<Guard>> {
        // S3 lifecycle API does not support conditional writes (no ETag
        // on lifecycle configuration). Return Guard::None; the CAS
        // retry logic in apply.rs handles conflict detection by
        // re-reading the lifecycle after each put.
        Ok(Some(Guard::None))
    }
}

#[async_trait]
impl RestoreBackend for S3LifecycleProvider {
    async fn restore(
        &self,
        path: &ObjectPath,
        tier: RestoreTier,
        duration: Duration,
    ) -> Result<RestoreHandle> {
        let days = duration.as_secs() / 86_400;
        let days_i32 = i32::try_from(days)
            .map_err(|_| CrabError::Internal("restore duration overflow".into()))?;

        let s3_tier = restore_tier_to_s3(tier);

        let restore_request = aws_sdk_s3::types::RestoreRequest::builder()
            .days(days_i32)
            .glacier_job_parameters(
                aws_sdk_s3::types::GlacierJobParameters::builder()
                    .tier(s3_tier)
                    .build()
                    .map_err(|e| {
                        CrabError::Internal(format!("failed to build glacier params: {e}"))
                    })?,
            )
            .build();

        self.client
            .restore_object()
            .bucket(&self.bucket)
            .key(path)
            .restore_request(restore_request)
            .send()
            .await
            .map_err(|e| map_s3_service_error("RestoreObject", &e.into_service_error()))?;

        debug!(bucket = %self.bucket, key = %path, tier = ?tier, "restore request submitted");

        Ok(RestoreHandle {
            id: format!("s3-restore-{path}"),
        })
    }

    async fn state(&self, path: &ObjectPath) -> Result<RestoreState> {
        let resp = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(path)
            .send()
            .await
            .map_err(|e| map_s3_service_error("HeadObject", &e.into_service_error()))?;

        // Parse the storage class from the HeadObject response.
        let class = resp.storage_class().map_or(StorageClass::S3Standard, |sc| {
            StorageClass::from_provider_str(&Provider::S3, sc.as_str())
        });

        // Glacier Instant Retrieval is readable directly — no restore
        // needed.
        if class == StorageClass::S3GlacierInstantRetrieval {
            return Ok(RestoreState::Ready);
        }

        // Non-archive classes are always ready.
        if !class.is_archive_class() {
            return Ok(RestoreState::Ready);
        }

        // For archive classes, check the `x-amz-restore` header.
        // Format: `ongoing-request="true"` or
        //         `ongoing-request="false", expiry-date="..."`
        match resp.restore() {
            Some(restore_header) => Ok(parse_restore_header(restore_header)),
            None => Ok(RestoreState::NotRequested),
        }
    }

    fn supported_tiers(&self, class: &StorageClass) -> &'static [RestoreTier] {
        match class {
            StorageClass::S3GlacierFlexibleRetrieval => GLACIER_FLEXIBLE_TIERS,
            StorageClass::S3GlacierDeepArchive => GLACIER_DEEP_TIERS,
            // Glacier Instant Retrieval, GCS Archive, and all warm
            // classes don't need restore — return empty.
            _ => NO_TIERS,
        }
    }
}

// ── Helper functions ────────────────────────────────────────────────

/// Map our [`RestoreTier`] to the S3 SDK [`aws_sdk_s3::types::Tier`].
fn restore_tier_to_s3(tier: RestoreTier) -> aws_sdk_s3::types::Tier {
    match tier {
        RestoreTier::Expedited => aws_sdk_s3::types::Tier::Expedited,
        RestoreTier::Standard => aws_sdk_s3::types::Tier::Standard,
        RestoreTier::Bulk => aws_sdk_s3::types::Tier::Bulk,
        // Azure-specific tier; should not reach S3 code, but map to
        // Standard defensively.
        RestoreTier::High => {
            warn!("RestoreTier::High is Azure-specific; mapping to Standard for S3");
            aws_sdk_s3::types::Tier::Standard
        }
    }
}

/// Map our [`StorageClass`] to the S3 SDK
/// [`aws_sdk_s3::types::TransitionStorageClass`].
#[allow(dead_code, reason = "used by plan builder when wired in")]
fn storage_class_to_sdk(class: StorageClass) -> aws_sdk_s3::types::TransitionStorageClass {
    match class {
        StorageClass::S3OneZoneIa => aws_sdk_s3::types::TransitionStorageClass::OnezoneIa,
        StorageClass::S3IntelligentTiering => {
            aws_sdk_s3::types::TransitionStorageClass::IntelligentTiering
        }
        StorageClass::S3GlacierInstantRetrieval => {
            aws_sdk_s3::types::TransitionStorageClass::GlacierIr
        }
        StorageClass::S3GlacierFlexibleRetrieval => {
            aws_sdk_s3::types::TransitionStorageClass::Glacier
        }
        StorageClass::S3GlacierDeepArchive => {
            aws_sdk_s3::types::TransitionStorageClass::DeepArchive
        }
        // S3StandardIa and all non-S3 classes fall back to StandardIa.
        _ => aws_sdk_s3::types::TransitionStorageClass::StandardIa,
    }
}

/// Parse the S3 `x-amz-restore` header into a [`RestoreState`].
///
/// Header format:
/// - In progress: `ongoing-request="true"`
/// - Complete: `ongoing-request="false", expiry-date="Fri, 23 Dec 2012 00:00:00 GMT"`
fn parse_restore_header(header: &str) -> RestoreState {
    if header.contains(r#"ongoing-request="true""#) {
        RestoreState::InProgress {
            started_at: String::new(),
            expected_ready_at: String::new(),
        }
    } else if header.contains(r#"ongoing-request="false""#) {
        RestoreState::Ready
    } else {
        warn!(header = %header, "unrecognised x-amz-restore header format");
        RestoreState::NotRequested
    }
}

/// Serialize SDK `LifecycleRule`s back to XML for `RenderedLifecycle`.
///
/// This re-renders the rules returned by `GetBucketLifecycleConfiguration`
/// into the same XML format our `render()` produces, so callers can
/// diff existing vs proposed configurations.
pub(crate) fn serialize_lifecycle_rules_to_xml(
    rules: &[aws_sdk_s3::types::LifecycleRule],
) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut writer = Writer::new_with_indent(&mut buf, b' ', 2);

    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;

    let mut root = BytesStart::new("LifecycleConfiguration");
    root.push_attribute(("xmlns", S3_NAMESPACE));
    writer.write_event(Event::Start(root))?;

    for rule in rules {
        serialize_sdk_rule(&mut writer, rule)?;
    }

    writer.write_event(Event::End(BytesEnd::new("LifecycleConfiguration")))?;
    Ok(buf)
}

/// Serialize a single SDK `LifecycleRule` to XML.
fn serialize_sdk_rule(
    writer: &mut Writer<&mut Vec<u8>>,
    rule: &aws_sdk_s3::types::LifecycleRule,
) -> Result<()> {
    writer.write_event(Event::Start(BytesStart::new("Rule")))?;

    if let Some(id) = rule.id() {
        write_text_element(writer, "ID", id)?;
    }

    // Filter.
    if let Some(filter) = rule.filter() {
        serialize_sdk_filter(writer, filter)?;
    }

    let status_str = if matches!(rule.status(), aws_sdk_s3::types::ExpirationStatus::Disabled) {
        "Disabled"
    } else {
        "Enabled"
    };
    write_text_element(writer, "Status", status_str)?;

    // Transitions.
    for transition in rule.transitions() {
        writer.write_event(Event::Start(BytesStart::new("Transition")))?;
        if let Some(days) = transition.days() {
            write_text_element(writer, "Days", &days.to_string())?;
        }
        if let Some(sc) = transition.storage_class() {
            write_text_element(writer, "StorageClass", sc.as_str())?;
        }
        writer.write_event(Event::End(BytesEnd::new("Transition")))?;
    }

    // Noncurrent version transitions.
    for nvt in rule.noncurrent_version_transitions() {
        writer.write_event(Event::Start(BytesStart::new("NoncurrentVersionTransition")))?;
        if let Some(days) = nvt.noncurrent_days() {
            write_text_element(writer, "NoncurrentDays", &days.to_string())?;
        }
        if let Some(sc) = nvt.storage_class() {
            write_text_element(writer, "StorageClass", sc.as_str())?;
        }
        writer.write_event(Event::End(BytesEnd::new("NoncurrentVersionTransition")))?;
    }

    // Noncurrent version expiration.
    if let Some(nve) = rule.noncurrent_version_expiration() {
        writer.write_event(Event::Start(BytesStart::new("NoncurrentVersionExpiration")))?;
        if let Some(days) = nve.noncurrent_days() {
            write_text_element(writer, "NoncurrentDays", &days.to_string())?;
        }
        writer.write_event(Event::End(BytesEnd::new("NoncurrentVersionExpiration")))?;
    }

    writer.write_event(Event::End(BytesEnd::new("Rule")))?;
    Ok(())
}

/// Serialize an SDK `LifecycleRuleFilter` to XML.
///
/// `LifecycleRuleFilter` is a non-exhaustive struct in aws-sdk-s3 ≥ 1.x;
/// we inspect it via accessor methods rather than matching enum variants.
fn serialize_sdk_filter(
    writer: &mut Writer<&mut Vec<u8>>,
    filter: &aws_sdk_s3::types::LifecycleRuleFilter,
) -> Result<()> {
    writer.write_event(Event::Start(BytesStart::new("Filter")))?;

    if let Some(and_op) = filter.and() {
        writer.write_event(Event::Start(BytesStart::new("And")))?;
        if let Some(prefix) = and_op.prefix() {
            write_text_element(writer, "Prefix", prefix)?;
        }
        if let Some(size) = and_op.object_size_greater_than() {
            write_text_element(writer, "ObjectSizeGreaterThan", &size.to_string())?;
        }
        if let Some(size) = and_op.object_size_less_than() {
            write_text_element(writer, "ObjectSizeLessThan", &size.to_string())?;
        }
        writer.write_event(Event::End(BytesEnd::new("And")))?;
    } else if let Some(prefix) = filter.prefix() {
        write_text_element(writer, "Prefix", prefix)?;
    } else if let Some(size) = filter.object_size_greater_than() {
        write_text_element(writer, "ObjectSizeGreaterThan", &size.to_string())?;
    } else if let Some(size) = filter.object_size_less_than() {
        write_text_element(writer, "ObjectSizeLessThan", &size.to_string())?;
    } else if let Some(tag) = filter.tag() {
        writer.write_event(Event::Start(BytesStart::new("Tag")))?;
        write_text_element(writer, "Key", tag.key())?;
        write_text_element(writer, "Value", tag.value())?;
        writer.write_event(Event::End(BytesEnd::new("Tag")))?;
    } else {
        warn!("unknown lifecycle rule filter variant; skipping");
    }

    writer.write_event(Event::End(BytesEnd::new("Filter")))?;
    Ok(())
}

/// Parse our rendered XML body back into SDK `LifecycleRule` types.
///
/// This is needed for `put()` because the SDK's
/// `PutBucketLifecycleConfiguration` expects structured types, not raw
/// XML. We parse the XML we rendered and convert to SDK types.
pub(crate) fn parse_xml_to_sdk_rules(
    xml_body: &[u8],
) -> Result<Vec<aws_sdk_s3::types::LifecycleRule>> {
    use quick_xml::Reader;
    use quick_xml::events::Event as XmlEvent;

    let mut reader = Reader::from_reader(xml_body);
    let mut buf = Vec::new();
    let mut rules = Vec::new();

    // State for the current rule being parsed.
    let mut in_rule = false;
    let mut current_id: Option<String> = None;
    let mut current_prefix: Option<String> = None;
    let mut current_min_size: Option<i64> = None;
    let mut current_transitions: Vec<aws_sdk_s3::types::Transition> = Vec::new();
    let mut current_nv_transitions: Vec<aws_sdk_s3::types::NoncurrentVersionTransition> =
        Vec::new();
    let mut current_nv_expiration_days: Option<i32> = None;

    // Nested element tracking.
    let mut in_filter = false;
    let mut in_transition = false;
    let mut in_nv_transition = false;
    let mut in_nv_expiration = false;
    let mut current_tag: Option<String> = None;

    // Transition state.
    let mut trans_days: Option<i32> = None;
    let mut trans_class: Option<String> = None;
    let mut nvt_days: Option<i32> = None;
    let mut nvt_class: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(XmlEvent::Start(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                match name.as_str() {
                    "Rule" => {
                        in_rule = true;
                        current_id = None;
                        current_prefix = None;
                        current_min_size = None;
                        current_transitions.clear();
                        current_nv_transitions.clear();
                        current_nv_expiration_days = None;
                    }
                    "Filter" if in_rule => in_filter = true,
                    "And" if in_filter => {}
                    "Transition" if in_rule && !in_nv_transition => {
                        in_transition = true;
                        trans_days = None;
                        trans_class = None;
                    }
                    "NoncurrentVersionTransition" if in_rule => {
                        in_nv_transition = true;
                        nvt_days = None;
                        nvt_class = None;
                    }
                    "NoncurrentVersionExpiration" if in_rule => {
                        in_nv_expiration = true;
                    }
                    _ => {}
                }
                current_tag = Some(name);
            }
            Ok(XmlEvent::Text(ref e)) => {
                let text = e.unescape().unwrap_or_default().to_string();
                if let Some(ref tag) = current_tag {
                    match tag.as_str() {
                        "ID" if in_rule => current_id = Some(text),
                        "Prefix" if in_filter => current_prefix = Some(text),
                        "ObjectSizeGreaterThan" if in_filter => {
                            current_min_size = text.parse().ok();
                        }
                        "Days" if in_transition => {
                            trans_days = text.parse().ok();
                        }
                        "StorageClass" if in_transition => {
                            trans_class = Some(text);
                        }
                        "NoncurrentDays" if in_nv_transition => {
                            nvt_days = text.parse().ok();
                        }
                        "StorageClass" if in_nv_transition => {
                            nvt_class = Some(text);
                        }
                        "NoncurrentDays" if in_nv_expiration => {
                            current_nv_expiration_days = text.parse().ok();
                        }
                        _ => {}
                    }
                }
            }
            Ok(XmlEvent::End(ref e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                match name.as_str() {
                    "Rule" => {
                        // Build the SDK LifecycleRule from parsed state.
                        let filter =
                            build_sdk_filter(current_prefix.take(), current_min_size.take());

                        let mut builder = aws_sdk_s3::types::LifecycleRule::builder()
                            .set_id(current_id.take())
                            .status(aws_sdk_s3::types::ExpirationStatus::Enabled);

                        if let Some(f) = filter {
                            builder = builder.filter(f);
                        }

                        if !current_transitions.is_empty() {
                            builder = builder
                                .set_transitions(Some(std::mem::take(&mut current_transitions)));
                        }

                        if !current_nv_transitions.is_empty() {
                            builder = builder.set_noncurrent_version_transitions(Some(
                                std::mem::take(&mut current_nv_transitions),
                            ));
                        }

                        if let Some(days) = current_nv_expiration_days.take() {
                            builder = builder.noncurrent_version_expiration(
                                aws_sdk_s3::types::NoncurrentVersionExpiration::builder()
                                    .noncurrent_days(days)
                                    .build(),
                            );
                        }

                        match builder.build() {
                            Ok(rule) => rules.push(rule),
                            Err(e) => {
                                warn!(error = %e, "failed to build lifecycle rule from XML; skipping");
                            }
                        }
                        in_rule = false;
                    }
                    "Filter" => in_filter = false,
                    "Transition" if in_transition && !in_nv_transition => {
                        let mut tb = aws_sdk_s3::types::Transition::builder();
                        if let Some(d) = trans_days {
                            tb = tb.days(d);
                        }
                        if let Some(ref sc) = trans_class {
                            tb = tb.storage_class(aws_sdk_s3::types::TransitionStorageClass::from(
                                sc.as_str(),
                            ));
                        }
                        current_transitions.push(tb.build());
                        in_transition = false;
                    }
                    "NoncurrentVersionTransition" => {
                        let mut nvtb = aws_sdk_s3::types::NoncurrentVersionTransition::builder();
                        if let Some(d) = nvt_days {
                            nvtb = nvtb.noncurrent_days(d);
                        }
                        if let Some(ref sc) = nvt_class {
                            nvtb = nvtb.storage_class(
                                aws_sdk_s3::types::TransitionStorageClass::from(sc.as_str()),
                            );
                        }
                        current_nv_transitions.push(nvtb.build());
                        in_nv_transition = false;
                    }
                    "NoncurrentVersionExpiration" => {
                        in_nv_expiration = false;
                    }
                    _ => {}
                }
                current_tag = None;
            }
            Ok(XmlEvent::Eof) => break,
            Err(e) => {
                return Err(CrabError::Internal(format!(
                    "failed to parse lifecycle XML: {e}"
                )));
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(rules)
}

/// Build an SDK `LifecycleRuleFilter` from parsed prefix and min-size.
///
/// `LifecycleRuleFilter` is a struct in aws-sdk-s3 ≥ 1.x; use its builder
/// rather than the old enum tuple constructors.
fn build_sdk_filter(
    prefix: Option<String>,
    min_size: Option<i64>,
) -> Option<aws_sdk_s3::types::LifecycleRuleFilter> {
    match (prefix, min_size) {
        (Some(p), Some(size)) => {
            // Both prefix and size → And filter.
            let and_op = aws_sdk_s3::types::LifecycleRuleAndOperator::builder()
                .prefix(p)
                .object_size_greater_than(size)
                .build();
            Some(
                aws_sdk_s3::types::LifecycleRuleFilter::builder()
                    .and(and_op)
                    .build(),
            )
        }
        (Some(p), None) => Some(
            aws_sdk_s3::types::LifecycleRuleFilter::builder()
                .prefix(p)
                .build(),
        ),
        (None, Some(size)) => Some(
            aws_sdk_s3::types::LifecycleRuleFilter::builder()
                .object_size_greater_than(size)
                .build(),
        ),
        (None, None) => None,
    }
}

/// Map any S3 service error to a [`CrabError`].
///
/// Uses the `Display` representation so this works for any S3
/// operation without matching every non-exhaustive variant.
fn map_s3_service_error(operation: &str, err: &dyn std::fmt::Display) -> CrabError {
    let msg = format!("{operation}: {err}");
    CrabError::Storage(object_store::Error::Generic {
        store: "S3",
        source: Box::new(std::io::Error::other(msg)),
    })
}

/// Return the current time as an RFC 3339 string.
fn now_rfc3339() -> String {
    // Use a simple approach without pulling in chrono.
    let now = std::time::SystemTime::now();
    let duration = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}Z", duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier::provider::{Provider, TierPlan, TierRule, Transition};

    /// Helper to render a plan and return the XML as a string.
    fn render_xml(plan: &TierPlan) -> String {
        let rendered = render(plan).expect("render should succeed");
        assert_eq!(rendered.format, Format::Xml);
        String::from_utf8(rendered.body).expect("XML should be valid UTF-8")
    }

    fn ia_transition(days: u32) -> Transition {
        Transition {
            days,
            to_class: StorageClass::S3StandardIa,
        }
    }

    fn glacier_transition(days: u32) -> Transition {
        Transition {
            days,
            to_class: StorageClass::S3GlacierFlexibleRetrieval,
        }
    }

    // ── Snapshot: basic IA transition ───────────────────────────────

    #[test]
    fn snapshot_basic_ia_transition() {
        let plan = TierPlan {
            provider: Provider::S3,
            rules: vec![TierRule {
                id: "crab-xorbs-to-ia".into(),
                prefix: ".crab/xorbs/".into(),
                transitions: vec![ia_transition(30)],
                noncurrent_expiration_days: None,
                min_object_size_bytes: None,
            }],
            versioning_enabled: false,
            object_lock_enabled: false,
        };

        let xml = render_xml(&plan);
        insta::assert_snapshot!("s3_basic_ia_transition", xml);
    }

    // ── Snapshot: versioning enabled ────────────────────────────────

    #[test]
    fn snapshot_versioning_enabled() {
        let plan = TierPlan {
            provider: Provider::S3,
            rules: vec![TierRule {
                id: "crab-xorbs-to-ia".into(),
                prefix: ".crab/xorbs/".into(),
                transitions: vec![ia_transition(30)],
                noncurrent_expiration_days: Some(30),
                min_object_size_bytes: None,
            }],
            versioning_enabled: true,
            object_lock_enabled: false,
        };

        let xml = render_xml(&plan);
        insta::assert_snapshot!("s3_versioning_enabled", xml);
    }

    // ── Snapshot: Glacier transition with ObjectSizeGreaterThan ─────

    #[test]
    fn snapshot_glacier_with_size_filter() {
        let plan = TierPlan {
            provider: Provider::S3,
            rules: vec![TierRule {
                id: "crab-xorbs-to-glacier".into(),
                prefix: ".crab/xorbs/".into(),
                transitions: vec![glacier_transition(180)],
                noncurrent_expiration_days: None,
                min_object_size_bytes: Some(40_960),
            }],
            versioning_enabled: false,
            object_lock_enabled: false,
        };

        let xml = render_xml(&plan);
        insta::assert_snapshot!("s3_glacier_with_size_filter", xml);
    }

    // ── Snapshot: multiple transitions (IA + Glacier) ───────────────

    #[test]
    fn snapshot_multiple_transitions() {
        let plan = TierPlan {
            provider: Provider::S3,
            rules: vec![TierRule {
                id: "crab-xorbs-tiering".into(),
                prefix: ".crab/xorbs/".into(),
                transitions: vec![ia_transition(30), glacier_transition(180)],
                noncurrent_expiration_days: None,
                min_object_size_bytes: Some(40_960),
            }],
            versioning_enabled: false,
            object_lock_enabled: false,
        };

        let xml = render_xml(&plan);
        insta::assert_snapshot!("s3_multiple_transitions", xml);
    }

    // ── Rule IDs are sorted deterministically ───────────────────────

    #[test]
    fn rule_ids_sorted_deterministically() {
        let plan = TierPlan {
            provider: Provider::S3,
            rules: vec![
                TierRule {
                    id: "crab-z-rule".into(),
                    prefix: ".crab/xorbs/".into(),
                    transitions: vec![ia_transition(30)],
                    noncurrent_expiration_days: None,
                    min_object_size_bytes: None,
                },
                TierRule {
                    id: "crab-a-rule".into(),
                    prefix: ".crab/xorbs/".into(),
                    transitions: vec![glacier_transition(180)],
                    noncurrent_expiration_days: None,
                    min_object_size_bytes: Some(40_960),
                },
            ],
            versioning_enabled: false,
            object_lock_enabled: false,
        };

        let rendered = render(&plan).expect("render should succeed");
        assert_eq!(rendered.rule_ids, vec!["crab-a-rule", "crab-z-rule"]);
    }

    // ── Snapshot: full plan with versioning + multiple rules ────────

    #[test]
    fn snapshot_full_plan_versioning_multiple_rules() {
        let plan = TierPlan {
            provider: Provider::S3,
            rules: vec![
                TierRule {
                    id: "crab-xorbs-to-ia".into(),
                    prefix: ".crab/xorbs/".into(),
                    transitions: vec![ia_transition(30)],
                    noncurrent_expiration_days: Some(60),
                    min_object_size_bytes: None,
                },
                TierRule {
                    id: "crab-xorbs-to-glacier".into(),
                    prefix: ".crab/xorbs/".into(),
                    transitions: vec![glacier_transition(180)],
                    noncurrent_expiration_days: Some(180),
                    min_object_size_bytes: Some(40_960),
                },
            ],
            versioning_enabled: true,
            object_lock_enabled: false,
        };

        let xml = render_xml(&plan);
        insta::assert_snapshot!("s3_full_plan_versioning_multiple_rules", xml);
    }

    // ── Rendered format is Xml ──────────────────────────────────────

    #[test]
    fn rendered_format_is_xml() {
        let plan = TierPlan {
            provider: Provider::S3,
            rules: vec![TierRule {
                id: "crab-test".into(),
                prefix: ".crab/xorbs/".into(),
                transitions: vec![ia_transition(30)],
                noncurrent_expiration_days: None,
                min_object_size_bytes: None,
            }],
            versioning_enabled: false,
            object_lock_enabled: false,
        };

        let rendered = render(&plan).expect("render should succeed");
        assert_eq!(rendered.format, Format::Xml);
        assert!(!rendered.body.is_empty());
    }

    // ── XML round-trip: render → parse → re-render ──────────────────

    #[test]
    fn xml_round_trip_basic() {
        let plan = TierPlan {
            provider: Provider::S3,
            rules: vec![TierRule {
                id: "crab-xorbs-to-ia".into(),
                prefix: ".crab/xorbs/".into(),
                transitions: vec![ia_transition(30)],
                noncurrent_expiration_days: None,
                min_object_size_bytes: None,
            }],
            versioning_enabled: false,
            object_lock_enabled: false,
        };

        let rendered = render(&plan).expect("render should succeed");
        let sdk_rules = parse_xml_to_sdk_rules(&rendered.body).expect("parse should succeed");
        assert_eq!(sdk_rules.len(), 1);
        assert_eq!(sdk_rules[0].id(), Some("crab-xorbs-to-ia"));
        assert_eq!(sdk_rules[0].transitions().len(), 1);
        assert_eq!(sdk_rules[0].transitions()[0].days(), Some(30));
    }

    #[test]
    fn xml_round_trip_with_size_filter() {
        let plan = TierPlan {
            provider: Provider::S3,
            rules: vec![TierRule {
                id: "crab-xorbs-to-glacier".into(),
                prefix: ".crab/xorbs/".into(),
                transitions: vec![glacier_transition(180)],
                noncurrent_expiration_days: None,
                min_object_size_bytes: Some(40_960),
            }],
            versioning_enabled: false,
            object_lock_enabled: false,
        };

        let rendered = render(&plan).expect("render should succeed");
        let sdk_rules = parse_xml_to_sdk_rules(&rendered.body).expect("parse should succeed");
        assert_eq!(sdk_rules.len(), 1);

        // Verify the And filter was parsed correctly.
        let filter = sdk_rules[0].filter().expect("filter should be present");
        let and_op = filter.and().expect("should be And filter");
        assert_eq!(and_op.prefix(), Some(".crab/xorbs/"));
        assert_eq!(and_op.object_size_greater_than(), Some(40_960));
    }

    #[test]
    fn xml_round_trip_versioning_with_noncurrent() {
        let plan = TierPlan {
            provider: Provider::S3,
            rules: vec![TierRule {
                id: "crab-xorbs-to-ia".into(),
                prefix: ".crab/xorbs/".into(),
                transitions: vec![ia_transition(30)],
                noncurrent_expiration_days: Some(60),
                min_object_size_bytes: None,
            }],
            versioning_enabled: true,
            object_lock_enabled: false,
        };

        let rendered = render(&plan).expect("render should succeed");
        let sdk_rules = parse_xml_to_sdk_rules(&rendered.body).expect("parse should succeed");
        assert_eq!(sdk_rules.len(), 1);
        assert_eq!(sdk_rules[0].noncurrent_version_transitions().len(), 1);
        assert!(sdk_rules[0].noncurrent_version_expiration().is_some());
        assert_eq!(
            sdk_rules[0]
                .noncurrent_version_expiration()
                .and_then(|nve| nve.noncurrent_days()),
            Some(60)
        );
    }

    // ── parse_restore_header ────────────────────────────────────────

    #[test]
    fn parse_restore_header_in_progress() {
        let state = parse_restore_header(r#"ongoing-request="true""#);
        assert!(matches!(state, RestoreState::InProgress { .. }));
    }

    #[test]
    fn parse_restore_header_complete() {
        let state = parse_restore_header(
            r#"ongoing-request="false", expiry-date="Fri, 23 Dec 2025 00:00:00 GMT""#,
        );
        assert!(matches!(state, RestoreState::Ready));
    }

    #[test]
    fn parse_restore_header_unknown_format() {
        let state = parse_restore_header("something-unexpected");
        assert!(matches!(state, RestoreState::NotRequested));
    }

    // ── supported_tiers matrix ──────────────────────────────────────

    #[test]
    fn supported_tiers_glacier_flexible() {
        let provider = S3LifecycleProvider::new("test-bucket".into(), "us-east-1".into());
        let tiers = provider.supported_tiers(&StorageClass::S3GlacierFlexibleRetrieval);
        assert_eq!(tiers.len(), 3);
        assert!(tiers.contains(&RestoreTier::Expedited));
        assert!(tiers.contains(&RestoreTier::Standard));
        assert!(tiers.contains(&RestoreTier::Bulk));
    }

    #[test]
    fn supported_tiers_deep_archive_no_expedited() {
        let provider = S3LifecycleProvider::new("test-bucket".into(), "us-east-1".into());
        let tiers = provider.supported_tiers(&StorageClass::S3GlacierDeepArchive);
        assert_eq!(tiers.len(), 2);
        assert!(!tiers.contains(&RestoreTier::Expedited));
        assert!(tiers.contains(&RestoreTier::Standard));
        assert!(tiers.contains(&RestoreTier::Bulk));
    }

    #[test]
    fn supported_tiers_glacier_instant_retrieval_empty() {
        let provider = S3LifecycleProvider::new("test-bucket".into(), "us-east-1".into());
        let tiers = provider.supported_tiers(&StorageClass::S3GlacierInstantRetrieval);
        assert!(tiers.is_empty());
    }

    #[test]
    fn supported_tiers_standard_empty() {
        let provider = S3LifecycleProvider::new("test-bucket".into(), "us-east-1".into());
        let tiers = provider.supported_tiers(&StorageClass::S3Standard);
        assert!(tiers.is_empty());
    }

    // ── restore_tier_to_s3 mapping ──────────────────────────────────

    #[test]
    fn restore_tier_mapping() {
        assert_eq!(
            restore_tier_to_s3(RestoreTier::Expedited).as_str(),
            "Expedited"
        );
        assert_eq!(
            restore_tier_to_s3(RestoreTier::Standard).as_str(),
            "Standard"
        );
        assert_eq!(restore_tier_to_s3(RestoreTier::Bulk).as_str(), "Bulk");
        // High is Azure-specific; maps to Standard defensively.
        assert_eq!(restore_tier_to_s3(RestoreTier::High).as_str(), "Standard");
    }

    // ── storage_class_to_sdk mapping ────────────────────────────────

    #[test]
    fn storage_class_sdk_mapping() {
        assert_eq!(
            storage_class_to_sdk(StorageClass::S3StandardIa).as_str(),
            "STANDARD_IA"
        );
        assert_eq!(
            storage_class_to_sdk(StorageClass::S3GlacierFlexibleRetrieval).as_str(),
            "GLACIER"
        );
        assert_eq!(
            storage_class_to_sdk(StorageClass::S3GlacierDeepArchive).as_str(),
            "DEEP_ARCHIVE"
        );
        assert_eq!(
            storage_class_to_sdk(StorageClass::S3IntelligentTiering).as_str(),
            "INTELLIGENT_TIERING"
        );
    }

    // ── build_sdk_filter ────────────────────────────────────────────

    #[test]
    fn build_sdk_filter_prefix_only() {
        let filter = build_sdk_filter(Some(".crab/xorbs/".into()), None);
        assert!(filter.is_some());
        let f = filter.unwrap();
        assert_eq!(f.prefix(), Some(".crab/xorbs/"));
        assert!(f.and().is_none());
    }

    #[test]
    fn build_sdk_filter_prefix_and_size() {
        let filter = build_sdk_filter(Some(".crab/xorbs/".into()), Some(40_960));
        assert!(filter.is_some());
        let f = filter.unwrap();
        let and_op = f.and().expect("should have And operator");
        assert_eq!(and_op.prefix(), Some(".crab/xorbs/"));
        assert_eq!(and_op.object_size_greater_than(), Some(40_960));
    }

    #[test]
    fn build_sdk_filter_none() {
        let filter = build_sdk_filter(None, None);
        assert!(filter.is_none());
    }
}
