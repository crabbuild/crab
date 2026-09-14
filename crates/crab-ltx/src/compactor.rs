// Derived from denoland/celld, commit 10cb1303dac710dcb3b557e318e08c855261f68b.
// Apache-2.0; see LICENSE and UPSTREAM.md. Modified by Crab contributors.

use crate::Txid;
use crate::codec::{Decoder, Encoder};
use crate::error::{CrabError, Result};
use crate::ltx::{Header, PageHeader, VERSION};
use std::io::{Read, Write};

pub struct Compactor<W, R> {
    encoder: Encoder<W>,
    inputs: Vec<CompactorInput<R>>,
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
        }
    }

    pub fn into_writer(self) -> W {
        self.encoder.writer
    }

    pub fn compact(&mut self) -> Result<()> {
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

        loop {
            let Some(page_number) = self.fill_page_buffers()? else {
                break;
            };
            self.write_page_buffer(page_number)?;
        }

        for input in &mut self.inputs {
            input.decoder.close()?;
        }

        let post_apply_checksum = self.inputs[self.inputs.len() - 1]
            .decoder
            .trailer
            .post_apply_checksum;
        self.encoder.close(post_apply_checksum)
    }

    fn fill_page_buffers(&mut self) -> Result<Option<u32>> {
        let mut minimum = None;
        for input in &mut self.inputs {
            if input.page.is_none() {
                input.page = input.decoder.decode_page(&mut input.data)?;
            }
            if let Some(page) = input.page {
                minimum = Some(minimum.map_or(page.pgno, |value: u32| value.min(page.pgno)));
            }
        }
        Ok(minimum)
    }

    fn write_page_buffer(&mut self, page_number: u32) -> Result<()> {
        let commit = self.encoder.header.commit;
        let mut written = false;
        for input in self.inputs.iter_mut().rev() {
            let Some(page) = input.page else {
                continue;
            };
            if page.pgno != page_number {
                continue;
            }
            input.page = None;
            if written || page_number > commit {
                continue;
            }
            written = true;
            self.encoder.encode_page(page, &input.data)?;
        }
        Ok(())
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
