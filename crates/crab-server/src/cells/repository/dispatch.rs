use super::collaboration::links::{GetLink, ListLinks, SetLink};
use super::collaboration::milestones::{
    AssignMilestone, CreateMilestone, EditMilestone, GetAssignment, GetMilestone,
    ListMilestoneItems, ListMilestones,
};
use super::collaboration::reactions::{GetReactions, SetReaction};
use super::*;

pub(in crate::cells) fn dispatch_mutation(
    context: &mut RepositoryTransaction<'_>,
    id: u32,
    input: serde_json::Value,
) -> crate::cells::Result<MutationResult<serde_json::Value>> {
    match id {
        <SetPullReadiness as Mutation>::ID => invoke_mutation::<SetPullReadiness>(context, input),
        <SetLink as Mutation>::ID => invoke_mutation::<SetLink>(context, input),
        <SetReaction as Mutation>::ID => invoke_mutation::<SetReaction>(context, input),
        <CreateMilestone as Mutation>::ID => invoke_mutation::<CreateMilestone>(context, input),
        <EditMilestone as Mutation>::ID => invoke_mutation::<EditMilestone>(context, input),
        <AssignMilestone as Mutation>::ID => invoke_mutation::<AssignMilestone>(context, input),
        <CreateIssue as Mutation>::ID => invoke_mutation::<CreateIssue>(context, input),
        <CreateComment as Mutation>::ID => invoke_mutation::<CreateComment>(context, input),
        <UpdateIssue as Mutation>::ID => invoke_mutation::<UpdateIssue>(context, input),
        <UpdateComment as Mutation>::ID => invoke_mutation::<UpdateComment>(context, input),
        <CreateLabel as Mutation>::ID => invoke_mutation::<CreateLabel>(context, input),
        <UpdateLabel as Mutation>::ID => invoke_mutation::<UpdateLabel>(context, input),
        <DeleteLabel as Mutation>::ID => invoke_mutation::<DeleteLabel>(context, input),
        <CreateCommitStatus as Mutation>::ID => {
            invoke_mutation::<CreateCommitStatus>(context, input)
        }
        <CreateCheckRun as Mutation>::ID => invoke_mutation::<CreateCheckRun>(context, input),
        <UpdateCheckRun as Mutation>::ID => invoke_mutation::<UpdateCheckRun>(context, input),
        <ReplaceBranchProtections as Mutation>::ID => {
            invoke_mutation::<ReplaceBranchProtections>(context, input)
        }
        <ReplaceRepositoryLifecycle as Mutation>::ID => {
            invoke_mutation::<ReplaceRepositoryLifecycle>(context, input)
        }
        <CreatePull as Mutation>::ID => invoke_mutation::<CreatePull>(context, input),
        <UpdatePull as Mutation>::ID => invoke_mutation::<UpdatePull>(context, input),
        <CreatePullComment as Mutation>::ID => invoke_mutation::<CreatePullComment>(context, input),
        <UpdatePullComment as Mutation>::ID => invoke_mutation::<UpdatePullComment>(context, input),
        <CreatePullReview as Mutation>::ID => invoke_mutation::<CreatePullReview>(context, input),
        <UpdatePullReview as Mutation>::ID => invoke_mutation::<UpdatePullReview>(context, input),
        <CreatePullReviewThread as Mutation>::ID => {
            invoke_mutation::<CreatePullReviewThread>(context, input)
        }
        <UpdatePullReviewThread as Mutation>::ID => {
            invoke_mutation::<UpdatePullReviewThread>(context, input)
        }
        <CreatePullReviewReply as Mutation>::ID => {
            invoke_mutation::<CreatePullReviewReply>(context, input)
        }
        <UpdatePullReviewReply as Mutation>::ID => {
            invoke_mutation::<UpdatePullReviewReply>(context, input)
        }
        <ReservePullMerge as Mutation>::ID => invoke_mutation::<ReservePullMerge>(context, input),
        <TransitionPullMerge as Mutation>::ID => {
            invoke_mutation::<TransitionPullMerge>(context, input)
        }
        <CreateRelease as Mutation>::ID => invoke_mutation::<CreateRelease>(context, input),
        <UpdateRelease as Mutation>::ID => invoke_mutation::<UpdateRelease>(context, input),
        <CompleteReleasePublication as Mutation>::ID => {
            invoke_mutation::<CompleteReleasePublication>(context, input)
        }
        <DeleteRelease as Mutation>::ID => invoke_mutation::<DeleteRelease>(context, input),
        <ReserveReleaseAsset as Mutation>::ID => {
            invoke_mutation::<ReserveReleaseAsset>(context, input)
        }
        <AttachReleaseAsset as Mutation>::ID => {
            invoke_mutation::<AttachReleaseAsset>(context, input)
        }
        <DeleteReleaseAsset as Mutation>::ID => {
            invoke_mutation::<DeleteReleaseAsset>(context, input)
        }
        <projection::ApplyProjectionBatch as Mutation>::ID => {
            invoke_mutation::<projection::ApplyProjectionBatch>(context, input)
        }
        _ => Err(crate::cells::Error::Invalid("unknown repository operation")),
    }
}

