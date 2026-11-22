"""Wire contracts for projected-view retention."""

from typing import Annotated, Literal

from pydantic import BaseModel, ConfigDict, Field, TypeAdapter

HexDigest = Annotated[str, Field(pattern=r"^[0-9a-f]{64}$")]
RemainingMs = Annotated[int, Field(strict=True, gt=0, le=150_000)]
Incarnation = Annotated[str, Field(
    pattern=r"^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$"
)]
ViewPrefix = Annotated[str, Field(min_length=1, max_length=1024)]


class RetentionRecord(BaseModel):
    model_config = ConfigDict(extra="forbid")


class RetentionRevision(RetentionRecord):
    incarnation: Incarnation
    sequence: Annotated[int, Field(strict=True, ge=0, lt=2**64)]


class ViewRetentionBinding(RetentionRecord):
    view_prefix: ViewPrefix
    reader_nonce: HexDigest = Field(repr=False)
    target: HexDigest


class RetainedViewCredentials(ViewRetentionBinding):
    incarnation: Incarnation = Field(repr=False)


class AcquireRetention(RetentionRecord):
    action: Literal["acquire"]


class ChangeRetention(RetentionRecord):
    action: Literal["renew", "release"]
    revision: RetentionRevision


RetentionAction = Annotated[AcquireRetention | ChangeRetention, Field(discriminator="action")]


class RetentionAcquired(RetentionRecord):
    action: Literal["acquired"]
    repo_prefix: ViewPrefix
    view_state_digest: HexDigest
    revision: RetentionRevision
    remaining_ms: RemainingMs


class RetentionRenewed(RetentionRecord):
    action: Literal["renewed"]
    revision: RetentionRevision
    remaining_ms: RemainingMs


class RetentionReleased(RetentionRecord):
    action: Literal["released"]


class RetentionVerified(RetentionRecord):
    action: Literal["verified"]
    repo_prefix: ViewPrefix
    remaining_ms: RemainingMs


RetentionReply = Annotated[
    RetentionAcquired | RetentionRenewed | RetentionReleased,
    Field(discriminator="action"),
]
NativeRetentionReply = Annotated[
    RetentionAcquired | RetentionRenewed | RetentionReleased | RetentionVerified,
    Field(discriminator="action"),
]
NATIVE_RETENTION_REPLY = TypeAdapter(NativeRetentionReply)


class RetentionConflict(RuntimeError):
    """The native helper could not acknowledge the requested revision."""
