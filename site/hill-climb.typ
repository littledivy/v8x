#import "./shim/html.typ": *

#set document(
  title: "hill climb — v8x",
  description: "How v8x measures progress: run V8's own test suites against each backend, ratchet the passing set so it only grows.",
)

#show: html-shim

= The hill climb

How do you know a V8 compatibility layer is actually compatible? Run V8's
own tests against it. `v8x` tracks two suites per backend — six cells total
— on the #link("status/")[public dashboard]:

#table(
  columns: 2,
  [*suite*], [*what runs*],
  [`rusty_v8`], [the vendored rusty_v8 test files, unmodified, against the shim],
  [`deno_core`], [`cargo nextest run -p deno_core` in a patched Deno checkout],
)

The test files are never edited — they run as-is. When a test fails, the
*backend* gets fixed.

== The ratchet

Every cell has a baseline: the list of tests known to pass, checked into
`tests/status/baselines/<backend>/<suite>.txt`. CI is red if a baselined
test regresses *or* if new tests pass that aren't recorded yet. So every
improvement must land together with its updated baseline — progress is
locked in, and the passing count only moves up.

```sh
node tests/harness/run.mjs rusty_v8 quickjs            # run a cell
node tests/harness/run.mjs rusty_v8 quickjs --update   # record new passes
node tests/harness/run.mjs rusty_v8 quickjs --check    # verify the ratchet
```

#note[
  A test file that fails to *link* (an undefined `v8__*` symbol) scores zero
  for the whole file — so the biggest files are the biggest payoffs, and
  even stubbing a symbol moves the number.
]

== Contributing

Fixes follow one loop: make a test pass in `src/jsc/` or `src/quickjs/`,
re-run the cell with `--update`, commit the fix and the baseline together,
open a PR. CI re-checks the cell; green means mergeable. Details in the
repo's #link("https://github.com/littledivy/v8x/blob/main/CLAUDE.md")[CLAUDE.md]
— the playbook both humans and agents follow.
