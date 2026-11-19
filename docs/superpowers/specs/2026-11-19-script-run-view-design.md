# Seeing a Script Run in the TUI Design

- Date: 2026-11-19
- Status: approved in conversation
- Baseline: `feat/script-quickjs-engine` after its stage-1 plan
  (`2026-11-19-script-quickjs-engine-design.md`)

## Purpose

When the model calls `run_script`, the person watching sees one line per
tool call the script made, such as `grep TODO · ok · 12ms`, and when the
script ends those lines are replaced by what it returned. The script the
model wrote is never shown. What the model was told about the tool is not
shown anywhere.

This change lets the person see, in the conversation itself:

- the script the model wrote;
- each call it made, with its arguments, how it ended and the start of what
  came back, and which calls ran together;
- what went back to the model;

and, with a new `/scripts` command, the text that teaches the model to use
the tool.

## What is shown

### The row, collapsed

One line, as today, ending with what the runtime said of the run:

```
● run_script · 4 tool calls · 412ms · completed
```

### The row, expanded

Expanded with the gesture every expandable row uses (a click, or ctrl+o):

```
● run_script · 4 tool calls · 412ms · completed
  Script
   1 │ const hits = await tools.grep({ pattern: "TODO" });
   2 │ const files = await Promise.all(paths.map((path) => tools.read_file({ path })));
   3 │ return files.filter((text) => text.includes("fixme")).length;
  Calls
    1   grep {"pattern":"TODO"} · ok · 12ms · 2.1 KiB
          src/a.rs:3: // TODO remove
          … 40 more lines
    2 ┬ read_file {"path":"a.rs"} · ok · 3ms · 1.2 KiB
          fn main() {
          … 30 more lines
    3 ├ read_file {"path":"b.rs"} · ok · 4ms · 8.0 KiB
          use std::io;
          … 211 more lines
    4 └ read_file {"path":"c.rs"} · failed · 1ms
          c.rs does not exist
  Returned to the model · 1 B
   2
```

`┬`, `├` and `└` join the calls of one batch: calls the script started
together, which ran at the same time. A call on its own has no mark.

A script that failed shows `Failed` and the error where a completed one
shows `Returned to the model`. A script still running shows `Script` and
the calls answered so far.

The three sections stay after the script ends. Today the call lines are
lost at that moment, because the result replaces them.

### `/scripts`

Prints one expanded system message into the conversation:

```
run_script is on
  60s per script · 100 tool calls · 64 MiB · 8 calls at once · 256 KiB per result
  a script can call: read_file, glob, grep, git_status

Description sent to the model
  Run JavaScript that calls read-only tools itself and returns only what it
  computes. …

Parameters sent to the model
  {
    "type": "object",
    "properties": { "script": { "type": "string", "description": "…" } },
    "required": ["script"]
  }
```

When the tool is off the first line is `run_script is off` followed by
``set `enabled = true` under [tools.script] in the user configuration and
start a new session``, and the description and parameters are still shown:
they are what the model would be sent.

The description and the parameters are the registered tool's own, read
from the tool registry, not a copy kept in the TUI. The limits are the
session's configuration.

## Where each part comes from

### The script

The runtime surface already carries a tool request's arguments
(`SurfaceToolRequest.raw_arguments`), and so does the `tool.call.requested`
event. The TUI's `ToolRequested` event drops them. For `run_script`, and
only for it, the two projections that build `ToolRequested`
(`surface_projection.rs` and `hosted_session.rs`) now pass the arguments
on, and the reducer takes `script` out of them. Arguments that are not the
expected JSON leave the row without a `Script` section; nothing fails.

### The calls

The runtime writes what a script's calls did to the tool's output stream
(`tool.output.delta`), one line per call, when a batch ends. That stays the
channel. No surface type is added, so the surface contract and its hashes
do not change. What is written per call changes from one line to:

```
{n} {mark} {tool} {arguments} · {ok|failed} · {ms}ms[ · {size}]
      {preview line}
      … {k} more lines
```

- `n` is the call's number, the same `n` as in its request id
  `<run_script call id>:<n>`, right-aligned in three columns.
- `mark` is a space for a batch of one; for a batch of several, `┬` on the
  first call, `└` on the last, `├` between.
- `arguments` is the call's arguments as compact JSON, cut at 200
  characters with `…`.
- `size` is the size of the result the script was handed, as `312 B`,
  `2.1 KiB`; absent for a call that failed or was refused.
