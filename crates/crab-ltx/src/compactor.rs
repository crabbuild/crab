// Derived from denoland/celld, commit 10cb1303dac710dcb3b557e318e08c855261f68b.
// Apache-2.0; see LICENSE and UPSTREAM.md. Modified by Crab contributors.

use crate::codec::{Decoder, Encoder};
use crate::error::{CrabError, Result};
use crate::ltx::{Header, PageHeader, VERSION, checksum_page};
use crate::{CHECKSUM_FLAG, Txid};
use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    io::{Read, Write},
};

pub struct Compactor<W, R> {
    encoder: Encoder<W>,
    inputs: Vec<CompactorInput<R>>,
    post_apply_checksum: u64,
    image_digest: blake3::Hasher,
    next_image_page: u32,
    zero_page: Vec<u8>,
}

pub(crate) struct CompactionProof {
    pub(crate) header: Header,
    pub(crate) post_apply_checksum: u64,
    pub(crate) image_digest: [u8; 32],
}

impl<W: Write, R: Read> Compactor<W, R> {
    pub fn new(writer: W, readers: Vec<R>) -> Self {
        Self {
            encoder: Encoder::new_block(writer),
            inputs: readers
                .into_iter()
                .map(|reader| CompactorInput {
                    decoder: Decoder::new(reader),
                    page: None,
                    data: Vec::new(),
                })
                .collect(),
            post_apply_checksum: CHECKSUM_FLAG,
            image_digest: blake3::Hasher::new(),
            next_image_page: 1,
            zero_page: Vec::new(),
        }
    }

    pub fn into_writer(self) -> W {
        self.encoder.writer
    }

    pub fn compact(&mut self) -> Result<CompactionProof> {
        if self.inputs.is_empty() {
            return Err(CrabError::LTXCorrupted);
        }

        for input in &mut self.inputs {
            input.decoder.decode_header()?;
        }

        for index in 1..self.inputs.len() {
            let previous = self.inputs[index - 1].decoder.header;
            let current = self.inputs[index].decoder.header;
            if previous.page_size != current.page_size {
                return Err(CrabError::LTXCorrupted);
            }
            if !is_contiguous(previous.max_txid, current.min_txid, current.max_txid) {
                return Err(CrabError::LTXCorrupted);
            }
        }

        let first = self.inputs[0].decoder.header;
        let last = self.inputs[self.inputs.len() - 1].decoder.header;
        self.zero_page = vec![0; first.page_size as usize];
        self.encoder.encode_header(Header {
            version: VERSION,
            flags: 0,
            page_size: first.page_size,
            commit: last.commit,
            min_txid: first.min_txid,
            max_txid: last.max_txid,
            timestamp: last.timestamp,
            pre_apply_checksum: first.pre_apply_checksum,
            ..Header::default()
        })?;

        for input in &mut self.inputs {
            input.data.resize(first.page_size as usize, 0);
        }

        self.merge_pages()?;

        for input in &mut self.inputs {
            input.decoder.close()?;
        }

        while self.next_image_page <= self.encoder.header.commit {
            self.image_digest.update(&self.zero_page);
            self.next_image_page = self
                .next_image_page
                .checked_add(1)
                .ok_or(CrabError::LTXCorrupted)?;
        }

        // Recompute the snapshot checksum from the pages selected by the merge;
        // do not trust a source trailer when validating compaction output.
        let proof = CompactionProof {
            header: self.encoder.header,
            post_apply_checksum: self.post_apply_checksum,
            image_digest: *self.image_digest.finalize().as_bytes(),
        };
        self.encoder.close(self.post_apply_checksum)?;
        Ok(proof)
    }

    fn merge_pages(&mut self) -> Result<()> {
        let mut heads = BinaryHeap::with_capacity(self.inputs.len());
        let mut consumed = Vec::with_capacity(self.inputs.len());
        for index in 0..self.inputs.len() {
            if let Some(page) = self.decode_next(index)? {
                heads.push(Reverse((page.pgno, index)));
            }
        }

        while let Some(Reverse((page_number, first_index))) = heads.pop() {
            let mut selected = None;
            consumed.clear();
            self.consume_head(page_number, first_index, &mut selected, &mut consumed)?;
            while let Some(Reverse((next_page, index))) = heads.peek().copied() {
                if next_page != page_number {
                    break;
                }
                heads.pop();
                self.consume_head(page_number, index, &mut selected, &mut consumed)?;
            }

            if let Some((index, page)) = selected {
                let commit = self.encoder.header.commit;
                if page_number <= commit {
                    let data = &self.inputs[index].data;
                    while self.next_image_page < page_number {
                        self.image_digest.update(&self.zero_page);
                        self.next_image_page = self
                            .next_image_page
                            .checked_add(1)
                            .ok_or(CrabError::LTXCorrupted)?;
                    }
                    self.image_digest.update(data);
                    self.next_image_page =
                        page_number.checked_add(1).ok_or(CrabError::LTXCorrupted)?;
                    self.encoder.encode_page(page, data)?;
                    self.post_apply_checksum = CHECKSUM_FLAG
                        | (self.post_apply_checksum ^ checksum_page(page_number, data));
                }
            }
            for index in consumed.drain(..) {
                if let Some(next) = self.decode_next(index)? {
                    heads.push(Reverse((next.pgno, index)));
                }
            }
        }
        Ok(())
    }

    fn consume_head(
        &mut self,
        page_number: u32,
        index: usize,
        selected: &mut Option<(usize, PageHeader)>,
        consumed: &mut Vec<usize>,
    ) -> Result<()> {
        let page = self.inputs[index]
            .page
            .take()
            .ok_or(CrabError::LTXCorrupted)?;
        if page.pgno != page_number {
            return Err(CrabError::LTXCorrupted);
        }
        // Later inputs are newer. The heap visits equal page numbers in input
        // order, so replacing the candidate retains the newest page.
        *selected = Some((index, page));
        consumed.push(index);
        Ok(())
    }

    fn decode_next(&mut self, index: usize) -> Result<Option<PageHeader>> {
        let input = self.inputs.get_mut(index).ok_or(CrabError::LTXCorrupted)?;
        if input.page.is_some() {
            return Err(CrabError::LTXCorrupted);
        }
        let page = input.decoder.decode_page(&mut input.data)?;
        input.page = page;
        Ok(page)
    }
}

fn is_contiguous(previous_max: Txid, min: Txid, max: Txid) -> bool {
    previous_max.0.checked_add(1) == Some(min.0) && max >= min
}

struct CompactorInput<R> {
    decoder: Decoder<R>,
    page: Option<PageHeader>,
    data: Vec<u8>,
}
