This is a real Rust workspace. Make a small PR-sized change to the `orca-file-search` crate:

1. Add a new module `src/rank.rs` with:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankedFile {
    pub path: String,
    pub score: u32,
    pub hits: u32,
}

pub fn rank_matches(matches: &[SearchMatch], max_results: usize) -> Vec<RankedFile>;
```

`rank_matches` groups matches by `path`, sums their `score` into `RankedFile::score`, counts the
matches into `RankedFile::hits`, sorts by score descending and then by path ascending, and truncates to
`max_results` entries (`max_results == 0` returns an empty vec).

2. Declare the module and re-export `RankedFile` and `rank_matches` from `src/lib.rs`.

`cargo test -p orca-file-search` must stay green. A hidden integration test will be injected afterwards;
it imports `orca_file_search::{rank_matches, RankedFile, SearchMatch, MatchKind}` and checks aggregation,
ordering, ties, and the zero limit.
