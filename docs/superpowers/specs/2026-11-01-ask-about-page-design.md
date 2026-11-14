# Ask about this page: one typed line, one answer card

Status: **proposed, 2026-11-01** (AGENTS.md rule 12: written before any code,
waits for the owner). Covers #116. Builds on #92 (`actions/knowledge_answer.rs`,
the first action that answers a typed line), #104 (`inputs/page_text.rs`),
#67 (`summarize-page`), the [palette spec](2026-09-17-palette-design.md) and the
[knowledge spec](2026-10-16-knowledge-and-memory-design.md). This spec contains
**one open question that touches an owner rule** (no chat). It is the first item
under "Open questions".

## The problem

A page is open: an article, a policy, a long email. The user wants one fact from it,
for example the refund window in the third section, below the fold. A screenshot only
holds what is visible, and Summarize this page gives the gist, not the answer to a
specific question. The user needs to type one line and get one answer.

## The rule this must respect

Owner rule (2026-09-16): **no chat. One press, one action, one card. No
conversations, no follow-ups.** The palette's text box was reworded as a filter, not a
chat box (PR #411). This spec keeps that rule and says exactly where the edge is:

- A question is **one typed line**, sent once, with the page text.
- The result is **one card**. The card has no input box, no reply field and no history.
- **Nothing is remembered between questions.** The next question is a new press, a new
  line and a new run. The previous question and answer are never sent again and never
  stored.
- It is the same shape as the shipped "Ask my documents" (#92): the user types a line in
  the palette and picks the row, and one answer card comes back.

Whether single-turn question and answer still counts as chat is the owner's call, and
is asked plainly at the end.

## Where it is today

- `Action` has `takes_text` (from `inputs` listing `text`). The palette lists every
  `takes_text` action after the name matches whenever the line is non-empty
  (`palette_model`, decision #92), so the line reaches it even though it matches no name.
  `dispatch_palette_action(action_id, line, trigger)` already carries the line.
- `knowledge_answer` is the pattern: schema ordered evidence first (`evidence`, `answer`,
  `found`), quotes checked in code against the excerpts, a source line built in code,
  executor `none`.
- `summarize-page` reads: selection, else `PageText` (UIA, up to `DEFAULT_MAX_CHARS` =
  60,000 characters, the whole document including the part below the fold), else the
  screen. `page_text::read_with_retry` runs on a worker. Over the cap it is cut and says so.
- `router::candidates_from_catalogue` builds the router's candidate list from the
  catalogue. The options card has no text line, so an action that needs a typed line
  cannot run from it.

## Decisions

- **A catalogue action, `ask-about-page`** ("Ask about this page"), `inputs = ["text",
  "page_text", "screen"]`, proposal `page_answer`, executor `none`, `requires_model`. It is
  a generic action, like #92, so it needs no new executor and no new dispatch path.
- **How the question enters: an explicit row, never auto-converted.** The palette already
  pins `takes_text` rows under a non-empty line. The user types a question and **selects
  the "Ask about this page" row** (Enter on it). Typed text that matches no action does
  **not** silently become a question. Two reasons: more than one action takes text (Ask my
  documents, this one), so there is no single "the question" target; and a typo in an
  action name must not send the page to a model.
- **Not offered from the options card or by the router.** There is no typed line there, so
  `candidates_from_catalogue` leaves out every `takes_text` action. (Verify the existing
  behaviour at implementation time; a test asserts it.)
- **An empty line is an info card, not a run:** "Type your question first." Rule 7: never a
  silent no-op and never a model call with an empty question.
- **A double press does not replay it.** The options spec's "repeat last action" for a
  `takes_text` action opens the palette with nothing run, so an old question is never
  re-sent by a stray tap (open question 3).
- **One question per run, stateless.** No session id, no conversation store, no "Ask
  another" button. The card offers the normal footer (Copy, Read aloud) and nothing that
  accepts text.

## Inputs: where the page text comes from

Order, chosen for the done-when (an answer from below the fold):

1. **`PageText`** (UIA, #104): the whole document, not only what is visible. This is the
   input that makes the done-when possible, so it comes first, unlike `summarize-page`
   where the selection comes first.
2. **Selection**, if there is no readable document: the user's own highlighted text is the
   page. The question is about it.
3. **The screen** (a screenshot), last, for pages with no UIA text (an image-only PDF, a
   remote desktop). The card says "Answered from what is visible, not the whole page" so
   the user knows a paragraph below the fold was not read.

If the user selected text **and** a document is readable, the document is used and the
selection is passed as a hint ("the user has this selected") so "what does this mean?" has
a referent. (Open question 4.)

Reading runs on a worker with the existing retry; the card shows the existing Reading
stage. The password-manager and payment-page gates run before anything is read, as for
every action.

## Long pages

`DEFAULT_MAX_CHARS` is 60,000 characters (about 15,000 tokens). Sending all of it for a
one-line question is wasteful and may not fit a small local model. Decision:

- Up to a **per-provider budget** (`ask.max_page_chars`, default 24,000, about 6,000
  tokens), the whole page goes.
- Over it, the page is split into overlapping chunks of about 2,000 characters on
  paragraph boundaries and the chunks are **ranked locally** against the question by word
  overlap (a pure scorer in the style of the knowledge excerpt finder; no embeddings, no
  model call). The top chunks that fit the budget are sent **in page order**, each tagged
  with its position ("part 4 of 19"). The card says "Searched the whole page" or "The page
  was long, so Wingman sent the parts that matched your question".
- THEORY (unverified): word overlap finds the right paragraph often enough for plain
  factual questions and misses paraphrased ones. Manual check 3 measures it on a long
  document; if it misses too often the default budget is raised instead of adding a
  model-based ranker.

## Proposal and the answer

Property order is load-bearing (AGENTS.md rule 3), evidence first as in #92:

```json
{ "properties": {
    "evidence": { "type": "array", "items": { "quote": "string", "part": "integer" } },
    "answer": { "type": "string" },
    "found": { "type": "boolean" } },
  "required": ["evidence", "answer", "found"], "additionalProperties": false }
```

- Each `quote` must appear in the part it names (case and white space ignored). A quote
  that does not is dropped. With none left, `found` is forced false and the card says
  "The page did not say" and shows nothing the model invented. This is `knowledge_answer`'s
  `finalize`, reused, not rewritten.
- The card shows the question as a small line above the answer ("You asked: ...", cut to
  120 characters), then the answer, then "From part 4" built in code from the verified
  quote, never from model text.
- The page text is **data**. Instructions inside it ("ignore the above and ...") cannot start
  anything: the executor is `none`, links in the answer are shown and never opened, and the
  system prompt says the page is untrusted text. An answer is plain text with markdown
  characters stripped, as every card.

## Egress and redaction

- Cloud: the page text and the question both leave the machine. They go through the existing
  text redaction first (card numbers, IBANs, key-shaped tokens, emails), and a returned
  answer that contains a redaction marker has it restored only for display where the
  existing `restore` path allows, else the marker is refused. "Confirm before sending"
  (#105) shows the send preview with the question and the exact page text that will go,
  per the existing guard. A page from a password manager or a payment page is refused by the
  existing gates before it is read.
- Local (Ollama): nothing leaves the machine. The budget is clamped to the model's context.
- Offline mode with no local model: the action is hidden from the palette (it needs a
  model), as other model actions are.
- The question and the page text are never written to the diagnostics log; the egress log
  records the request as it does for every provider call.

## Failure cards

| Case | Result |
|---|---|
| Empty line | Info card "Type your question first." |
| No page text, no selection, screen capture fails | The existing capture error card |
| Page too long and no chunk matched | "I could not find a part of the page about that." No model call |
| The model's quotes all fail the check | "The page did not say." with the closest parts named |
| Paused, password manager, payment page, send guard | The existing gate cards |
| Refusal or timeout from the model | The existing provider error cards |

## Testing

Pure logic is unit-tested (rule 8); tests come first.

- Chunking and ranking: a long fixture with the answer in section 3; the right part is in the
  sent set; page order is preserved; a question with no overlapping word sends the head and
  the card says it; chunk cuts are on char boundaries (multi-byte text never panics); the
  budget is never exceeded.
- Finalise: a supported quote passes and builds "From part N"; an invented quote is dropped;
  none left makes `found` false; the model never supplies the source line; markdown stripped.
- Schema property order (`evidence`, `answer`, `found`) asserted.
- Palette: with a non-empty line the row is pinned under it; with an empty line the action
  runs to an info card, not a model call; the options card and `candidates_from_catalogue`
  never list a `takes_text` action; a double press on it opens the palette and runs nothing.
- Input order: `PageText` beats selection beats screen; a selection plus a document gives the
  document with the hint.
- A source-scan guard (CRLF-normalised, file set asserted non-empty) that the action holds no
  session or history state and that no card kind for this action has a text input.
- Tests use no real UIA window and no named kernel objects (rule 9).

### Manual checks (each names the observable that differs if wired to nothing)

1. **Below the fold.** A long article; ask about a sentence two screens down. Observable: the
   answer quotes that sentence's part. A screenshot-only version would say it did not find it.
2. **No chat.** After the answer, observe the card. Observable: no input, no follow-up button;
   the next question needs a new press and a new line, and the previous answer is not in the
   request (the egress log row shows only the new question).
3. **Long page.** A 100,000-character page, an answer in the middle. Observable: the card says
   it searched the whole page, and the answer is right; record the miss rate over ten questions.
4. **Image-only page.** Observable: the card says "Answered from what is visible".
5. **Send guard.** With "confirm before sending" on and a cloud model, the preview shows the
   typed question and the page text; Esc sends nothing.

## Slices

1. **Pure core.** Chunker, ranker, `finalize` reuse, tests.
2. **The action.** Catalogue entry, schema, inputs, cards, palette and router rules. Done when
   Manual checks 1, 2, 4 and 5 pass.
3. **Long pages.** Wire the budget and ranker, Manual check 3.

No new crate or `windows` feature. THEORY (unverified): about 7 KB.

## Open questions for the owner

1. **Does a single typed question with one answer card break the no-chat rule?** This spec says no: it is one line in, one card out, no follow-up, nothing remembered, the same as #92. If you read the rule as "no typing a question at all", this action should not ship and #116 should be closed or changed.
2. Is an explicit palette row right, or should typed text that matches no action become the question for this action when it is the only text action?
3. A double press on this action: open the palette with nothing run (recommended), or re-ask the last question?
4. With a selection and a readable page both present: use the page and pass the selection as a hint, or use only the selection?
5. Default page budget 24,000 characters, with local ranking over it: right, or send up to the 60,000 cap and skip ranking?
6. Should the card show "You asked: ..." above the answer, or the answer alone?