pub(in crate::cells) fn dispatch_read(
    context: &mut RepositoryTransaction<'_>,
    id: u32,
    input: serde_json::Value,
) -> crate::cells::Result<serde_json::Value> {
    match id {
        <GetLink as Read>::ID => invoke_read::<GetLink>(context, input),
        <ListLinks as Read>::ID => invoke_read::<ListLinks>(context, input),
        <collaboration::activity::ListActivity as Read>::ID => {
            invoke_read::<collaboration::activity::ListActivity>(context, input)
        }
        <GetReactions as Read>::ID => invoke_read::<GetReactions>(context, input),
        <ListMilestoneItems as Read>::ID => invoke_read::<ListMilestoneItems>(context, input),
        <GetMilestone as Read>::ID => invoke_read::<GetMilestone>(context, input),
        <ListMilestones as Read>::ID => invoke_read::<ListMilestones>(context, input),
        <GetAssignment as Read>::ID => invoke_read::<GetAssignment>(context, input),
        <GetIssue as Read>::ID => invoke_read::<GetIssue>(context, input),
        <GetIssueDetail as Read>::ID => invoke_read::<GetIssueDetail>(context, input),
        <GetComment as Read>::ID => invoke_read::<GetComment>(context, input),
        <ListIssues as Read>::ID => invoke_read::<ListIssues>(context, input),
        <ListComments as Read>::ID => invoke_read::<ListComments>(context, input),
        <ListLabels as Read>::ID => invoke_read::<ListLabels>(context, input),
        <ListCommitStatuses as Read>::ID => invoke_read::<ListCommitStatuses>(context, input),
        <GetCommitStatusSubmission as Read>::ID => {
            invoke_read::<GetCommitStatusSubmission>(context, input)
        }
        <ListCheckRuns as Read>::ID => invoke_read::<ListCheckRuns>(context, input),
        <GetCheckRun as Read>::ID => invoke_read::<GetCheckRun>(context, input),
        <GetCheckCreateSubmission as Read>::ID => {
            invoke_read::<GetCheckCreateSubmission>(context, input)
        }
        <GetCheckUpdateSubmission as Read>::ID => {
            invoke_read::<GetCheckUpdateSubmission>(context, input)
        }
        <GetBranchProtections as Read>::ID => invoke_read::<GetBranchProtections>(context, input),
        <GetRepositoryLifecycle as Read>::ID => {
            invoke_read::<GetRepositoryLifecycle>(context, input)
        }
        <GetPull as Read>::ID => invoke_read::<GetPull>(context, input),
        <GetPullSubmission as Read>::ID => invoke_read::<GetPullSubmission>(context, input),
        <ListPulls as Read>::ID => invoke_read::<ListPulls>(context, input),
        <GetPullComment as Read>::ID => invoke_read::<GetPullComment>(context, input),
        <ListPullComments as Read>::ID => invoke_read::<ListPullComments>(context, input),
        <GetPullReview as Read>::ID => invoke_read::<GetPullReview>(context, input),
        <ListPullReviews as Read>::ID => invoke_read::<ListPullReviews>(context, input),
        <GetPullReviewSubmission as Read>::ID => {
            invoke_read::<GetPullReviewSubmission>(context, input)
        }
        <GetPullReviewThread as Read>::ID => invoke_read::<GetPullReviewThread>(context, input),
        <ListPullReviewThreads as Read>::ID => invoke_read::<ListPullReviewThreads>(context, input),
        <GetPullReviewThreadSubmission as Read>::ID => {
            invoke_read::<GetPullReviewThreadSubmission>(context, input)
        }
        <GetPullReviewReply as Read>::ID => invoke_read::<GetPullReviewReply>(context, input),
        <ListPullReviewReplies as Read>::ID => invoke_read::<ListPullReviewReplies>(context, input),
        <GetPullReviewReplySubmission as Read>::ID => {
            invoke_read::<GetPullReviewReplySubmission>(context, input)
        }
        <GetPullMergeSubmission as Read>::ID => {
            invoke_read::<GetPullMergeSubmission>(context, input)
        }
        <GetRelease as Read>::ID => invoke_read::<GetRelease>(context, input),
        <GetReleaseByTag as Read>::ID => invoke_read::<GetReleaseByTag>(context, input),
        <GetReleaseSubmission as Read>::ID => invoke_read::<GetReleaseSubmission>(context, input),
        <ListReleases as Read>::ID => invoke_read::<ListReleases>(context, input),
        <projection::GetProjectionState as Read>::ID => {
            invoke_read::<projection::GetProjectionState>(context, input)
        }
        <projection::GetProjectionAttribution as Read>::ID => {
            invoke_read::<projection::GetProjectionAttribution>(context, input)
        }
        _ => Err(crate::cells::Error::Invalid("unknown repository operation")),
    }
}

fn invoke_mutation<T: Mutation>(
    context: &mut RepositoryTransaction<'_>,
    input: serde_json::Value,
) -> crate::cells::Result<MutationResult<serde_json::Value>> {
    let input = serde_json::from_value(input)?;
    Ok(match T::execute(context, input)? {
        MutationResult::Success(value) => MutationResult::Success(serde_json::to_value(value)?),
        MutationResult::Rejected(value) => MutationResult::Rejected(serde_json::to_value(value)?),
    })
}

fn invoke_read<T: Read>(
    context: &mut RepositoryTransaction<'_>,
    input: serde_json::Value,
) -> crate::cells::Result<serde_json::Value> {
    Ok(serde_json::to_value(T::execute(
        context,
        serde_json::from_value(input)?,
    )?)?)
}
