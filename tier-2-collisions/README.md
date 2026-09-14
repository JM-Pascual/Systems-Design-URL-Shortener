# Tier 2 — Code Generation and Collisions

**Problem being solved:** Tier 1 hands out codes from a counter without ever
asking *why* — or whether a different scheme might be better. This tier makes
that a real design decision by comparing it against the obvious alternative:
deriving the code from the URL's content instead of from a counter.

---

## The two paths

### Path A — counter + base62 *(what Tier 1 already does)*

Take the next value of a monotonically increasing counter, encode it in
base62. This is the path the rest of the course carries forward.

- **Uniqueness is guaranteed by construction.** The counter never repeats, so
  two codes can never collide — there is nothing to detect or retry.
- **The code is independent of the URL's content.** That means a destination
  can be edited later (`PATCH /{code}`) without the code itself changing.
- **Codes are enumerable.** `/0`, `/1`, `/2`, ... walks the entire database.
  Fixed by applying a reversible permutation (coprime multiplication, an XOR
  mask, or a small Feistel network) to the counter value *before* encoding —
  it preserves uniqueness while destroying the ordering.
- **Requires a shared counter.** Every instance minting codes has to agree on
  "what's next," which is fine with one process (Tier 1) and becomes a
  genuine open problem once there are several.

### Path B — truncated hash of the URL

Hash the submitted URL (e.g. SHA-256), truncate the digest to the code
length, encode it.

- **No shared counter needed.** Any instance can compute a candidate code
  from the URL alone, with no coordination. (A shared store is still needed
  to check for collisions on write — but that's a lookup you need anyway to
  store the mapping.)
- **Non-guessable by default.** A hash's output reveals nothing about
  insertion order or count, unlike a raw sequential counter.
- **Real collisions occur**, and have to be handled explicitly: two different
  URLs can truncate to the same code. This is where the course's collision
  resolution material lives — chaining and open addressing, straight out of
  the hash table Tier 1 already introduced, except now it's a *design*
  decision instead of an implementation detail of `std::collections::HashMap`.
- **Editing a URL necessarily changes its code** — the code *is* a function
  of the content, so a `PATCH` that changes the destination can't preserve
  the code without breaking the scheme's whole premise.

---

## Collisions arrive sooner than intuition suggests

Tier 0 sized the key space at 7 base62 characters: `62^7 ≈ 3.5 × 10^12`
possible codes. It's tempting to assume collisions only become a problem once
that space is close to full. The birthday paradox says otherwise: with a
keyspace of size `d`, the probability of at least one collision passes 50%
once roughly `√d` codes have been minted — nowhere near `d` itself.

```
d      = 62^7                     ≈ 3.52 × 10^12
√d                                ≈ 1.88 × 10^6   (≈ 1.88 million codes)

At Tier 0's write volume (40/sec):
  1.88 × 10^6 codes / 40 per sec  ≈ 47,000 seconds ≈ 13 hours
```

Thirteen hours of normal traffic — not "after the space is completed" — is
enough for a 50/50 shot at a collision under Path B. That number is the
actual argument for taking collision handling seriously rather than treating
it as a rare edge case.

### Resolving a collision

When `hash(url)` truncated to 7 chars already maps to a *different* URL:

- **Chaining doesn't fit.** The textbook answer to a hash-table collision is
  "store both values in this bucket" — but a short code must resolve to
  exactly one URL. There's no bucket to chain into; the *code itself* is the
  identity the client is holding onto.
- **Open addressing does fit.** On a collision, mix in a salt and retry:
  `hash(url + salt)` for `salt = 0, 1, 2, ...` until an unused code is found.
  This is the same probing idea from Tier 1's hash-table primer, applied to
  the codespace instead of an in-memory bucket array.

### The salting tension

Salting is also required for a second reason that has nothing to do with
collisions between *different* URLs: without it, hashing the same URL twice
deterministically produces the same code, which breaks the property Tier 1
already established (`shorten` called twice on the same URL returns two
distinct codes). But that determinism was the one advantage Path B had over
Path A — free deduplication, no lookup required. Salt it away, and Path B
keeps all of its costs (collision handling, retries) while losing the one
benefit a counter-based scheme doesn't already have for free. Worth stating
plainly: **for this system, Path A is strictly simpler and has no downside
Path B doesn't also have** — Path B earns its keep specifically in a
multi-writer setting with no shared counter, a problem the class doesn't go
on to solve a different way.

---

## What breaks here

| Problem | Status |
|---|---|
| A single shared counter (Path A) is a bottleneck once there are multiple app servers. | Open problem |

---

## Discussion questions

1. Under Path B, how many retries (extra salted hash attempts) would you
   expect *on average* once the codespace is 50% full? 90% full?
2. Path A's permutation step (coprime multiplier / XOR mask / Feistel
   network) needs to be a *bijection* on the counter's range — why? What
   goes wrong if two different counter values permute to the same output?
3. `PATCH /{code}` is deferred to Tier 4. Walk through why it's trivial under
   Path A and effectively impossible under unsalted Path B.
4. If you salted Path B to satisfy "duplicate URLs get distinct codes," is
   there any remaining reason to prefer it over Path A for this system? Try
   to construct one.
5. Real-world example: Git commit hashes and CDN cache keys are both
   content-derived. Why does content-addressing make sense for those and not
   for this system's codes?

**Previous:** [Tier 1 — Naive Single-Server Solution](../tier-1-naive/README.md) ·
**Next:** [Tier 3 — Persistence](../tier-3-persistence/README.md)
