#!/usr/bin/env node

import assert from "node:assert/strict";
import test from "node:test";

import { validateHttpClientBoundary } from "./validate-http-client-boundary.mjs";

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

validateHttpClientBoundary();
console.log("http client boundary validator tests passed");
