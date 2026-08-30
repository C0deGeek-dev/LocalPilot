# Research and investigation practices reference

Long-form companion to `research-lessons.json`. Ingest it (`localpilot ingest
run`) to make it reachable when the model calls `knowledge_search`. Headings are
phrased the way a model would query them.

## How to investigate methodically

Treat an investigation as a sequence of discriminating tests, not a pile of
observations. State a falsifiable hypothesis, then run the single test that most
cheaply tells it apart from the alternatives. Hold two or three hypotheses at
once rather than anchoring on the first plausible one, and abandon a hypothesis
the moment a clean test refutes it — even one you favoured. When elimination
narrows the problem, test the common factor directly: if A and B both fail and
share C, test C in isolation before blaming either. To localize a regression
between a known-good and known-bad state, bisect — halve the search space
repeatedly instead of guessing.

## How to treat evidence

Separate what you verified from what you assumed, and tag each claim with its
evidence; an unverified claim is a question, not a conclusion. Record the exact
command, environment, and raw numbers behind every measured result so it can be
reproduced. A null or negative result is a result: record it as observed rather
than re-running until it turns favourable or quietly dropping it. Beware
confirmation bias in tooling — a flag, a warning, or a search hit can mislead, so
confirm the mechanism behind a signal, not just the correlated signal.

## How to use sources and scope a question

Prefer primary and official sources when a fact matters: the spec, the API
documentation, the source code, the changelog — and note the date, because facts
expire. Scope the question before researching it; an underspecified question
yields an unfocused answer, so pin the constraints (version, platform, budget,
goal) first. Re-validate carried-forward findings against the current state
before acting, because a known issue may already be fixed.

## How to finish

Adversarially verify your own conclusion: try to refute it with one more test
before committing to it. The cheapest skeptic is you, before you ship the claim.
