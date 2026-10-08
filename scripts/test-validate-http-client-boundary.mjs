#!/usr/bin/env node

import assert from "node:assert/strict";
import test from "node:test";

import { codeOnly, validateHttpClientBoundary } from "./validate-http-client-boundary.mjs";

function validate(source, file = "crates/orca-provider/src/forged.rs") {
  return validateHttpClientBoundary({ sourceOverrides: new Map([[file, source]]) });
}

test("a qualified reqwest client outside the entry is rejected", () => {
  assert.throws(
    () =>
      validateHttpClientBoundary({
        sourceOverrides: new Map([
          ["crates/orca-runtime/src/forged.rs", "fn f() { let _ = reqwest::blocking::Client::new(); }"],
        ]),
      }),
    /direct reqwest client/,
  );
});

test("an imported or aliased reqwest client outside the entry is rejected", () => {
  assert.throws(
    () =>
      validateHttpClientBoundary({
        sourceOverrides: new Map([
          [
            "crates/orca-provider/src/forged.rs",
            "use reqwest::blocking::{Client as BlockingClient, Response};\nfn f() { BlockingClient::builder(); }",
          ],
        ]),
      }),
    /direct reqwest client/,
  );
});

test("the entry, and files that only name the client type, are allowed", () => {
  assert.doesNotThrow(() =>
    validateHttpClientBoundary({
      sourceOverrides: new Map([
        ["crates/orca-mcp/src/http.rs", "reqwest::Client::builder()"],
        ["crates/orca-provider/src/typed.rs", "use reqwest::Client;\nfn f(client: &Client) {}"],
      ]),
    }),
  );
});

