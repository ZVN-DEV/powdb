# Blind grade card: 2026-11-26

Kirby set the current blind grade at **7.5** on every metric below. That number is the assigned baseline. It is not recomputed from the story of the code. A later run may move a metric only when a release binary produces a new verified sample on that metric. The aim is **9.2 on every metric at once**, not an average.

The 26 November cold run is the evidence under the 7.5. It is buffered `pread` on a 64 MiB Btrfs fixture with compression turned off for that directory. Warm samples moved 0 device bytes. Cold sequential samples moved the whole file. Random queue depth 1 took 174.7 ms for 4,096 pages. Depth 32 took 9.8 ms in a bench-local thread pool, which is not an engine path.

## Metrics

| Id | What 7.5 is | 9.2 bar | Fail closed |
| --- | --- | --- | --- |
| R1 Random cold | 4,096 distinct pages, one `pread` at a time, median 174.7 ms, about 18 MB on the device | Same shape, library reader, depth 16: median ≤ 20 ms and at least 4× faster than depth 1 in that same run. Device bytes within 15% of 16 MiB | Bytes far above one pass, or a faster number with a wrong fingerprint |
| S1 Sequential cold | 64 MiB file, depth 1, median 21.1 ms, device bytes exactly 67,112,960. Warm checksum median 19.4 ms | Depth 1 cold median within 25% of that run’s own warm median. Warm median not worse than 21.4 ms on this class of machine | Device bytes not equal to the file, within one page |
| F1 Fanout | 4,096 rereads of 16 pages: 5.3 ms, 258,048 process bytes. One pass of those pages: 1.2 ms, same process bytes | Same run: batch or packet median ≤ 40% of the serial reread, and device bytes no higher than the serial reread | A latency win that multiplies device bytes |
| I1 Integrity | Every timed call in the 26 November JSON matched its page fingerprint. CRC checked on the way in | Stays perfect. A flipped byte in a fetched page must be refused. Order, duplicates, and missing slots match | Any mismatched value. Speed does not offset this |
| P1 Single page | `HeapFile::get` and `DiskManager::read_page` are one synchronous `pread` | Default one-page reads stay on that path. Existing storage tests pass | A batch scheduler that makes a point read wait on a thread pool |

## Rules for the push

- Side branch only. Do not merge to `main` and do not push unless Kirby asks.
- One heavy job at a time. Do not fill `/mnt/models`. A verification fixture stays at or under 512 MiB and is deleted before the next one.
- Do not retune a constant because a sample moved a few percent. The next change has to be able to move R1, F1, or I1 on the bars above.
- Queue depth in the bench thread pool is not a 9.2. The timed path has to be code in `powdb-storage` or `powdb-query`.
- `O_DIRECT` is not the default. It would throw away the warm hits that already cost 0 device bytes.
