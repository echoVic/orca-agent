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

The row's first line is its icon, the tool's name and what the runtime said
of the run, its last line. A status word (`failed`, `interrupted`) follows
only when the run ended without completing, as on every tool row. Under it
one row says how to open it:

```
✓ run_script  4 tool calls · 412ms
└ +18 lines · click or ctrl+o to expand
```

### The row, expanded

Expanded with the gesture every expandable row uses (a click, or ctrl+o):

```
✓ run_script  4 tool calls · 412ms
│ Script
│  1 │ const hits = await tools.grep({ pattern: "TODO" });
│  2 │ const files = await Promise.all(paths.map((path) => tools.read_file({ path })));
│  3 │ return files.filter((text) => text.includes("fixme")).length;
│ Calls
│   1   grep {"pattern":"TODO"} · ok · 12ms · 2.1 KiB
│           src/a.rs:3: // TODO remove
│           … 40 more lines
│   2 ┬ read_file {"path":"a.rs"} · ok · 3ms · 1.2 KiB
│           fn main() {
│           … 30 more lines
│   3 ├ read_file {"path":"b.rs"} · ok · 4ms · 8.0 KiB
│           use std::io;
│           … 211 more lines
│   4 └ read_file {"path":"c.rs"} · failed · 1ms
│           c.rs does not exist
│ Returned to the model · 1 B
│  2
└ 18 lines · ctrl+o to collapse
```

`┬`, `├` and `└` join the calls of one batch: calls the script started
together, which ran at the same time. A call on its own has no mark.

A script that failed shows `Failed` and the error where a completed one
shows `Returned to the model`. A script still running shows `Script`, and
in a turn that delivers them the calls answered so far (see "Sessions
resumed or reattached").

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
event. The TUI's `ToolRequested` event drops them, and still does. For
`run_script`, and only for it, the two projections that build
`ToolRequested` (`surface_projection.rs` and `hosted_session.rs`) take
`script` out of the arguments and follow the request with a `ToolScript`
event that carries it. Arguments that are not the expected JSON leave the
row without a `Script` section; nothing fails.

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
- Control characters in the arguments and in each preview line (C0, DEL,
  C1, and U+2028 and U+2029) are written as `�`, and so are the characters
  that set the direction text is drawn in (U+202A to U+202E and U+2066 to
  U+2069). A tab is written as the spaces up to the line's next tab stop,
  four characters apart, so indentation reads as it was written. This is
  done before either is cut, so the limits are measured on what is written.

A preview line always starts with ten spaces and a summary line
never does, so the two cannot be mistaken for each other by a reader or by
a program counting calls. A carriage return in what a call read cannot
take a terminal back over that indent: it is written as `�`.

A call past the script's call limit is, as today, not counted and not
written.

The last line of the stream changes from `4 tool calls` to
`4 tool calls · 412ms`, the time from the script's start to its end. The
collapsed row shows that line; the TUI computes nothing from the stream.

One call adds at most about 1.3 KiB to the stream: 200 characters of
arguments can be 800 bytes, and a preview is at most 400 bytes of text plus
its indents. A script that makes all 100 calls it is allowed adds about
130 KiB to the session's record. The full results are not kept: one may be
256 KiB and a script may make 100 calls.

### What went back

Unchanged: the tool's result.

### In the TUI

The transcript keeps, per `run_script` call id, the run's parts: the
script's source, the text of its calls and how it ended
(`TranscriptState.script_runs`). Output chunks of a `run_script` row are
appended to the calls text instead of `output`, and completion records how
the run ended. After each, the row's `output` is set to what the expanded
row shows, composed from the parts, and its `target` to the run's last
line. A request for `run_script` starts its id's run afresh, so a call id
used again does not show the earlier run's parts. The lines of the script
and of the result, and the calls text line by line, are composed with
control characters replaced as the runtime replaces them, so a stream
recorded by an older runtime is as safe. Rows of other tools behave as they
do now.

## Sessions resumed or reattached

A script's calls are kept with its result. When `run_script` ends, the
runtime records everything it wrote to the output stream as the tool
view's streamed output (`SurfaceToolView.streamed_output`), in the same
recorded commit as the result: one `OutputDelta` before the `Completed`.
The field and the patch already existed and nothing had written them, so
the surface contract and its hashes do not change. A session file grows by
what the stream holds, at most about 130 KiB for a script that makes all
100 calls. The text is redacted as a result is.

A resumed or reattached session's rows are rebuilt from the surface's
snapshot of the conversation (`history_message_from_surface_item` in
`surface_projection.rs`): each tool result, with the request and result the
snapshot holds for its tool. A `run_script` row shows `Script`, taken from
the request's arguments, `Calls`, taken from the tool view's streamed
output, and `Returned to the model` or `Failed`, taken from the result. The
same rebuild happens within a session when the view returns to the main
conversation from `/side` or from a child's view, and shows the same.

A row whose record holds no calls says
`Calls were not recorded for this run` in their place. That is a run
recorded before calls were kept, and a conversation rebuilt from its saved
messages alone (`HistoryReplay` in `hosted_session.rs`, used only when the
snapshot holds no messages), which pairs each tool message with the
assistant message that requested it and has no tool views.

The recorded calls reach the TUI with the result, as `ToolOutputAt`, which
carries where the text starts and takes the place of what the row held from
there. In a turn the person started, that is how the calls arrive at all:
such a turn's `tool.output.delta` events go nowhere the TUI listens, so the
row shows `Script` while the script runs and gains `Calls` when it ends.
(Until calls were recorded, the row of such a turn never showed them.) A
turn the runtime starts from a message queued behind another turn does
deliver the events, so its row shows each batch as it ends, and nothing of
it is committed to the surface: its calls are shown live and are not kept.

What is kept is bounded at 256 KiB. A script allowed many more calls than
the default keeps the first of them, then a line saying how many lines were
left out, then the run's last line. A `run_script` call that ended before
its script ran (the tool is off, the arguments are not a script, two
scripts are still running after they were stopped) records
`0 tool calls · 0ms`. A result the runtime writes itself, for a call that
was cancelled before it started or was settled by recovery after a crash,
carries no calls, and its row says none were recorded.

Chunks that arrive for a run that has no row, as after a reattach while its
script is running, are not shown: a chunk carries only the call's id, so it
cannot be known to belong to a script. The row appears when the run ends,
with its calls and what went back to the model: recorded output that
arrives for a call with no row is held for the result that follows it. The
script itself, which came with the request, is shown once the row is next
rebuilt from the snapshot.

## Who else sees the change

The output stream of a `run_script` call is also what `orca exec` prints in
text mode and what its JSONL carries in `tool.output.delta`. They get the
same, more detailed text. No field is added, renamed or removed. An ACP
client has no path from `tool.output.delta`; it now receives the recorded
calls as one in-progress update of the tool call just before the completed
update, whose content takes its place.

`scripts/eval/code_mode/run.py` counts a script's calls by lines containing
` · `. It changes to count summary lines only: lines that match
`^ {0,2}\d+ . \S+ .* · (ok|failed) · \d+ms`. A summary line starts with at
most two spaces and a preview line with ten. The runner splits a chunk into
lines, and its JSONL into events, at `\n` only: `splitlines()` also ends a
line at a U+2028, which Orca's JSON writes as it is.

Previews put the start of what a script read on the screen and into the
session's record. A direct `read_file` already puts what it read in the TUI
and in the record, in full. `orca exec`'s text output prints nothing of a
direct read, so there previews are new; control characters in them are
replaced (see "The calls"), so what a script read cannot reach the terminal
as escape sequences. The model receives none of it: it gets only what the
script returned.

## Limits

| What | Limit |
|---|---|
| Arguments shown for one call | 200 characters |
| Preview of one result or error | 5 lines, 120 characters each, 400 bytes |
| Added to the stream by one call | about 1.3 KiB at most |

They are constants. Nothing is configurable.

## Testing

- **Runtime** (`script/calls.rs`): the summary line for a lone call and for
  each position in a batch; arguments cut at the limit; size formatting;
  the preview's three limits and its `more lines` count; an empty result;
  a failed call's preview is its error; a refused call (a tool outside the
  list) has a line and a preview of why; a call past the limit has neither;
  no preview line can match the summary pattern, even with a carriage
  return in it; control characters in a preview and in arguments are
  replaced, before the limits are measured. The last line carries the time.
