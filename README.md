# URL Shortener — A Systems Design Class

A teaching repository that builds one system — a URL shortener — **eight times**,
each version fixing a failure mode the previous one exhibits.

The rule for the whole repo: *no solution is introduced before the problem it
solves has been felt.* Every tier starts with a demo, a load test, or a
discussion question that makes the previous tier hurt.

**Language:** Rust (edition 2024, toolchain pinned in `rust-toolchain.toml`).
**Infrastructure:** Docker Compose, from Tier 3 onward.

---

## The narrative

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

⭐ Tier 4 is the centerpiece. Budget the most class time there.

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

### For students

The tiers ship as **skeletons**: types, signatures, tests, and detailed
step-by-step comments are provided; the function bodies are `todo!()` and are
yours to write. `cargo test -p tier-N-...` is your progress bar.

### For instructors

Each tier's README ends with discussion questions intended to be asked *before*
revealing that tier's solution. Where a tier demonstrates a failure mode, the
`demo/` folder contains a "before" script that reproduces it and an "after"
script that shows the fix.

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
