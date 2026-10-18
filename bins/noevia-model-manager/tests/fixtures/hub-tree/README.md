# Real Hub tree pages (noevia#1999)

Real `expand=true` tree listings, captured to measure the shared page limits in
`src/hub_page.rs` (`PAGE_NODES` 262,144, `PAGE_STRING_CHARS` 65,536) against what the Hub sends.
They are public metadata only: no token or other credential was sent, and no model file was
requested.

| File | What it is | Pages | Entries |
| --- | --- | --- | --- |
| `hub-tree-gguf-large.json` | a large repository of GGUF files (many quantisations and shards) | 8 | 395 |
| `hub-tree-pickle-imports.json` | a repository of pickle files, whose entries carry `securityFileStatus` with `pickleImportScan.pickleImports` lists | 1 | 17 |

## How they were captured

On 2026-10-17, with `tools/capture-hub-tree.py` (standard library only, no `Authorization` header):

```
python3 -I tools/capture-hub-tree.py <repo-id> hub-tree-<kind>.json
```

It sends the request the native model manager sends,
`GET https://huggingface.co/api/models/<repo-id>/tree/main?recursive=true&expand=true`, and follows each
`Link: <...>; rel="next"` header like `hf.repo_detail` (the Hub pages this listing by 50 entries).
## Anonymised

The fixtures are anonymised: the repository ids, and every model, organisation and
product name in paths, commit titles, `reportLink` URLs, scanner names and pickle module names,
were replaced by neutral tokens (`example-org/large-gguf-repo`, `example-org/pickle-repo`,
`example-model`, `scan-a.example`, `tensorlib`, ...). Quantisation and shard tokens (`Q4_K_M`,
`BF16`, `-00001-of-00005`), file extensions, hashes, sizes and dates are kept. The replacement
was deterministic, and the structure is exactly the Hub's: the same keys, the same nesting and
the same entry and node counts as captured. Only a few string lengths changed, and the table
below was measured on the anonymised files the tests read.

Nothing was trimmed: both listings are complete (`"complete": true`). Apart from the anonymisation,
the pages are as the Hub sent them, in this wrapper:

```
{"captured": "<date>", "request": "<first URL>", "complete": true, "pages": [<page 1>, <page 2>, ...]}
```

The tests re-serialise each page compactly with `serde_json` before reading it (which may reorder
keys and drops spacing, so what they parse is not the file's bytes); the node and string counts are
unaffected. The tests (`hub_page::tests::real_hub_pages_stay_far_below_the_limits`,
`draft_heads::tests::real_hub_listings_project_into_files`,
`check_updates::tests::real_hub_pages_give_a_date_per_file`) read each page back through the
capped reader and the shared parse. The data is a snapshot: the Hub may list other files today.

## Measured (the nodes are counted as `model_files::json` counts them: values and object keys)

| | GGUF | Pickle |
| --- | --- | --- |
| Entries | 395 | 17 |
| Nodes per entry, average | 50.4 | 261.2 |
| Nodes per entry, smallest to largest | 17 to 71 | 55 to 304 |
| Largest page, nodes | 3,551 (1.4 % of the cap) | 4,440 (1.7 % of the cap) |
| Longest string, characters | 168 (0.3 % of the cap) | 164 (0.3 % of the cap) |
| Largest parsed page, heap | 0.26 MiB | 0.31 MiB |

The earlier estimate of 30 to 60 nodes per expanded entry holds for GGUF files. A pickle file
costs more: each entry of its `pickleImports` list is an object of 3 keys and 3 values (7
nodes), and the largest entry here lists 33 imports. At 304 nodes an entry, a page would need
about 860 entries to reach the node cap, where the Hub sends 50.
