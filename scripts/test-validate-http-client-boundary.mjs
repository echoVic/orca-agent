#!/usr/bin/env node

import assert from "node:assert/strict";
import test from "node:test";

import { validateHttpClientBoundary } from "./validate-http-client-boundary.mjs";

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
];
for (const [name, source] of rejected) {
  test(`${name} outside the entry is rejected`, () => {
    assert.throws(() => validate(source), /direct reqwest client in crates\/orca-provider\/src\/forged\.rs/);
  });
}

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