// Each of these builds a client, and reqwest 0.13 panics on one built before
// a crypto provider is installed. One case for each form.
const rejected = [
  // `get` builds a client of its own.
  ["qualified reqwest::get", "async fn f() { let _ = reqwest::get(URL).await; }"],
  ["qualified reqwest::blocking::get", "fn f() { let _ = reqwest::blocking::get(URL); }"],
  ["a reference to reqwest::get", "fn f() { let _ = fetch_with(reqwest::get); }"],
  ["imported get", "use reqwest::get;\nasync fn f() { let _ = get(URL).await; }"],
  ["imported blocking get", "use reqwest::blocking::get;\nfn f() { let _ = get(URL); }"],
  ["aliased blocking get", "use reqwest::blocking::get as fetch;\nfn f() { let _ = fetch(URL); }"],
  ["get through an imported blocking module", "use reqwest::blocking;\nfn f() { let _ = blocking::get(URL); }"],
  ["get through an aliased crate", "use reqwest as http;\nasync fn f() { let _ = http::get(URL).await; }"],
  // `default` builds a client without the entry as `new` does.
  ["qualified Client::default", "fn f() { let _ = reqwest::Client::default(); }"],
  ["qualified blocking Client::default", "fn f() { let _ = reqwest::blocking::Client::default(); }"],
  ["qualified ClientBuilder::default", "fn f() { let _ = reqwest::ClientBuilder::default(); }"],
  [
    "qualified blocking ClientBuilder::default",
    "fn f() { let _ = reqwest::blocking::ClientBuilder::default(); }",
  ],
  ["a reference to Client::default", "fn f() { let _ = Some(1).map(|_| reqwest::Client::default); }"],
  ["imported Client::default", "use reqwest::Client;\nfn f() { let _ = Client::default(); }"],
  ["aliased Client::default", "use reqwest::Client as HttpClient;\nfn f() { let _ = HttpClient::default(); }"],
  [
    "imported blocking Client::default",
    "use reqwest::blocking::Client;\nfn f() { let _ = Client::default(); }",
  ],
  [
    "imported ClientBuilder::default",
    "use reqwest::{ClientBuilder, StatusCode};\nfn f() { let _ = ClientBuilder::default(); }",
  ],
  [
    "aliased blocking ClientBuilder::default",
    "use reqwest::blocking::{ClientBuilder as Builder, Response};\nfn f() { let _ = Builder::default(); }",
  ],
  [
    "Client::default through an imported blocking module",
    "use reqwest::blocking;\nfn f() { let _ = blocking::Client::default(); }",
  ],
  [
    "ClientBuilder::default through an aliased crate",
    "use reqwest as http;\nfn f() { let _ = http::ClientBuilder::default(); }",
  ],
  [
    "Client::default through a self import",
    "use reqwest::{self as http, blocking::{self}};\nfn f() { let _ = http::Client::default(); let _ = blocking::Client::default(); }",
  ],
  ["Client::default through a glob import", "use reqwest::blocking::*;\nfn f() { let _ = Client::default(); }"],
  ["get through a glob import", "use reqwest::*;\nasync fn f() { let _ = get(URL).await; }"],
  [
    "Client::default through a type alias",
    "type Http = reqwest::blocking::Client;\nfn f() { let _ = Http::default(); }",
  ],
  [
    "a global-path reqwest client",
    "fn f() { let _ = ::reqwest::Client::default(); }",
  ],
  // A comment is not code: a `use` in one is no declaration, so it cannot
  // swallow the import that follows it, and a line in a block comment that
  // starts with `use` cannot swallow a call.
  [
    "Client::new after an import that follows a doc comment saying use",
    "//! Helpers that use\n//! the network.\nuse reqwest::blocking::Client;\nfn f() { let _ = Client::new(); }",
  ],
  [
    "Http::builder after an aliased import that follows a line comment saying use",
    "// we use the blocking client here\nuse reqwest::blocking::{Client as Http, Response};\nfn f() { let _ = Http::builder(); }",
  ],
  [
    "Client::default after an import that follows a block comment saying use",
    "/* we use\n   the blocking client here */\nuse reqwest::blocking::Client;\nfn f() { let _ = Client::default(); }",
  ],
  [
    "a qualified call after a block comment with a line that starts with use",
    "/*\nuse the shared client\n*/\nfn f() { let _ = reqwest::Client::new(); }",
  ],
  [
    "a qualified call after a line comment that starts with use",
    "// use the shared client;\nfn f() { let _ = reqwest::Client::new(); }",
  ],
  [
    "a qualified call after a doc comment with a use and a semicolon",
    "/// Callers use it; they must not build their own.\nfn f() { let _ = reqwest::Client::new(); }",
  ],
  // An import is read wherever an item can start.
  [
    "Client::new after an import that follows an attribute",
    "#[cfg(test)]\nuse reqwest::blocking::Client;\nfn f() { let _ = Client::new(); }",
  ],
  [
    "Http::new after a pub(crate) import",
    "pub(crate) use reqwest::Client as Http;\nfn f() { let _ = Http::new(); }",
  ],
  [
    "Client::new after an import inside a function",
    "fn f() { use reqwest::blocking::Client; let _ = Client::new(); }",
  ],
  [
    "Client::new after an import that follows another import",
    "use std::time::Duration; use reqwest::blocking::Client;\nfn f() { let _ = Client::new(); }",
  ],
  [
    "a call after a use bound that is no declaration",
    "fn f<'a>(text: &'a str) -> impl Sized + use <'a> { let _ = reqwest::Client::new(); text }",
  ],
  // Reading the source as code must not be fooled by the tokens that look
  // like the start of a comment or a string: a real call after each of them
  // is still seen.
  [
    "a call after a string with // in it",
    'fn f() { let _url = "http://example.invalid"; let _ = reqwest::Client::new(); }',
  ],
  [
    "a call after a string that looks like a commented call",
    'fn f() { let _note = "// use reqwest::Client::new()"; let _ = reqwest::Client::new(); }',
  ],
  [
    "a call after a string that holds a block comment opener",
    'const OPEN: &str = "/*"; fn f() { let _ = reqwest::Client::new(); }',
  ],
  [
    "a call after a string with an escaped quote",
    'fn f() { let _s = "say \\"hi\\" // later"; let _ = reqwest::Client::new(); }',
  ],
  [
    "a call after a raw string with quotes and // in it",
    'fn f() { let _s = r#"say "hi" // later"#; let _ = reqwest::Client::new(); }',
  ],
  [
    "a call after a byte string",
    'fn f() { let _s = b"// "; let _ = reqwest::Client::new(); }',
  ],
  [
    "a call after a lifetime",
    "fn f<'a>(text: &'a str) -> &'a str { let _ = reqwest::Client::new(); text }",
  ],
  [
    "a call after a label",
    "fn f() { 'outer: loop { let _ = reqwest::Client::new(); break 'outer; } }",
  ],
  [
    "a call after a quote character",
    "fn f() { let _quote = '\"'; let _ = reqwest::Client::new(); }",
  ],
  [
    "a call after an escaped quote character",
    "fn f() { let _quote = '\\''; let _ = reqwest::Client::new(); }",
  ],
  [
    "a call after a slash character",
    "fn f() { let _slash = '/'; let _ = reqwest::Client::new(); }",
  ],
  [
    "a call after a quote in a comment",
    '// don\'t "quote\nfn f() { let _ = reqwest::Client::new(); }',
  ],
  [
    "a call after a nested block comment",
    "/* outer /* inner */ still outer */ fn f() { let _ = reqwest::Client::new(); }",
  ],
  [
    "a call on the line where a block comment ends",
    "/* a\n   b */ fn f() { let _ = reqwest::Client::new(); }",
  ],
];
for (const [name, source] of rejected) {
  test(`${name} outside the entry is rejected`, () => {
    assert.throws(() => validate(source), /direct reqwest client in crates\/orca-provider\/src\/forged\.rs/);
  });
}

