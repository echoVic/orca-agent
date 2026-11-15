This is a real Rust workspace. Add two public functions to the `orca-file-search` crate:

1. `pub fn normalize_query_path(input: &str) -> String` — removes `.` segments, collapses `//`, drops a trailing `/`, and lets `..` remove the previous segment when one exists; empty input stays empty.
2. `pub fn normalize_query_paths(inputs: &[String]) -> Vec<String>` — normalizes each path with the first function, drops results that are empty, deduplicates while keeping the first occurrence's order.

Re-export both from the crate root. `cargo test -p orca-file-search` must stay green, and a hidden integration test will be injected afterwards.
