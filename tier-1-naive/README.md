# Tier 1 — Naive Single-Server Solution

**Problem being solved:** none yet. This tier is the *baseline* — the simplest
thing that satisfies Tier 0's two core requirements. Every later tier exists
because of something that is broken here.

---

## What it is

One process. One `HashMap<String, String>` in RAM. One `u64` counter.

```
POST /shorten  {"url": "..."}   ->  201 {"code": "0", "short_url": "..."}
GET  /{code}                    ->  307 Location: <long url>   (or 404)
GET  /stats                     ->  "N links"
```

```
                 ┌─────────────────────────────┐
   client ──────▶│  axum handler               │
                 │    ↓                        │
                 │  Mutex<Store>               │
                 │    ├── HashMap<code, url>   │
                 │    └── next_id: u64         │
                 └─────────────────────────────┘
                        one process, all RAM
```

## Run it

```bash
cargo run -p tier-1-naive

# in another shell
curl -X POST localhost:3000/shorten \
     -H 'content-type: application/json' \
     -d '{"url":"https://en.wikipedia.org/wiki/Hash_table"}'
# -> {"code":"0","short_url":"http://localhost:3000/0"}

curl -i localhost:3000/0
# -> HTTP/1.1 307 Temporary Redirect
#    location: https://en.wikipedia.org/wiki/Hash_table
```

## Your implementation tasks

Four `todo!()`s. Run `cargo test -p tier-1-naive` — everything fails until you
fill them in, and passes when you are done.

| File | Function | Teaches |
|---|---|---|
| `src/base62.rs` | `encode` | repeated division; `Vec<u8>` → `String` |
| `src/base62.rs` | `decode` | Horner's method; checked arithmetic; custom error types |
| `src/store.rs` | `shorten` | ownership: taking `String` by value to store it |
| `src/store.rs` | `resolve` | why you clone out of a `Mutex` instead of returning `&str` |

Each `todo!()` has a step-by-step comment above it, including the Rust-specific
notes. Read the comment, write the body, delete the `let _ = ...;` line above it.

---

## The two ideas in this tier

### 1. The hash table

`src/store.rs` opens with the full explanation: buckets, hash function, load
factor, resizing, and why "amortized O(1)" is the honest way to describe it.

The reason we belabour this at Tier 1 is that **Tier 4 replaces this exact
structure with Redis**. Redis is not a new concept — it is this hash map, moved
out of the process and onto the network so several app servers can share it.
If students carry that mental model forward, Tier 4 is easy.

### 2. base62 encoding — and why it is not a hash

`src/base62.rs` opens with the comparison table. The short version:

|  | `base62::encode` | `sha256(url)[..7]` |
|---|---|---|
| Reversible? | yes, it is a bijection | no, one-way |
| Collisions? | **impossible** | possible |
| Depends on the URL? | **no** — only on the counter | yes |

That last row is the one to underline. Because the code comes from a counter
and not from the URL's content, **you can change where a code points without
changing the code.** Tier 4's cache-invalidation and thundering-herd demos are
built entirely on that property. Tier 2 makes the argument properly.

---

## What is broken here (i.e. the rest of the class)

| Problem | Symptom you can demo | Fixed in |
|---|---|---|
| **Volatile** | `curl` a link, `Ctrl-C` the server, restart, `curl` again → 404 | Tier 3 — Postgres |
| **Single process** | Start two servers on ports 3000 and 3001. Both hand out code `"0"` for different URLs. | Tier 5 — distributed IDs |
| **Bounded by RAM** | Tier 0 estimated 50 GB/month of URLs. | Tier 6 — sharding |
| **One global mutex** | Every request, reads included, serialises behind one lock. | Tier 4 — shared cache |
| **Enumerable codes** | `curl localhost:3000/0`, `/1`, `/2`, … walks the entire database. | Tier 2 — permutation |

Try the first one now — it takes ten seconds and it is the motivation for the
next two tiers.

---

## Discussion questions

1. Rust's `HashMap` resizes when it is ~87.5% full, rehashing everything. If a
   redirect arrives *during* that resize, what is its latency? Is "amortized
   O(1)" still an honest thing to promise a user?
2. We return **307**, not **301**. Why does a permanent redirect make click
   analytics impossible? (And why would it break the `PATCH` endpoint we add
   in Tier 4?)
3. `shorten("https://x.com")` twice returns two different codes. Is that a bug?
   What would it cost to make it idempotent?
4. The counter is a `u64` and codes are base62. At 40 writes/sec (Tier 0), how
   long until codes are 8 characters long? Until the `u64` overflows?
5. Replace `Mutex<Store>` with `RwLock<Store>`. Given a 100:1 read/write ratio,
   how much throughput would you expect that to buy?

**Previous:** [Tier 0 — Requirements](../tier-0-requirements/README.md) ·
**Next:** Tier 2 — Code Generation and Collisions
