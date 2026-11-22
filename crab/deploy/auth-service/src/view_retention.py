"""Wire contracts for projected-view retention."""

from typing import Annotated, Literal

from pydantic import BaseModel, ConfigDict, Field, TypeAdapter

HexDigest = Annotated[str, Field(pattern=r"^[0-9a-f]{64}$")]
RemainingMs = Annotated[int, Field(strict=True, gt=0, le=150_000)]


class RetentionRecord(BaseModel):
    model_config = ConfigDict(extra="forbid")


class RetentionRevision(RetentionRecord):
    incarnation: Annotated[str, Field(
        pattern=r"^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$"
    )]
    sequence: Annotated[int, Field(strict=True, ge=0, lt=2**64)]


class AcquireRetention(RetentionRecord):
    action: Literal["acquire"]


class ChangeRetention(RetentionRecord):
    action: Literal["renew", "release"]
    revision: RetentionRevision


RetentionAction = Annotated[AcquireRetention | ChangeRetention, Field(discriminator="action")]


class RetentionAcquired(RetentionRecord):
    action: Literal["acquired"]
    repo_prefix: Annotated[str, Field(min_length=1, max_length=1024)]
    view_state_digest: HexDigest
    revision: RetentionRevision
    remaining_ms: RemainingMs


class RetentionRenewed(RetentionRecord):
    action: Literal["renewed"]
    revision: RetentionRevision
    remaining_ms: RemainingMs


class RetentionReleased(RetentionRecord):
    action: Literal["released"]


RetentionReply = Annotated[
    RetentionAcquired | RetentionRenewed | RetentionReleased,
    Field(discriminator="action"),
]
RETENTION_REPLY = TypeAdapter(RetentionReply)


class RetentionConflict(RuntimeError):
    """The native helper could not acknowledge the requested revision."""
