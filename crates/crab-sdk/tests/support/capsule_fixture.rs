use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use crab_metadata::{capsule_protocol::*, git_visibility::GitVisibilityEdit};
use crab_storage::{Store, StoreLayout};

/// Independent native writer: Git supplies closure and kind evidence; metadata
/// encodes complete capsules. SDK reads cannot borrow the source checkout.
pub struct CapsuleFixture {
    layout: StoreLayout<Store>,
    record: RootRecord,
    runs: Vec<CapsuleRun>,
    pack: CapsuleGitPack,
    closures: BTreeMap<String, BTreeSet<String>>,
}

impl CapsuleFixture {
    pub async fn new(
        layout: StoreLayout<Store>,
        source: &Path,
        pack_path: &Path,
        targets: &[String],
    ) -> Self {
        let index = pack_path.with_extension("idx");
        let reverse = pack_path.with_extension("rev");
        crab_git::write_pack_reverse_index(&index, &reverse).unwrap();
        let body = std::fs::read(pack_path).unwrap();
        let locations =
            crab_git::PackLocationIter::open(&index, &reverse, body.len() as u64).unwrap();
        let checksum = locations.pack_checksum();
        let oids = locations
            .map(|entry| entry.unwrap().oid)
            .collect::<Vec<_>>();
        let kinds = crab_git::object_kinds_from_git_dir(&source.join(".git"), &oids).unwrap();
        let kinds = oids.iter().map(|oid| kinds[oid]).collect::<Vec<_>>();
        let locator = crab_git::pack_locator::encode_pack_kind_metadata(checksum, &kinds).unwrap();
        let pack = CapsuleGitPack::new(
            body.into(),
            std::fs::read(index).unwrap().into(),
            std::fs::read(reverse).unwrap().into(),
            locator.into(),
            checksum.to_string(),
            oids.len() as u64,
        )
        .unwrap();
        let closures = targets
            .iter()
            .map(|tip| {
                let objects =
                    super::git(source, &["rev-list", "--objects", "--no-object-names", tip]);
                (tip.clone(), objects.lines().map(str::to_owned).collect())
            })
            .collect();
        let record = RootRecord::encode(
            RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap(),
        )
        .unwrap();
        create_root(&layout, record.clone()).await.unwrap();
        Self {
            layout,
            record,
            runs: Vec::new(),
            pack,
            closures,
        }
    }

    pub async fn publish(&mut self, refs: BTreeMap<String, String>, catalog: PointerCatalog) {
        let names = self
            .record
            .root()
            .refs()
            .keys()
            .chain(refs.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let edits = names
            .into_iter()
            .filter_map(|name| {
                let old = self.record.root().refs().get(&name);
                let new = refs.get(&name);
                (old != new).then(|| CapsuleRefEdit::new(&name, old.cloned(), new.cloned(), None))
            })
            .collect::<Vec<_>>();
        let transaction = CapsuleTransaction::new(self.record.digest(), edits).unwrap();
        let visibility = transaction
            .edits()
            .iter()
            .filter_map(|edit| {
                edit.new_oid().map(|tip| {
                    (
                        edit.ref_name().to_owned(),
                        GitVisibilityEdit::replacement(
                            edit.expected_old().map(str::to_owned),
                            tip.to_owned(),
                            &self.closures[tip],
                        ),
                    )
                })
            })
            .collect();
        let mut sections = vec![CapsuleSection::new(
            CapsuleSectionKind::VisibilityDelta,
            CapsuleVisibilityDelta::new(visibility)
                .unwrap()
                .encode()
                .unwrap(),
        )];
        if !catalog.is_empty() {
            sections.push(CapsuleSection::new(
                CapsuleSectionKind::CatalogDelta,
                catalog.encode_delta().unwrap(),
            ));
        }
        let packs = if self.runs.is_empty() {
            vec![self.pack.clone()]
        } else {
            Vec::new()
        };
        let capsule = Capsule::build(&transaction, packs, sections).unwrap();
        self.runs.push(CapsuleRun::leaf(capsule).unwrap());
        let run = if self.runs.len() == 1 {
            self.runs[0].clone()
        } else {
            CapsuleRun::compact(self.runs.clone()).unwrap()
        };
        self.layout
            .store()
            .put(&self.layout.capsule_path(run.hash()), run.bytes().clone())
            .await
            .unwrap();
        let pointer = CapsulePointer::new(
            run.hash(),
            run.bytes().len() as u64,
            run.control_offset(),
            run.control_size(),
            run.footer_hash(),
            run.level(),
            run.transaction_ids(),
            run.newest_base_root_digest(),
        )
        .unwrap();
        let next = self
            .record
            .root()
            .advance(
                self.record.digest(),
                refs,
                BTreeMap::new(),
                vec![pointer],
                &transaction.id().unwrap(),
            )
            .unwrap();
        self.record = RootRecord::encode(next).unwrap();
        self.layout
            .store()
            .put_overwrite(
                &self.layout.capsule_root_path(),
                self.record.bytes().clone(),
            )
            .await
            .unwrap();
    }

    pub async fn replace_without_catalog(&mut self) {
        let refs = self.record.root().refs().clone();
        // Model a new captured authority without catalog bindings while leaving
        // old immutable inputs available to already-open readers.
        self.record = RootRecord::encode(
            RepositoryRoot::initial(&"2".repeat(64), "refs/heads/main").unwrap(),
        )
        .unwrap();
        self.runs.clear();
        self.publish(refs, PointerCatalog::new()).await;
    }
}

#[cfg(feature = "content")]
pub async fn catalog_for_shard(
    layout: &StoreLayout<Store>,
    hash: &str,
    body: &[u8],
) -> PointerCatalog {
    let mut catalog = PointerCatalog::new();
    let mut xorbs = BTreeSet::new();
    crab_xet::shard_parse::visit_file_entries_from_reader(
        &mut std::io::Cursor::new(body),
        |file, terms| {
            let size = terms
                .map(|term| u64::from(term.unpacked_segment_bytes))
                .sum();
            catalog
                .insert_file(file.hex(), FileCatalogEntry::new(size, hash))
                .unwrap();
            Ok(())
        },
    )
    .unwrap();
    crab_xet::shard_parse::visit_xorb_chunks_from_reader(
        &mut std::io::Cursor::new(body),
        |xorb, _, _, _| {
            xorbs.insert(xorb.hex());
            Ok(())
        },
    )
    .unwrap();
    catalog
        .insert_shard(
            hash.to_owned(),
            ShardCatalogEntry::new(body.len() as u64, xorbs.iter().cloned().collect()),
        )
        .unwrap();
    for hash in xorbs {
        let (bytes, _) = layout
            .store()
            .get_with_etag(&layout.xorb_path(&crab_xet::hash::MerkleHash::from_hex(&hash).unwrap()))
            .await
            .unwrap();
        let parser = crab_xet::xorb::parser::XorbParser::parse(bytes.clone()).unwrap();
        parser.verify_all_chunks().unwrap();
        let chunks = (0..parser.num_chunks())
            .map(|index| {
                let meta = parser.chunk_meta(index).unwrap();
                XorbChunkEntry::new(meta.hash.hex(), meta.uncompressed_len)
            })
            .collect();
        catalog
            .insert_xorb(
                hash,
                XorbCatalogEntry::new(
                    bytes.len() as u64,
                    blake3::hash(&bytes).to_hex().to_string(),
                    chunks,
                ),
            )
            .unwrap();
    }
    catalog
}