- **TUI reducer**: a `run_script` request carries its script; chunks go to
  the calls text; completion keeps them and sets the result; arguments that
  are not JSON leave no script and fail nothing; a call id used again shows
  only its new run; rows of other tools are unchanged.
- **TUI composition**: control characters in the script, the result and an
  older runtime's calls are replaced, so a carriage return cannot make a
  line into a section title; clean calls are shown as they came.
- **TUI rendering**: the collapsed line; the three sections expanded; a
  failed run; a running run; a run with no calls; a run still receiving its
  arguments, drawn as any tool's row; a one-line body says `1 line`.
- **Resume**: a snapshot holding a `run_script` call, its recorded calls
  and its result is rebuilt as a row with all three; a snapshot without
  recorded calls, and a saved conversation holding a `run_script` request
  and its result, are each rebuilt as a row with the script, the result and
  the line saying no calls were recorded; a result of any other tool is
  rebuilt as it is today. Recorded output is projected with where it
  starts, and a row that already holds the same text shows it once.
- **Both projections** pass arguments for `run_script` and for no other
  tool.
- **`/scripts`**: on and off; the description and parameters equal the
  registry's; it is in the command list and its help.
- **End to end**: the one thing the TUI reads out of the stream is its last
  line. A `run_script` turn through the runtime asserts the shape of that
  line and of a summary line, and the TUI's tests use lines of the same
  shape, so the two halves cannot drift apart unnoticed.
- `scripts/eval/code_mode/test_run.py`: the call count for a stream in the
  new format, including a preview line that contains ` · `, preview lines
  shaped like summary lines, and a summary line with a U+2028 in its
  arguments.

## Documentation

The configuration page's section on scripts says how to see a run and what
`/scripts` prints, in English and Chinese.

## Out of scope

- A panel listing a session's runs, or comparing them.
- The full text of what a call returned.
- Replaying a recorded script.
- Showing the whole request sent to the model (system prompt, every tool's
  definition, the messages).
- Syntax highlighting of the script.
