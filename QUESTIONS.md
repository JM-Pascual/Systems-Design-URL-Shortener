# Questions

- Can we directly encode with b62 a URL as it comes?
- What's the issue in part 1 with in-memory storage and potential clustering / multiple instance spawning? If we scale our service into multiple instances, each one will have a different storage access layer.
- Why should we use 307 or 302 instead of 301?
- `code TEXT PRIMARY KEY` versus a separate `id SERIAL PRIMARY KEY` with a `UNIQUE` index on `code` — what does the second buy you, and is it worth the extra index?
- Postgres's `nextval()` is non-transactional, so a rolled-back transaction never reclaims the sequence value it consumed. Why is that the right trade-off for concurrency, and why doesn't the resulting gap in codes threaten correctness?
- `resolve`'s lease is released with a blind `DEL lease:{code}` — it never checks that the key still holds the value *this* worker wrote. If a slow leader finishes after its lease has already expired and been re-acquired by a second worker, the slow leader's `DEL` deletes the second worker's active lease instead of its own. Why is that harmless in this specific design, and what would a fencing token (a unique value per lease holder, deleted only via a conditional check) close that this version leaves open?
