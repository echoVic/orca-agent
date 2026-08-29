#!/usr/bin/env node

import assert from "node:assert/strict";
import test from "node:test";

import { validateExecutionBrokerBoundary } from "./validate-execution-broker-boundary.mjs";

test("direct ProcessJob launches are rejected outside the broker/platform boundary", () => {
  assert.throws(
    () =>
      validateExecutionBrokerBoundary({
        sourceOverrides: new Map([
          ["crates/orca-runtime/src/forged_launch.rs", "fn run() { ProcessJob::spawn(&mut command); }"],
        ]),
      }),
    /direct ProcessJob launch/,
  );
});

test("broker and platform implementation remain allowed", () => {
  assert.doesNotThrow(() =>
    validateExecutionBrokerBoundary({
      sourceOverrides: new Map([
        ["crates/orca-core/src/execution_broker.rs", "ProcessJob::spawn(&mut command);"],
        ["crates/orca-platform/src/process.rs", "ProcessJob::spawn(&mut command);"],
      ]),
    }),
  );
});

validateExecutionBrokerBoundary();
console.log("execution broker boundary validator tests passed");
