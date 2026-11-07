use std::path::PathBuf;

use ts_rs::{Config, TS};

use super::{members, organizations, profiles};
use crate::{
    auth::{SessionIdentity, SessionMode, SessionResponse},
    catalog::AccessUpdate,
    cells::{directory, organization},
    config::{BranchProtection, RepositoryAccess, RepositoryGroup, RepositoryMember},
};

fn typescript() -> String {
    // HTTP uses Serde JSON numbers; native versions already enforce the safe
    // integer bound. Export only types present in public HTTP payloads.
    let config = Config::default().with_large_int("number");
    let declarations = [
        SessionIdentity::decl(&config),
        SessionMode::decl(&config),
        SessionResponse::decl(&config),
        BranchProtection::decl(&config),
        RepositoryAccess::decl(&config),
        RepositoryMember::decl(&config),
        RepositoryGroup::decl(&config),
        AccessUpdate::decl(&config),
        members::MembershipState::decl(&config),
        members::roster::Candidate::decl(&config),
        directory::ActorProfile::decl(&config),
        directory::ActorSummary::decl(&config),
        profiles::OwnProfile::decl(&config),
        organization::Role::decl(&config),
        organization::Organization::decl(&config),
        organization::InvitationState::decl(&config),
        organization::Invitation::decl(&config),
        organization::InvitationView::decl(&config),
        organizations::OrganizationView::decl(&config),
        organizations::OrganizationPage::<String>::decl(&config),
        organizations::OrganizationMembership::decl(&config),
        organizations::OrganizationMembers::decl(&config),
        organizations::invitations::InvitationOffer::decl(&config),
        organization::teams::Visibility::decl(&config),
        organization::teams::Role::decl(&config),
        organization::teams::Team::decl(&config),
        organization::teams::View::decl(&config),
        organization::teams::Summary::decl(&config),
        organization::teams::Membership::decl(&config),
        organization::teams::Policy::decl(&config),
        organization::teams::Command::decl(&config),
        organizations::teams::ChangeRequest::decl(&config),
        organizations::teams::Acknowledgement::decl(&config),
        organizations::teams::TeamListing::decl(&config),
        organizations::teams::TeamMember::decl(&config),
        organizations::teams::TeamRoster::decl(&config),
    ];
    let mut source = String::from(
        "// Generated from server HTTP types; do not edit by hand.\n\
         // Run npm run contracts:generate to update.\n",
    );
    for declaration in declarations {
        // Keep generator output stable; formatting must not create a second
        // representation of the authoritative Rust declaration.
        source.push_str("\n// prettier-ignore\nexport ");
        source.push_str(&declaration);
        source.push('\n');
    }
    source
}

fn path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packages/ui/src/lib/contracts.ts")
}

#[test]
fn browser_contracts_match_server() {
    let actual = std::fs::read_to_string(path()).unwrap();
    // Git checkouts may use CRLF; the declaration contract is platform neutral.
    assert_eq!(
        actual.replace("\r\n", "\n"),
        typescript(),
        "run npm run contracts:generate in packages/ui"
    );
}

#[test]
#[ignore = "explicitly regenerates the checked-in browser contract"]
fn export_browser_contracts() {
    std::fs::write(path(), typescript()).unwrap();
}