- The preview is the first lines of the result, or of the error: at most 5
  lines, each cut at 120 characters, at most 400 bytes in all, each
  indented by ten spaces. When lines were left out, a last line says how
  many: `… 40 more lines`. A result with no text has no preview.

A preview line always starts with at least six spaces and a summary line
never does, so the two cannot be mistaken for each other by a reader or by
a program counting calls.

A call past the script's call limit is, as today, not counted and not
written.

The last line of the stream changes from `4 tool calls` to
`4 tool calls · 412ms`, the time from the script's start to its end. The
collapsed row shows that line; the TUI computes nothing from the stream.

One call adds at most about 700 bytes to the stream, so a script that makes
all 100 calls it is allowed adds about 70 KiB to the session's record. The
full results are not kept: one may be 256 KiB and a script may make 100
calls.

### What went back

Unchanged: the tool's result.

### In the TUI

`ChatMessage::ToolCall` gains one field for a script's run: the script's
source and the text of its calls. For a `run_script` row, output chunks are
appended to the calls text instead of `output`, and completion sets
`output` to the result without touching it. Rows of other tools behave as
they do now.

## Sessions resumed or reattached

A resumed session's rows are rebuilt from its saved conversation
(`hosted_session.rs`): the assistant message that requested the tool and
the tool message that answered it. Output chunks are not part of the saved
conversation and are not replayed.

So a `run_script` row of a resumed session shows `Script`, taken from the
saved request's arguments, and `Returned to the model`, taken from the
saved result. In place of the calls it says
`Calls are shown only in the session that ran the script`. The rebuild
already pairs a result with the request it answers (`HistoryReplay`), to
restore the row's name and target; it now keeps a `run_script` request's
arguments too.

Keeping the calls of a run across a resume means saving their text with
the conversation, which changes what a session file holds. That is not
part of this change.

A session reattached while its script is still running receives the chunks
written after it attached; the row shows those.

## Who else sees the change

The output stream of a `run_script` call is also what `orca exec` prints in
text mode, what its JSONL carries in `tool.output.delta`, and what an ACP
client receives as the tool's output. They get the same, more detailed
text. No field is added, renamed or removed.

`scripts/eval/code_mode/run.py` counts a script's calls by lines containing
` · `. It changes to count summary lines only: lines that match
`^\s*\d+ . \S+ .* · (ok|failed) · \d+ms`.

Previews put the start of what a script read on the screen and into the
session's record. A direct `read_file` already does both, in full. The
model receives none of it: it gets only what the script returned.

## Limits

| What | Limit |
|---|---|
| Arguments shown for one call | 200 characters |
| Preview of one result or error | 5 lines, 120 characters each, 400 bytes |
| Added to the stream by one call | about 700 bytes |

They are constants. Nothing is configurable.

## Testing

- **Runtime** (`script/calls.rs`): the summary line for a lone call and for
  each position in a batch; arguments cut at the limit; size formatting;
  the preview's three limits and its `more lines` count; an empty result;
  a failed call's preview is its error; a refused call (a tool outside the
  list) has a line and a preview of why; a call past the limit has neither;
  no preview line can match the summary pattern. The last line carries the
  time.
- **TUI reducer**: a `run_script` request carries its script; chunks go to
  the calls text; completion keeps them and sets the result; arguments that
  are not JSON leave no script and fail nothing; rows of other tools are
  unchanged.
- **TUI rendering**: the collapsed line; the three sections expanded; a
  failed run; a running run; a run with no calls.
- **Resume**: a saved conversation holding a `run_script` request and its
  result is rebuilt as a row with the script, the
  result and the line saying where calls are shown; a saved result of any
  other tool is rebuilt as it is today.
- **Both projections** pass arguments for `run_script` and for no other
  tool.
- **`/scripts`**: on and off; the description and parameters equal the
  registry's; it is in the command list and its help.
- **End to end**: the one thing the TUI reads out of the stream is its last
  line. A `run_script` turn through the runtime asserts the shape of that
  line and of a summary line, and the TUI's tests use lines of the same
  shape, so the two halves cannot drift apart unnoticed.
- `scripts/eval/code_mode/test_run.py`: the call count for a stream in the
  new format, including a preview line that contains ` · `.

## Documentation

The configuration page's section on scripts says how to see a run and what
`/scripts` prints, in English and Chinese. The slash-command reference
lists `/scripts`.

## Out of scope

- A panel listing a session's runs, or comparing them.
- The full text of what a call returned.
- Keeping a run's calls across a resume.
- Replaying a recorded script.
- Showing the whole request sent to the model (system prompt, every tool's
  definition, the messages).
- Syntax highlighting of the script.
