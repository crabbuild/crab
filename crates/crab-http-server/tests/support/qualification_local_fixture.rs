use std::sync::Arc;

use crab_storage::Store;
use object_store::{memory::InMemory, path::Path};

use crate::qualification_fixture::{PublicHostFixture, public_host_fixture_with_store};

pub async fn public_host_fixture() -> PublicHostFixture {
    public_host_fixture_with_store(
        Store::new(Arc::new(InMemory::new())),
        Path::from("public-host-qualification"),
    )
    .await
}
