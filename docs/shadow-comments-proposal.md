# Shadow comments and local review mode

Status: proposal. These interfaces and commands are sketches for a future feature.

Add a local review layer to Helix: threaded comments attached to source ranges,
stored beside the source file, plus a diff view with green edits and expandable
deleted text. People and agents use the same threads and can resolve or reopen
them. The editor remains the place where code is edited and discussed.

## The editing experience

A small gutter marker identifies an open thread. Selecting a range and choosing
**Comment** opens a Markdown composer. Opening an existing marker shows the
conversation in a right-hand panel, with the quoted code above the replies.
On a narrow terminal, the conversation uses a popup instead. The panel offers
Reply, Resolve, Reopen, and navigation to the next open thread.

Resolved threads remain available through a filter. A faint check mark can show
them in the gutter when that filter is enabled. An edit that removes the quoted
code marks a thread as outdated; resolving the discussion is a separate action.

Enabling review mode adds the diff to this experience:

| Element | Proposed appearance and action |
| --- | --- |
| Added or changed current lines | Subtle green line backgrounds; stronger emphasis on changed fragments. Syntax colors remain readable. |
| Collapsed deletion | A virtual row reading `▶ 3 lines deleted · expand`, with a red accent. |
| Expanded deletion | Read-only red rows showing the previous text and its original line numbers. Collapse returns to the compact marker. |
| Open discussion | A gutter dot and reply count; opening it focuses the conversation. |
| Resolved discussion | A faint check mark, shown when resolved threads are included. |
| Outdated discussion | A distinct marker and its original quote; a Reattach action selects a new range. |

Deleted rows and comment summaries occupy display space without becoming source
text. Typing and selection operate on actual document positions; the conversation
panel and deleted-text viewer have their own focus and navigation. Deletions at
the beginning or end of a file receive the same expandable markers.

The proposed default comparison is Git HEAD, resolved to a specific commit and
blob when review starts. A base picker also offers another revision, the index,
or a saved snapshot. Choosing a new base recomputes the displayed diff while
preserving conversations. Outside Git, start with a snapshot of the file.

All views use ordinary terminal cells: colored backgrounds, gutter glyphs, and
box borders. The compact markers have ASCII alternatives.

## Three visual prototypes

1. **[Review overlay](prototypes/shadow-comments/01-review-overlay.png):**
   a full-width editor with green changes, a collapsed
   deletion, and a compact thread marker. This is the default review view.
2. **[Thread inspector](prototypes/shadow-comments/02-thread-inspector.png):**
   the same editor with a conversation panel containing
   replies from you and Codex, a reply composer, and Resolve.
3. **[Expanded deletion](prototypes/shadow-comments/03-expanded-deletion.png):**
   the old three lines appear as red virtual rows above
   their replacement, with a discussion attached to the deleted side.

Use the first layout for scanning, the second for discussion, and the third for
understanding what was removed. The prototypes illustrate the proposed UI.

## The companion file

For `src/session.ts`, use `src/.session.ts.hx-review.json`. Create it on the first
comment or explicitly saved review. Keep it out of version control with
`*.hx-review.json` in the repository's local `.git/info/exclude`, or put the same
pattern in `.gitignore` when the team wants that convention.

The sidecar stores a schema version, file identity, comparison base, and threads.
Each thread has a stable ID, an anchor, open or resolved status, and ordered
messages. Messages carry their own IDs, author identities, timestamps, and
Markdown bodies. Resolution records who resolved the thread and when.

An abbreviated example, with placeholder IDs and hashes:

```json
{
  "version": 1,
  "file": "session.ts",
  "base": {
    "kind": "git",
    "ref": "HEAD",
    "commit": "<resolved commit>",
    "blob": "<base blob>"
  },
  "threads": [
    {
      "id": "thread-01",
      "anchor": {
        "side": "working",
        "start": { "line": 43, "character": 2 },
        "end": { "line": 43, "character": 43 },
        "quote": "const signal = AbortSignal.timeout(5000);",
        "before": "export async function loadSession() {",
        "after": "const response = await fetch(url, { signal });",
        "sourceHash": "<hash of the anchored source>",
        "state": "attached"
      },
      "status": "open",
      "messages": [
        {
          "id": "message-01",
          "author": "user:mewhhaha",
          "createdAt": "2026-10-02T11:00:00Z",
          "body": "Should this also stop when the view closes?"
        },
        {
          "id": "message-02",
          "author": "agent:codex",
          "createdAt": "2026-10-02T11:01:00Z",
          "body": "I can combine the timeout with the parent signal."
        }
      ]
    }
  ]
}
```

Positions are zero-based and refer to source text, with a documented Unicode
indexing convention. Anchors on deleted text use `side: "base"` and store their
own pinned blob ID, preserving the original context when the comparison base
changes. A snapshot base stores its text once so it can supply deleted rows.

## Keeping comments attached

During editing, map ranges through Helix transactions, including undo and redo.
Refresh the saved quote and neighboring context when a range remains attached.
On reopening or after external edits, locate the quote using its context and
nearby position. Accept a unique match; ambiguous matches go into an unplaced
thread list for manual reattachment.

If the selected text is replaced or deleted, preserve the original quote and
conversation as outdated. Comments save independently of the source buffer;
on reopening, a comment about unsaved text may require reattachment. A rename
performed through Helix should move its companion file. An external rename
should offer to associate the existing review with the new file.

## People and agents

An agent can read the source and the review through a small structured CLI,
then add a reply to a particular thread. A file watcher brings replies into the
open editor and marks new messages. Opening a discussion displays the stored
conversation; running an agent remains an explicit action.

Proposed operations are list threads, create thread, reply, resolve, reopen,
and reattach. Stable IDs let agents address a thread without relying on its
current line number. A shared writer should lock briefly, read the latest version,
merge messages by ID, and atomically replace the file. Concurrent status changes
should retain enough history for reconciliation. Invalid external edits
should preserve the last usable review and report the sidecar error.

Proposed editor commands:

| Command | Action |
| --- | --- |
| `:review-mode` | Toggle the diff overlay and review controls. |
| `:review-base` | Choose a revision or snapshot. |
| `:review-comment` | Comment on the selection. |
| `:review-thread` | Open the discussion at the cursor. |
| `:review-next` | Jump to the next open thread. |
| `:review-resolve` | Resolve the focused thread. |
| `:review-deletion` | Expand or collapse the deletion at the cursor. |

Actual keybindings can be chosen after trying the prototype in an editor.

## Building it in Helix

Helix already has a Git diff gutter, document transactions, virtual line
annotations, and Markdown rendering. Use those as the starting points:
`helix-view` owns review state and anchors; `helix-core` supplies range mapping
and layout primitives; `helix-term` draws the markers, diff rows, and thread
panel. Expanded deletions need explicit focus, line mapping, and selection
behavior in addition to drawing virtual rows.

Compute diffs and anchor recovery in the bounded background worker pool. Cancel
superseded work and apply results only to the document version and review base
that requested them. Cache diff hunks, thread layouts, and Markdown. Render
visible rows and debounce sidecar writes rather than rewriting on every keypress.

## First implementation

Start with range comments, replies, resolve and reopen, the companion file, and
transaction-aware anchors. Then add the selectable base, green edits, folded
deletions, and the deleted-text viewer. Finish with agent operations and external
file watching so both participants can use the same review safely.

Before expanding the scope, verify anchors through Unicode edits, undo, external
changes, and renames; concurrent replies; comments on deleted text; and virtual
row navigation with soft wrapping and split views.
