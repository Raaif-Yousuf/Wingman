# Wingman — positioning and launch

Written 2026-09-16 from two research passes (Hacker News, GitHub, Windows
enthusiast forums, tech press; Reddit itself was unreachable from the tools
available that day and should be re-checked by hand). Sources are cited
inline. This is the document to reread before writing the README or a launch
post.

## The one-line pitch

**It's the Copilot key. It's actually yours now.**

Press it, Wingman looks at your screen and proposes one thing: review this
email, fill this form, put this event on your calendar, check this answer,
explain this error. You confirm, it does it. Local models or your own API
key. Open source, one small exe, and it never presses Send.

## What we do differently from Copilot

The owner's three, sharpened, plus two the research adds:

| # | Wingman | Copilot on Windows |
|---|---|---|
| 1 | **Quiet.** A tray icon, ~2 MB, 0% CPU until you press the key. No taskbar button, no notifications, no sign-in nags | A taskbar surface, an account wall, a WebView2 app, periodic prompts |
| 2 | **Local and verifiable.** Offline mode refuses the network in code. An egress log shows every request and every byte. Screenshots are redacted before upload. The prompt is a text file you can read. Open source, so the claim can be checked | Copilot Vision sends the screen to Microsoft's servers. Recall shipped plaintext screenshots and had to be pulled twice before relaunching opt-in. "Trust us" from a vendor that lost the room |
| 3 | **Yours to extend.** An action is a TOML file: prompt, inputs, executor. Read it, edit it, share it, install one from the gallery | Closed |
| 4 | **Never the final button.** It fills, drafts and proposes. You press Send, Submit, Pay | Copilot Actions and every "agent" product sell autonomy, and autonomous irreversible actions are the single fastest trust-killer in the literature |
| 6 | **Not a chatbot.** One press, one action, one card, done. No conversation view, no follow-ups. If you want a chat, ChatGPT and Ollama already exist | Copilot is a chat pane first and everything else second |
| 5 | **Bring your own model, on the laptop you already have.** Claude, GPT, Gemini, any OpenAI-compatible endpoint, or Ollama. No Copilot+ PC, no NPU gate, no subscription | Recall and Click to Do require a 40+ TOPS NPU that tinkerers proved is unnecessary; the good models sit behind Copilot Pro |

Number 3 is what makes 1 and 2 credible. Anyone can open `actions.toml` and
see exactly what the key can and cannot do.

## The WinRAR / Greenshot / Notepad++ test

Each of those replaced something Windows ships and became a reflex. They
share six traits, and Wingman has to pass all six:

| trait | what it means for Wingman |
|---|---|
| **Sits at a reflex point** | Greenshot owns Print Screen; Notepad++ owns double-clicking a `.txt`. Wingman owns the Copilot key, a dead or resented key on every 2024+ laptop. Microsoft's own "remap it" setting (Release Preview, 2026-09-10) is the admission that it does nothing useful |
| **Wins in the first ten seconds** | The first press must produce a useful card with zero setup. That means a **no-model tier**: OCR to clipboard, region capture, calculator and unit conversion, profile form-fill by label, all with no key and no Ollama. Then a model is an upgrade, not a prerequisite |
| **Zero setup, portable, tiny** | One exe, portable mode, no account, no installer required. If Ollama is present, it is the default; if not, "paste a key" or "install Ollama" are both one card away |
| **Never makes you regret it** | Idle cost of zero. No telemetry. No auto-update. Settings and hotkeys survive updates (the PowerToys remap that silently reverted after a Windows update is the cautionary tale). Nothing phones home unless the egress log shows it |
| **Deterministic where it counts** | Greenshot never guesses. Wingman's executors never guess: UI Automation, not coordinates; preview equals execution; undo where possible |
| **Still there in ten years** | Monthly releases, answered issues, `PROMISES.md`: MIT forever, no paid tier for the app, no telemetry, no relicensing. Screenpipe lost its community the day it changed license and added pricing; Rewind's capture died the day Meta bought it |

## What people say they want (evidence, not guesses)

Ranked from the tools people are building and the threads they upvote:

1. Explain or summarize what is on screen (errors, articles, code). Universal.
2. Read and answer questions about on-screen text.
3. Fill out forms from context.
4. Draft or review email replies.
5. Put detected dates and events on the calendar.
6. Explain a stack trace or error dialog.
7. Check homework or verify an answer.
8. Voice question about the current window.
9. Ask about a screen region.
10. Automate repetitive clicks (RPA-style) from a hotkey.
11. Summarize a local PDF or long document.
12. Translate on-screen text.
13. Extract tables and structured data from a screenshot.
14. Search a local timeline ("what was I reading yesterday about X").
15. Read screen content aloud.

