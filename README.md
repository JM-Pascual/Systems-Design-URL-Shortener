# URL Shortener — A Systems Design Class

This course builds one system — a URL shortener — **eight times over**. Each
version is broken in a specific, demonstrable way, and the next one exists to
fix it.

We teach it this way because the alternative does not stick. A course that
presents hash tables, then caching, then sharding as a sequence of topics leaves
students able to define each one and unable to say when to reach for it. Here,
nothing is introduced until the previous version of the system has visibly
failed without it — in a load test, a bug demo, or a question nobody in the room
can answer.

So the rule for the whole repository is: **no solution before the problem has
been felt.**

**Language:** Rust (edition 2024, toolchain pinned in `rust-toolchain.toml`).
**Infrastructure:** Docker Compose, from Tier 3 onward.

---

## The course, tier by tier

Read the right-hand column as the syllabus: it is the reason each session
exists.

| Tier | Adds | Because the previous tier… |
|---|---|---|
| **0** — [Requirements](tier-0-requirements/) | A written spec. No code. | — |
| **1** — [Naive](tier-1-naive/) | In-memory `HashMap` + base62 counter, over HTTP. | — (baseline) |
| **2** — Code generation | Counter+base62 (Path A) vs. hash-of-URL (Path B); collisions, birthday paradox, enumerability. | …never justified *why* codes come from a counter. |
| **3** — Persistence | Postgres. B-tree index vs. hash map. | …loses every link on restart. |
| **4** — Caching ⭐ | Redis cache-aside, TTLs, eviction, invalidation, **thundering herd**. | …now pays a disk round-trip on all 99% of traffic that is reads. |
| **5** — Distributed IDs | Redis `INCR`, ticket server, Snowflake. | …has one counter in one process, so two app servers collide. |
| **6** — Availability | Replication, consistent hashing, sharding. | …has one Redis and one Postgres — both single points of failure. |
| **7** — Analytics *(optional)* | Async click pipeline, rate limiting, Bloom filters. | …logs clicks inside the redirect latency budget. |

⭐ Tier 4 is the centerpiece.

---

## Repository layout

```
Cargo.toml              workspace root — lists every tier as a member
rust-toolchain.toml     pins the compiler so everyone builds identically
tier-0-requirements/    README only
tier-N-.../
  README.md             problem, what changed, trade-offs, discussion questions
  src/                  self-contained implementation
  demo/  | loadtest/    scripts that reproduce the failure and verify the fix
```

**Tiers do not depend on each other.** Each is a full copy of the previous one
plus the new idea. That duplication is deliberate: a student reads *one* folder
top to bottom, and an instructor can run `diff -ru tier-3-persistence
tier-4-caching` to show precisely what a tier introduced.

---

## Getting started

```bash
# One-time: install the Rust toolchain (rustup reads rust-toolchain.toml).
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

git clone <this repo> && cd Systems-Design-URL-Shortener

cargo build                       # builds every tier
cargo test  -p tier-1-naive       # run one tier's tests
cargo run   -p tier-1-naive       # start one tier's server on :3000
cargo clippy --workspace          # lints
cargo fmt --all                   # format
```

From Tier 3 onward each tier ships a `docker-compose.yml` for its Postgres and
Redis; `docker compose up -d` inside the tier folder is all that is needed.

---

## How a session runs

Each tier is roughly one session, and each session has the same three beats.

**1. Break the previous tier.** Start with the demo or the discussion questions
at the end of the previous README — a load test that falls over, a server
restart that loses every link, two app servers handing out the same code. The
failure has to be seen before it is explained.

**2. Work out the fix together.** The tier's own README has the argument: what
changed, what it costs, and which requirement from Tier 0 it is paying for. The
discussion questions are meant to be asked *before* the answer is on screen.

**3. Write the code.** Tiers ship as **skeletons** — types, signatures, tests
and step-by-step comments are given; the function bodies are `todo!()` and are
yours. `cargo test -p tier-N-...` is the progress bar: it starts red and you are
done when it is green.

Where a tier demonstrates a failure mode, its `demo/` folder holds a "before"
script that reproduces the failure and an "after" script that shows the fix, so
the same thing can be run live or worked through alone afterwards.

Two habits are worth carrying through all eight tiers. First: whenever a piece
of infrastructure appears, name the Tier 0 requirement it is paying for — if you
cannot, go back. Second: prefer the smallest version of a fix that makes the
demo pass, then ask what it costs. Most of the interesting material in this
course lives in that second question.

---

## Status

- [x] Tier 0 — requirements
- [x] Tier 1 — naive in-memory (skeleton + tests)
- [ ] Tier 2 — code generation and collisions
- [ ] Tier 3 — persistence
- [ ] Tier 4 — caching and the thundering herd
- [ ] Tier 5 — distributed ID generation
- [ ] Tier 6 — availability and partitioning
- [ ] Tier 7 — analytics and abuse prevention
