# Ask my documents

Point Wingman at a folder of notes and ask it a question. It finds the passages
that match, asks your model to answer from them only, and shows one card with
the answer and the file each quote came from. One question, one card, no
follow-ups.

This page describes what works today. Design and later plans are in the
[knowledge and memory spec](superpowers/specs/2026-10-16-knowledge-and-memory-design.md).

## Turn it on

Add one setting to `config.toml` (in `%APPDATA%\Wingman`, or the Wingman folder
in portable mode):

```toml
[knowledge]
folder = 'C:\Notes'
```

- It must be a full path to a folder, not a drive such as `C:\`. A relative
  path is refused with a card, and nothing is indexed.
- Wingman never creates the folder. Leave the setting empty (the default) and
  Wingman reads no folder and keeps no text from one.
- There is no Settings page for this yet, so edit the file and restart Wingman.

## What it reads

- `.txt` and `.md` files (also `.text` and `.markdown`), in the folder and its
  subfolders. Markdown headings are kept so an answer can name the section.
- `.html` and `.htm` files: only the text you would see. Scripts, styles,
  comments and tags are dropped, character codes such as `&amp;` are decoded,
  and nothing is loaded from the network.
- `.csv` and `.tsv` files: the column names on one line, then one line per row
  as `Name: Jane; City: Lisbon`. Empty cells are left out, and a row with more
  cells than the first line gets "column 3" style names. Only the first 256
  cells of a row are kept.
- `.json` files: one line per value, as `address.city: Paris` or `skills[0]:
  rust`. Empty values and `null` are left out. A file that is not valid JSON,
  or is over 1 MB, is read as plain text instead.
- A heading line is never made from a cell, a key or a web page line: a line
  that would start with `#` gets a backslash. (A `.txt` file is kept as it is,
  so a `#` line in one still reads as a heading.)
- Other file types are ignored for now: PDF, Word, images and the rest are not
  read.
- Left out on purpose: folders whose name starts with a dot (such as
  `.obsidian`), shortcuts and links to other folders, files named `~$...` and
  `.tmp` files.
- Limits: a file over 25 MB is skipped, and only the first 2 MB of a file's text
  is kept. At most 20,000 files are indexed. A file that does not look like
  text is skipped. Folders nested more than 32 levels deep are not read.
- Cloud-only files that are not downloaded to this PC (for example OneDrive
  "online only") are not fetched.

## When it reads

When Wingman starts, on a short background task, and then whenever a file in
the folder changes. Files that have not changed since the last start are not read again, and
only the passages of an edited file that changed are redone. A deleted file is
dropped from the index. If you change the folder setting or clear it, the saved
text of the old folder is deleted at the next start.

A new, edited, renamed or deleted file is picked up within about 2 seconds. If
the folder itself is deleted or renamed while Wingman runs, it stops watching,
shows a note, and keeps what was already indexed. Choose Reload settings once
the folder is back. Nothing runs while the folder is quiet: the watch waits for
Windows to report a change, with no timer.

If some files could not be read, the next answer card says how many. A file that
was open in another program is tried again a few times, then at the next
change or start.

## Ask a question

1. Open Quick Ask from the tray: **Actions**, then **Quick Ask**.
2. Type your question, for example `what was my start date at Acme`.
3. A question does not match an action name, so the list shows "No action
   matches. Press Down to ask your documents with this line." Press **Down** to
   pick **Ask my documents**, then **Enter**.

The card shows a short answer, then **From your documents** with one line per
quote: the file (and the heading, if the file has one) and the exact words the
answer relied on. Wingman checks that every quote really appears in the passage
it came from, and drops any that does not. It also checks the answer. Every
number in it must be a number in a quote: digits must match whole digits (or the
word for them), and number words (English, German, French and Spanish, such as
"three" or "vingt et un") must match a number the quote states, read one
sentence at a time. A German compound such as "einundzwanzig" is accepted only
when a quote has that same word. An answer or quote that holds a redaction
marker such as [email 1] is never shown. Wingman adds the file names itself; the
model never supplies one.

If no quote holds up, the card says **Your documents had no supported answer**
and names the closest files, up to three. That is the answer to a question your
documents do not cover. Wingman does not guess.

Other cards you may see: no folder is set; the folder has no indexed text yet
(add `.txt` or `.md` files and they are picked up shortly); nothing in your documents matches the
question.

## What leaves your computer, and when

Reading and indexing make no network request. Your files stay on this PC, in
Wingman's local store, which only your Windows account can open.

A passage leaves the computer in one case: you ask a question. Wingman sends up
to eight matching passages (about 2,000 tokens of them) with your question to
the provider that answers, marked as untrusted data. No screenshot is sent for
this action. Where it goes depends on your model: with Ollama on this computer
it stays here.

- **Offline** blocks it like every cloud request. **Local** sends it only to a
  local model. With a local model, Offline works.
- **Pause** stops it, as it stops every request.
- The egress log records the request with one label per passage, such as
  `resume.txt#chunk 12:ab12cd34` (file, chunk number, start of the chunk's
  hash), never the text. The send preview, when you turn it on, shows the same
  labels.

The full statement is in [PRIVACY.md](../PRIVACY.md#your-documents-folder).

## Masking

Sensitive numbers are removed twice.

- **When indexing:** card numbers, IBANs and national id numbers are replaced
  with `[card number]`, `[IBAN]` and `[national id]` before the text is saved,
  so the store never holds them. The file itself is not changed.
- **When asking:** email addresses, card numbers, IBANs and key-like tokens in
  the passages are replaced with markers before they are sent, whatever your
  screenshot redaction setting is.

A masked number cannot be found or quoted: ask for it and the answer says the
documents have none.

Not masked: a password or other secret written as ordinary words next to a label,
such as `password: hunter2` or `"pin": "1234"` in a `.json` or `.csv` file. If an
excerpt holding one is chosen for a question, it is sent. Keep files like that
out of the documents folder (issue #1112).

## Limits worth knowing

- Text files only. Changes are picked up while Wingman runs.
- The answer is one or two sentences, up to 700 characters, with at most five
  quotes shown.
- Passages are found by the words in your question. A question that shares no
  words with the document (asking "employer" when the file says "company") may
  find nothing.
- There is no chat: to ask something else, ask again.