Items 1 to 9 have direct evidence; 10 to 15 are inferred from feature sets of
Recall, Screenpipe and Click to Do. Phase 2 covers 1, 3, 4, 5, 7 and 13;
the rest are filed.

The strongest signal of all: the hotkey-to-screen-to-local-model loop is
being reinvented right now by independent solo developers (Clicky Windows,
AI Cowork, neuronection's desktop-assistant, PyGPT's screenshot mode). Every
one of them stops at "answer" or "speak". None executes. That is the
whitespace.

## What makes people uninstall (and the rule each one becomes)

| trigger | Wingman's rule |
|---|---|
| A network call from a feature sold as local (Copilot Vision vs Recall) | Offline mode enforced at the socket; egress log; "show me what you're sending" |
| Artificial hardware or account gates | None. Any Windows 11 machine, no account, no NPU |
| Updates that silently reset customizations | A CI test that upgrades over a configured install and checks every setting and hotkey survived |
| Bait-and-switch monetization on an "open" tool | `PROMISES.md`, MIT, no app pricing tier, ever. Sustainability, if needed, comes from optional cloud-model referral or sponsorship, never from gating features |
| Autonomous irreversible actions, agents misreporting what they did | Never the final button; result cards derived from executor return values, not intent |
| Electron and WebView2 RAM bloat | Native card and palette; WebView2 only for the rarely opened settings window, destroyed on close; a CI gate on idle RSS |
| "Yet another local chat wrapper" | There is no chat. The top comment on r/ollama's biggest launch of the year asked why everyone builds the same thing; Wingman's answer is that it does not have a chat window at all |

## What Reddit actually says (read by hand, 2026-09-16)

Threads read through a logged-in browser after every automated route was
blocked. Scores are as of that day. Paraphrased, not quoted at length.

**The Copilot key is resented, not ignored.** r/Windows11, "Microsoft
releasing more remapping options for CoPilot button" (165 points, 45
comments): the top comment calls it a key nobody uses in place of a useful
button; the next says Microsoft forced OEMs to put it there. r/Windows11,
"Microsoft admits the Copilot key breaks certain workflows" (184 points).
NoCopilotKey, a tiny utility that turns it back into Right Ctrl, got 68 points
on its own. r/PowerToys has a steady trickle of "remap the Copilot key"
threads, several reporting it does not behave as a real modifier. **Implication:**
the key is a known pain with a known audience; "give it a job" lands.

**"An OS should be an OS. If someone wants AI they can install it."** That
line, at 86 points, is the second-highest comment on r/Windows11's biggest AI
thread of the year, "Microsoft is reevaluating its AI efforts on Windows 11"
(491 points, 140 comments). Third: make AI something people choose (52). One
dissenter at 3 points: Click to Do is somewhat useful. **Implication:** Wingman
is the AI you chose to install, behind a key you chose to give it. Say
exactly that.

**Recall failed on workflow, speed and battery, not only on privacy.**
r/Windows11, "Is there anyone here who uses Windows Recall?" (52 comments):
it never fit into my workflow (18); the service kept loading after being
turned off (9); slow to open and search; drained the battery; one genuine fan
who uses it to find references across email, chat and docs. **Implication:**
awareness must cost nothing idle, be searched from where you already are (a
one-line palette question, not a timeline app), and be measurably light on
battery.
The one fan's use case is exactly the context-chip design.

**r/privacy does not believe toggles.** "I've heard Copilot collects data
from your screen" (196 points, 95 comments): opt-out switches are called
placebo buttons; the common advice is debloat tools or Linux. **Implication:**
a switch is not enough. The egress log, the socket-level offline guard and
open source are the only forms of "off" that this audience accepts.

**Local-assistant fatigue is real.** r/ollama, "I built a full desktop AI
assistant that runs on Ollama" (313 points, 197 comments) is InnerZero: 30+
tools, memory, voice, offline Wikipedia, auto-installs Ollama. Its top comment
(36) asks why everyone builds a derivative of the same thing, no different
from Open WebUI, LM Studio, AnythingLLM or "the thousand vibe-coded harnesses
for Ollama". InnerZero is also not open source (a releases-only repo, no
license). **Implication:** never present Wingman as a chat app with a local
model. Present it as the key that does the thing. Owner decision the same
day: there is no chat window at all.

