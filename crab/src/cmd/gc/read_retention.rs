//! Reader claim intake shared by repository and global payload collectors.

use std::collections::BTreeMap;

use crab_coordination::read_retention::{
    RetainedReadRoot, load_read_retention_root, retained_read_root,
};
use crab_storage::{Store, StoreLayout};
use object_store::ObjectMeta;

use crate::core::error::{CrabError, Result};

pub(super) async fn claims(
    layout: &StoreLayout<Store>,
    objects: &[ObjectMeta],
    retirement_time: Option<i64>,
) -> Result<BTreeMap<String, RetainedReadRoot>> {
    let prefix = format!("{}/", layout.read_retention_prefix());
    let mut roots = BTreeMap::new();
    for object in objects {
        let Some(owner) = object
            .location
            .as_ref()
            .strip_prefix(&prefix)
            .and_then(|path| path.strip_suffix("/claim"))
        else {
            continue;
        };
        if !blake3::Hash::from_hex(owner).is_ok_and(|hash| hash.to_hex().as_str() == owner) {
            return Err(CrabError::CorruptObject {
                path: object.location.to_string(),
                reason: "read claim owner is not canonical".to_owned(),
            });
        }
        let root = match retirement_time {
            Some(now) => retained_read_root(layout.store().inner(), &object.location, now).await,
            None => load_read_retention_root(layout.store().inner(), &object.location).await,
        }
        .map_err(|error| match error {
            error @ crab_coordination::CoordinationError::Serialize { .. } => {
                CrabError::Io(std::io::Error::other(error))
            }
            error => CrabError::from(error),
        })?;
        if let Some(root) = root
            && roots.insert(owner.to_owned(), root).is_some()
        {
            return Err(CrabError::CorruptObject {
                path: object.location.to_string(),
                reason: "read claim was listed twice".to_owned(),
            });
        }
    }
    Ok(roots)
}
