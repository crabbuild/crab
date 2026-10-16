//! Union authenticated checkpoint columns without retaining their recipes.
use super::*;
use crate::pages::object::Node;
use std::collections::BTreeMap;

type RecipeDigests = BTreeMap<(u8, [u8; 32]), [u8; 32]>;

impl CatalogPages {
    // Both complete inputs must have passed `verify` under this same budget.
    // Newer bindings win, but cannot hide immutable conflicts or size changes.
    pub(in crate::capsule_protocol) async fn merge(
        &self,
        store: &Store,
        newer: &Self,
        work: &mut PageWork,
    ) -> Result<Descriptor> {
        let mut result = newer.descriptor.clone();
        let mut recipes = RecipeDigests::new();
        for (kind, target) in [
            (Kind::CatalogFiles, &mut result.files),
            (Kind::CatalogShards, &mut result.shards),
            (Kind::CatalogXorbs, &mut result.xorbs),
            (Kind::CatalogChunks, &mut result.chunks),
        ] {
            if self.root(kind)? == target.as_ref() {
                continue;
            }
            if target.is_none() {
                *target = self.root(kind)?.cloned();
                continue;
            }
            let mut pending = self.root(kind)?.cloned().into_iter().collect();
            while let Some((_, node)) = self
                .pages
                .next_page(store, &mut pending, kind, work)
                .await?
            {
                let Node::Leaf(rows) = node.as_ref() else {
                    continue;
                };
                let keys = rows.iter().map(|(key, _)| key.clone()).collect::<Vec<_>>();
                let existing = newer
                    .pages
                    .lookup(store, target.as_ref(), kind, &keys, work)
                    .await?;
                let mut changed = Vec::new();
                for ((key, incoming), existing) in rows.iter().zip(existing) {
                    let Some(prior) = existing else {
                        changed.push((key.clone(), incoming.clone()));
                        continue;
                    };
                    if prior == *incoming {
                        continue;
                    }
                    match kind {
                        Kind::CatalogFiles => {
                            if file_from_bytes(&prior)?.size != file_from_bytes(incoming)?.size {
                                return Err(contract_error(format!(
                                    "file {} has conflicting declared sizes",
                                    hex(key)?
                                )));
                            }
                        }
                        Kind::CatalogShards => {
                            let prior: ShardRow = from_json(&prior)?;
                            let incoming: ShardRow = from_json(incoming)?;
                            if prior.encoded_size != incoming.encoded_size
                                || !newer
                                    .same_recipe(
                                        store,
                                        (prior.xorbs, incoming.xorbs),
                                        Kind::CatalogShardXorbs,
                                        &mut recipes,
                                        work,
                                    )
                                    .await?
                            {
                                return Err(contract_error(format!(
                                    "shard {} has conflicting descriptors",
                                    hex(key)?
                                )));
                            }
                        }
                        Kind::CatalogXorbs => {
                            let prior: XorbRow = from_json(&prior)?;
                            let incoming: XorbRow = from_json(incoming)?;
                            if prior.encoded_size != incoming.encoded_size
                                || prior.body_digest != incoming.body_digest
                                || !newer
                                    .same_recipe(
                                        store,
                                        (Some(prior.chunks), Some(incoming.chunks)),
                                        Kind::CatalogXorbChunks,
                                        &mut recipes,
                                        work,
                                    )
                                    .await?
                            {
                                return Err(contract_error(format!(
                                    "xorb {} has conflicting descriptors",
                                    hex(key)?
                                )));
                            }
                        }
                        Kind::CatalogChunks => {
                            // Each input proved lowest-xorb/first-occurrence ownership.
                            // Their union must retain the lower of those two placements.
                            if prior[..36] > incoming[..36] {
                                changed.push((key.clone(), incoming.clone()));
                            }
                        }
                        _ => return Err(corrupt("catalog merge selected an invalid column")),
                    }
                }
                *target = newer
                    .pages
                    .edit(store, target.as_ref(), kind, changed, work)
                    .await?;
            }
        }
        result.validate()?;
        Ok(result)
    }
    async fn same_recipe(
        &self,
        store: &Store,
        roots: (Option<Reference>, Option<Reference>),
        kind: Kind,
        recipes: &mut RecipeDigests,
        work: &mut PageWork,
    ) -> Result<bool> {
        if roots.0 == roots.1 {
            return Ok(true);
        }
        let (Some(left), Some(right)) = roots else {
            return Ok(false);
        };
        let left = self
            .recipe_digest(store, &left, kind, recipes, work)
            .await?;
        let right = self
            .recipe_digest(store, &right, kind, recipes, work)
            .await?;
        Ok(left == right)
    }

    async fn recipe_digest(
        &self,
        store: &Store,
        root: &Reference,
        kind: Kind,
        recipes: &mut RecipeDigests,
        work: &mut PageWork,
    ) -> Result<[u8; 32]> {
        let identity = (kind as u8, root.hash);
        if let Some(digest) = recipes.get(&identity) {
            return Ok(*digest);
        }
        let value_bytes = match kind {
            Kind::CatalogShardXorbs => 0,
            Kind::CatalogXorbChunks => 36,
            _ => return Err(corrupt("catalog comparison selected an invalid recipe")),
        };
        // Cache only a complete authenticated scan. Charge logical work once per
        // root, so shared or repaged recipes cannot authorize quadratic replay.
        work.admit_materialized(
            u64::from(root.count) * (kind.key_bytes() + value_bytes + 8) as u64,
        )?;
        let mut hasher = blake3::Hasher::new_derive_key("crab catalog recipe comparison");
        hasher.update(&[kind as u8]);
        hasher.update(&root.count.to_le_bytes());
        let mut pending = vec![root.clone()];
        while let Some((_, node)) = self
            .pages
            .next_page(store, &mut pending, kind, work)
            .await?
        {
            if let Node::Leaf(rows) = node.as_ref() {
                for (key, value) in rows {
                    hasher.update(key);
                    hasher.update(&(value.len() as u64).to_le_bytes());
                    hasher.update(value);
                }
            }
        }
        let digest = *hasher.finalize().as_bytes();
        recipes.insert(identity, digest);
        Ok(digest)
    }
}

#[cfg(test)]
mod tests;
