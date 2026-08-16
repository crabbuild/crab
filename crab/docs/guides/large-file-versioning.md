# Large-File Versioning Guide

Crab diff compares committed Crab pointer files by chunk sequence. It reports
changed files, changed byte ranges, and reuse metrics without hydrating the full
payload.

## Chunk Diff

```python
diff = repo.diff("model-v1", "model-v2", path="models/model.safetensors")
for file in diff.files:
    metrics = file.chunk_metrics
    if metrics is not None:
        print(file.path, metrics.reuse_ratio, metrics.changed_byte_ranges_new)
```

## Format Hints

Safetensors and Parquet examples should treat annotations as hints. Use
`Reader.read_range`, file headers, Parquet footers, or sidecar metadata before
claiming that a tensor, row group, document chunk, or embedding unit changed.

## RAG and Model Evaluation

For incremental embeddings, enqueue changed files and byte ranges first, then
map those ranges to document spans only when sidecar metadata proves the
mapping. For evaluation, record dataset rev, model rev, prompt rev, metrics
path, and diff summary in an evidence bundle.
