# Interoperability and runtime evidence

Protocol conformance and the independent Python peer now live in the
[HACP repository](https://github.com/manvendersingh21/hacp). There is no second
maintained copy of that peer here.

To run its interoperability test from this directory's compatibility script:

```sh
HACP_CHECKOUT=../hacp interop/run-interop.sh
```

Run from the HIVE root and provide your actual checkout location. The wrapper
tests that checkout; it does not silently clone, change revisions, or claim to
test HIVE's pinned revision. Run the full protocol suite in the HACP repository.

`live/` retains historical HIVE live-agent drivers, reports, and transcripts.
The driver requires a `--schemas` path pointing into a standalone HACP checkout.
It is a diagnostic, not the current durable orchestration entry point. For new
work use [hive collab](../docs/DISTRIBUTED-COLLABORATION.md). Running live agents
requires authorization and may consume provider credits.
