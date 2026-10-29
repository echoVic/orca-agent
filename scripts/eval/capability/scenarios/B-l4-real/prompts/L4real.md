This is a real Rust workspace. Add a new public function to the `orca-file-search` crate:

`pub fn normalize_query_path(input: &str) -> String`

It normalizes a user-typed path query:
- `.` segments are removed,
- empty segments (`//`) are collapsed,
- a trailing `/` is removed,
- `..` is kept and removes the previous segment when one exists (so `a/b/../c` becomes `a/c`),
- an empty input stays empty.

Re-export it from the crate root so `orca_file_search::normalize_query_path` resolves. Add a unit test
of your own if you like; the hidden tests for this contract are injected later. `cargo test -p
orca-file-search` must stay green.