**Someone already built the exact loop, in Rust, and posted about it in that
thread.** A commenter (2 points) describes a one-line bar above the Start
button: "looking at my screen, write a reply to that email", with the reply
typed directly into the Outlook window, plus PowerPoint and Downloads-folder
actions, using Qwen3 30B and Qwen3-VL 4B. r/LocalLLaMA also has "Built a
Windows tray assistant to send screenshots/clipboard to local LLMs" (2 points,
a Windows tray tool for translating on-screen text). **Implication:** the
demand and the builders exist; none of them ship a product. Invite them.

**Competitor licenses:** Pluely (r/ollama, 160 points) is the open-source
Cluely alternative, an "invisible" overlay for meetings and interviews, GPL-3.0,
2,663 stars; a different product and a copyleft license. Clicky Windows is MIT,
212 stars, Python, answers and speaks but does not act.

**Launch etiquette on r/software.** "Petition to add an AI disclosure
requirement to 'I built a…' posts" (148 points, 51 comments): the community is
tired of undisclosed vibe-coded launches and wants to know whether the author
can program. "I just made the best screenshot tool of all time" (106 points,
71 comments): the first reply is "what does this do better than ShareX";
another asks how much of the code is AI-generated; another says they have no
idea how to install things from GitHub. **Implication:** the launch post
discloses how the code was written, opens with what Wingman does that Copilot
and PowerToys do not, avoids superlatives, and offers `winget install` and a
single signed exe, never "clone and build".

## The moment, and the final push

The attention does not come after the last issue closes. It comes from one
demonstrable thing on one day, and the day is chosen for us:

- Microsoft's Copilot-key remap setting reaches general availability in the
  weeks after 2026-09-10. Every outlet will run "how to remap the Copilot
  key". Wingman's story is the sequel: *instead of disabling it, give it a
  job.*
- Windows 10 support ended 2025-10-14; the consumer ESU bridge ends
  2026-10-13. A wave of reluctant Windows 11 upgraders is hunting for "make
  Windows 11 bearable" utilities right now, the same audience that adopted
  PowerToys, Everything and Flow Launcher.
- Ollama reports millions of active users and native Windows ARM64 builds in
  2026; "local-first, cloud optional" is a mainstream pitch, not a niche one.

**The push is v0.2: Phase 2 complete, nothing more.** The core loop with
five actions (Explain what's on screen, Review this email, Add to calendar,
Fill this form, Check my work), the no-model tier, Offline mode, a signed
MSIX, `winget install Wingman`, and a README whose first screenful is a
15-second GIF of the key being pressed on an email, a form and an event. Do
not wait for the settings window, connectors or awareness; they are the reasons
people stay, not the reasons they arrive.

### Launch sequence

1. README first: the GIF, a "why not Copilot / Recall" table (the one
   above), one download link, portable build.
2. `PRIVACY.md`, `PROMISES.md` and the offline switch live before launch day,
   written against the Recall failure.
3. Signed release built by GitHub Actions with published checksums; a path to
   reproducible builds stated.
4. Ten `good first issue` actions labelled before any public post.
5. winget manifest submitted the week of launch.
6. Show HN, early in the week, modest and technical: the UI Automation
   executor model and the offline guard are the interesting parts.
7. Same day, separately worded posts to r/LocalLLaMA, r/Windows11,
   r/opensource, r/software, author disclosed.
8. console.dev and two newsletters pitched the week before against their
   stated criteria.
9. Preview builds to Britec09, MDTechVideos and The PC Security Channel
   (the security angle fits a tool that sees your screen).
10. Everything lands inside 24 hours: GitHub Trending rewards a burst over a
    steady climb.

Claim the Microsoft Store listing early: the Store has a documented history
of paid clones of free open-source apps (ScreenToGif, OBS, Captura).

## Sources

Adoption histories: Notepad++ (Lifehacker 2014 poll, XDA), 7-Zip
(HostingAdvice), Greenshot (How-To Geek), Everything (HN threads
39025062, 22852878), Flow Launcher (GitHub, star-history), PowerToys (The
Register), WinUtil (christitus.com/winutil-in-2026). Channels: dev.to HN
launch guide, redship.io Reddit rules, Microsoft Learn winget repository
docs, console.dev selection criteria, OSSInsight on Trending. Trust: Ars
Technica and Computing.co.uk on Recall; The Register and TechRadar on
Copilot Vision; PowerToys issue 37971; Windows Latest 2026-07-24 on the
Copilot key "do nothing" switch; Neowin 2026-09 on the remap setting;
osohq.com on agent trust. Demand: github.com/Bitshank-2338/clicky-windows,
sami-fd.github.io/ai-cowork, github.com/neuronection/desktop-assistant,
pygpt.net, HN threads on Screenpipe (218 and 88 points) and its business
model. Timing: Microsoft Support on Windows 10 end of support;
angelo-lima.fr on Ollama 2026.
