# Thundering Herd Demo

A live, navigable dashboard comparing [Tier 4.1](../tier-4.1-caching/) (no
coordination on a cache miss) against [Tier 4.3](../tier-4.3-caching/) (a
note in the key) under an identical simulated stampede — Prometheus scraping both
apps' `/metrics`, rendered in a Grafana dashboard that's pre-provisioned, not
something you build by clicking around.

Result from an actual run (release builds, 100 ms delay): five bursts of
2,000 concurrent requests per tier, spread over 20 cold keys, produced
**9,983 Postgres queries on Tier 4.1** (about one per request) and **100 on
Tier 4.3** (one per key per burst — the lease held). That's the number to
point at; the graph just makes it visible over time.

---

## Why there's an artificial delay involved

A real thundering herd is a problem because the query being rebuilt is slow
enough for concurrent misses to pile up while it runs. Against a local,
warm Postgres, a real query returns in well under a millisecond — too fast
for any concurrency tool to reliably land many requests inside that window,
which makes the failure nearly impossible to reproduce honestly on a laptop.
`DEMO_QUERY_DELAY_MS` (read by both tiers' `Store`, off by default) inserts
a real `tokio::time::sleep` after the Postgres query on a miss, simulating
a realistically slow backend rather than faking the comparison some other
way. Both tiers get the same delay, so the comparison stays fair. Use 100 ms:
Tier 4.3's waiters give up after 200 ms and query Postgres, and at 150 ms a
2,000-request burst left too little margin (317 queries instead of 100).

## Running it

**1. Bring up each tier's own services** (Postgres and Redis, plus a
bucket for Tier 4.3's Bloom filter snapshots; separate stacks, already
part of each tier's folder):

```bash
docker compose -f ../tier-4.1-caching/docker-compose.yml up -d
docker compose -f ../tier-4.3-caching/docker-compose.yml up -d
```

**2. Run both apps side by side**, on different ports, with the delay on.
Use release builds: a debug build is slow enough under this load that Tier
4.3's waiters time out. Raise the open-files limit, since each request is a
socket:

```bash
# from the repo root
ulimit -n 65536

DATABASE_URL=postgres://shortener:shortener@localhost:5434/shortener \
REDIS_URL=redis://localhost:6380 \
PORT=3000 DEMO_QUERY_DELAY_MS=100 \
cargo run --release -p tier-4-1-caching &

DATABASE_URL=postgres://shortener:shortener@localhost:5436/shortener \
REDIS_URL=redis://localhost:6382 \
PORT=3002 DEMO_QUERY_DELAY_MS=100 \
cargo run --release -p tier-4-3-caching &
```

If linking fails on macOS with `tapi error: malformed file ... unknown
architecture`, the SDK is newer than the Command Line Tools' linker. Point
the build at an older SDK, e.g.
`export SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk`.

**3. Bring up Prometheus + Grafana** (this folder):

```bash
docker compose up -d
```

On Linux, Prometheus reaches the host apps via `host.docker.internal`,
wired up in `docker-compose.yml` via `extra_hosts: host-gateway` — this is
automatic on Docker Desktop but needs that explicit entry on Linux Docker
Engine.

**4. Open the dashboard:** [http://localhost:3001/d/thundering-herd](http://localhost:3001/d/thundering-herd)
(anonymous viewer access, no login). It's provisioned and ready the moment
Grafana starts — nothing to configure.

**5. Fire the stampede:**

```bash
./load-test.sh
```

Watch the dashboard while it runs. By default it fires 5 bursts, each one
2,000 concurrent requests per tier (`KEYS=20` codes × `BURST_SIZE=100`).
Tune it with `KEYS`, `BURST_SIZE` (at most 300, curl's per-process limit),
`BURST_COUNT` and `BURST_INTERVAL_SECONDS`. Running it again mints fresh
codes; the counters only reset when the apps restart.

## A note on load-generation tools

`ab` (Apache Bench) gave unreliable results for this specific comparison —
it consistently under-counted concurrent misses on this particular
redirect-returning endpoint, for reasons not fully tracked down. A plain
bash loop of backgrounded `curl`s was verified at 50 concurrent requests.
`load-test.sh` now runs one `curl --parallel` process per key instead, so
2,000 requests don't need 2,000 processes. Worth knowing if you extend this
demo with a different tool.

## Cleaning up

```bash
docker compose down -v
docker compose -f ../tier-4.1-caching/docker-compose.yml down -v
docker compose -f ../tier-4.3-caching/docker-compose.yml down -v
```

Also stop the two `cargo run` processes (`kill %1 %2`, or `lsof -ti :3000
-ti :3002 | xargs kill`).