// A comment or a string is not code, however much it looks like a client.
const allowed = [
  [
    "a string that holds a commented call",
    'fn f() -> &\'static str { "// use reqwest::Client::new()" }',
  ],
  [
    "a multi-line string with a line that is a commented call",
    'const HELP: &str = "first\n// use reqwest::Client::new()\nlast";',
  ],
  [
    "a string that holds an import and a call",
    'const HELP: &str = "use reqwest::blocking::Client; Client::new()";',
  ],
  [
    "a raw string that holds a program",
    'const FIXTURE: &str = r#"\nuse reqwest::blocking::Client;\nfn main() { Client::new(); }\n"#;',
  ],
  [
    "a string with escaped quotes around a call",
    'fn f() -> &\'static str { "say \\"reqwest::Client::new()\\" now" }',
  ],
  [
    "a raw string with quotes around a call",
    'fn f() -> &\'static str { r#"say "reqwest::Client::new()" now"# }',
  ],
  ["a line comment that names a call", "// Built with reqwest::Client::new() before.\nfn f() {}"],
  [
    "a doc comment that names a call",
    "/// Not reqwest::blocking::Client::default(): build it with the entry.\nfn f() {}",
  ],
  ["an inner doc comment that names a call", "//! Never reqwest::get(url).\nfn f() {}"],
  ["a block comment that names a call", "/* reqwest::get(url) */\nfn f() {}"],
  [
    "a nested block comment that names a call after its inner comment ends",
    "/* outer /* inner */ reqwest::Client::new() still outer */\nfn f() {}",
  ],
  [
    "a commented-out import and call",
    "// use reqwest::blocking::Client;\n// let _ = Client::new();\nfn f() {}",
  ],
  [
    "a call of another Client after a commented-out import",
    "/*\nuse reqwest::blocking::Client;\n*/\nfn f() { let _ = Client::new(); }",
  ],
];
for (const [name, source] of allowed) {
  test(`${name} is allowed`, () => {
    assert.doesNotThrow(() => validate(source));
  });
}

test("a file the validator cannot read to its end is rejected, not skipped", () => {
  for (const [source, what] of [
    ["/* never closed\nreqwest::Client::new()", "a block comment never ends"],
    ['fn f() { let _ = "never closed; reqwest::Client::new(); }', "a string literal never ends"],
    ['fn f() { let _ = r#"never closed" ; reqwest::Client::new(); }', "a raw string literal never ends"],
  ]) {
    assert.throws(
      () => validate(source),
      new RegExp(`cannot read crates/orca-provider/src/forged\\.rs: ${what}`),
    );
  }
});

test("codeOnly blanks comments and the insides of literals, and keeps the rest", () => {
  const source = [
    "let a = 1; // note",
    'let b = "x // y"; /* c',
    "d */ let c = 'q'; let e: &'a str = r#\"z\"#;",
  ].join("\n");

  const code = codeOnly(source);

  assert.equal(code.length, source.length);
  assert.deepEqual(
    code.split("\n").map((line) => line.length),
    source.split("\n").map((line) => line.length),
  );
  for (const gone of ["note", "x // y", "d */", "'q'", "z"]) {
    assert.ok(!code.includes(gone), `${gone} should be blanked out of: ${code}`);
  }
  for (const kept of ["let a = 1;", "let b =", "let c =", "let e: &'a str ="]) {
    assert.ok(code.includes(kept), `${kept} should be kept in: ${code}`);
  }
});

test("code that only looks like a reqwest client call is allowed", () => {
  assert.doesNotThrow(() =>
    validate(
      [
        "use reqwest::StatusCode;",
        "use reqwest::header::{ACCEPT, HeaderMap};",
        "struct Cache { map: HashMap<String, String> }",
        "impl Cache {",
        "    fn get(&self, key: &str) -> Option<&String> { self.map.get(key) }",
        "}",
        "fn f(cache: &Cache) { cache.get(\"a\"); other::get(\"b\"); McpClient::default(); }",
        "fn status() -> StatusCode { StatusCode::OK }",
      ].join("\n"),
    ),
  );
  assert.doesNotThrow(() =>
    validate("use reqwest::Client;\nfn f() { let _ = other::Client::default(); let _ = AcpClient::new(); }"),
  );
  assert.doesNotThrow(() =>
    validate("use reqwest::get;\nfn get_all() {}\nfn f() { let _ = get_all(); }"),
  );
  // Even in a file that imports reqwest's `get`: a method, a definition and a
  // path are not a call of it.
  assert.doesNotThrow(() =>
    validate(
      [
        "use reqwest::get;",
        "impl Cache {",
        "    fn get(&self, key: &str) -> Option<&String> { self.map.get(key) }",
        "}",
        "fn f(cache: &Cache) { cache.get(\"a\"); other::get(\"b\"); }",
      ].join("\n"),
    ),
  );
});

validateHttpClientBoundary();
console.log("http client boundary validator tests passed");
