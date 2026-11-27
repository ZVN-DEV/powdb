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

## Ext4 library run, same day

`read_pages_verified` is opt-in. Point reads still use `DiskManager::read_page`. Evidence: `docs/benchmarks/2026-11-26-library-batch.json`, on ext4 `nvme0n1`, commit `298ea05` on `bench/cold-page-reads`.

| Id | Result | 9.2 |
| --- | --- | --- |
| R1 | Depth 1 median 137.6 ms. Depth 16 median 16.3 ms (8.45×). Device bytes 4.9% to 6.4% over 16 MiB. Fingerprints matched. | Met |
| F1 | Serial reread 4.7 ms, one pass 1.1 ms (23% of the reread). Both moved 192,512 device bytes. | Met |
| S1 | Cold 26.8 ms, warm 21.2 ms, ratio 1.26. Device bytes equal the 64 MiB file. Warm is under 21.4 ms. | Not met. The ratio is just over 1.25. Do not retune for that gap. |
| I1 | Every timed call matched. A corrupt page is refused in `batch_refuses_corrupt_or_missing_pages`. | Holds on this run |
| P1 | Batch depth is chosen by the caller. `read_page` is unchanged. | Holds |

The assigned blind grade was 7.5 until every bar held on one release run. That run is below.

## Readonly unique range, 2026-11-27

Evidence: `docs/benchmarks/2026-11-27-cold-query.json`, ext4 on `nvme0n1`, commit `a61bdae` on `bench/cold-page-reads`. The release binary ran `Gather filter .rk >= 0` through `Engine::execute_powql_readonly` after `posix_fadvise(DONTNEED)` on the heap. Keys were inserted shuffled and had to come back as `0..8191`, which is index order. The heap file is 33,558,528 bytes (8,193 pages). Each cold sample moved exactly 33,554,432 bytes in `/proc/self/io` and on `nvme0n1`: the 8,192 data pages, once. `nvme1n1` stayed at 0. The warm sample moved 0 bytes.

| Sample | Cold | Time | Process bytes |
| --- | --- | --- | --- |
| 0 | yes | 40.651 ms | 33,554,432 |
| 1 | yes | 40.109 ms | 33,554,432 |
| 2 | yes | 45.503 ms | 33,554,432 |
| 3 | yes | 43.656 ms | 33,554,432 |
| 4 | yes | 37.115 ms | 33,554,432 |
| 5 | no | 18.955 ms | 0 |

Cold median is 40.651 ms. The same machine's library depth-1 random read was 137.6 ms for half this many pages, so a one-pread-per-page walk of this heap would sit near 275 ms. 40.7 ms is the batched path, plus planning and row decode. This shape is every data page in key order, not the R1 sample of 4,096 pages drawn from a larger file, so it does not replace the R1 or S1 rows above. Warm at 0 bytes checks the eviction method. The fixture directory was removed.

Known multi-rid fetches now call `Table::get_many` at depth 16 in chunks of 4,096. One-row chunks stay on `Table::get`. That covers read-only and mutable index lookups, unique range scans, non-unique range scans on both executors, expression-index full rows, residual rows that already passed the projected check, nested index probes, and update/delete discovery. `try_bounded_index_page` is unchanged. Equivalence tests passed (13 and 4). Clippy `-D warnings` passed.

## Ext4 library run after contiguous reads, 2026-11-27

Evidence: `docs/benchmarks/2026-11-27-library-coalesce.json`, ext4 on `nvme0n1`, commit `763c4ba` on `bench/cold-page-reads`. Same probe shape as the earlier library run: 16,384 rows, 64 MiB heap, 7 samples, `posix_fadvise(DONTNEED)`, no `drop_caches`, no `O_DIRECT`. The fixture directory was removed. Every timed call matched its fingerprint. Warm samples moved 0 bytes. Cold sequential samples moved exactly 67,112,960 bytes, the file size.

Depth 1 in this probe is still one `pread` per page (`DiskManager::read_page`). Depths 4, 16, and 32 go through `read_pages_verified`, which now reads a contiguous run in one `pread` and then checks each page. `read_page` itself is unchanged. Storage library tests: 276 passed, 1 ignored.

| Id | Result | 9.2 |
| --- | --- | --- |
| R1 | Depth 1 median 137.5 ms. Depth 16 median 15.9 ms (8.67×). Device bytes 4.5% to 5.8% over 16 MiB. | Met |
| S1 | Cold 22.2 ms, warm 19.7 ms, ratio 1.13. Warm is under 21.4 ms. Device bytes equal the file. | Met |
| F1 | Serial reread 5.0 ms, one pass 1.0 ms (20% of the reread). Both moved 192,512 process bytes. | Met |
| I1 | Every timed call matched. A corrupt page is refused in `batch_refuses_corrupt_or_missing_pages`. | Met |
| P1 | `read_page` is still one pread. The storage library suite passed. | Met |

That S1 mark is the per-page path. The next section times depth 1 through `read_pages_verified` and replaces this score. The assigned grade does not move on this sample.

The earlier ext4 sample missed S1 at ratio 1.26 on the same depth-1 path. This sample of that path is 1.13. Sequential depth 16, which does coalesce, was 21.2 ms, close to depth 1 at 22.2 ms. Kernel readahead already covers sequential 4 KiB reads, so the ratio change is this run's cold and warm pair, not a change in the depth-1 code.

## Ext4 library run, sequential depth 1 coalesced

Evidence: `docs/benchmarks/2026-11-26-library-seq-depth1.json`, ext4 on `nvme0n1`, commit `b6a402c` on `bench/cold-page-reads`. Same fixture shape: 16,384 rows, 64 MiB heap, 7 samples, `posix_fadvise(DONTNEED)`, no `drop_caches`, no `O_DIRECT`. `sequential_distinct` at every depth, including depth 1, calls `read_pages_verified`. On this file that depth-1 call is one `pread` of all 67,112,960 bytes, then a CRC of every page. Random depth 1 and both fanout cases stay on `read_page`, so R1's baseline and F1's reread are still one `pread` per request. The fixture directory was removed. Every timed call matched its fingerprint. Warm samples moved 0 bytes. Cold sequential samples moved exactly 67,112,960 bytes on the process counter and on `nvme0n1`. `nvme1n1` stayed at 0.

| Id | Result | 9.2 |
| --- | --- | --- |
| R1 | Depth 1 median 132.8 ms. Depth 16 median 15.6 ms (8.53×). Device bytes 5.0% to 5.9% over 16 MiB. | Met |
| S1 | Cold depth 1 median 28.4 ms, warm 19.2 ms, ratio 1.48. Warm is under 21.4 ms. Device bytes equal the file. Depth 16 on the same reader was 22.2 ms. | Not met |
| F1 | Serial reread 5.0 ms, one pass 1.0 ms (21% of the reread). Process bytes 192,512 on both. | Met |
| I1 | Every timed call matched. Storage code did not change in this commit, so the earlier corrupt-page refusal still stands. | Met |
| P1 | `read_page` is still one pread. Storage tests were not re-run for a probe-only change. | Met |

The assigned grade stays 7.5. One `pread` of the whole file returns only after the device has finished, and the CRC starts after that, so the two do not overlap. Do not retune 1.25.

## Ext4 library run, 128 KiB windows

Evidence: `docs/benchmarks/2026-11-26-library-window128.json`, ext4 on `nvme0n1`, commit `5524ca5`. Same probe as the section above. A contiguous run is now read 32 pages at a time (128 KiB, this disk's `max_sectors_kb`) and each window is checked before the next read. The fixture directory was removed. Every timed call matched. Warm samples moved 0 bytes. Cold sequential samples moved exactly 67,112,960 bytes.

| Id | Result | 9.2 |
| --- | --- | --- |
| R1 | Depth 1 median 133.1 ms. Depth 16 median 15.7 ms (8.47×). Device bytes 4.7% to 5.6% over 16 MiB. | Met |
| S1 | Cold depth 1 median 32.4 ms, warm 21.9 ms, ratio 1.48. Warm is over 21.4 ms. Six of the seven warm samples are above that cap. Device bytes equal the file. | Not met |
| F1 | Serial reread 5.2 ms, one pass 1.1 ms (22% of the reread). Process bytes 192,512 on both. | Met |
| I1 | Every timed call matched. `batch_long_contiguous_run_matches_serial_reads` passed, and a corrupt page is still refused. | Met |
| P1 | `read_page` is still one pread. Storage library tests: 277 passed, 1 ignored. Clippy `-D warnings` passed. | Met |

The assigned grade stays 7.5. The checksum of 128 KiB is shorter than one device round trip, so the next read does not stay ahead, and the extra reads slow the warm pass past its cap. Do not try another window of that size. Do not retune 1.25.

## Ext4 library run, 8 MiB prefetch

Evidence: `docs/benchmarks/2026-11-26-library-prefetch.json`, ext4 on `nvme0n1`, commit `a3a7aa7`. A run longer than 8 MiB is read in 8 MiB windows. Before the checksum, the next window is hinted with `POSIX_FADV_WILLNEED`. The fixture directory was removed. Every timed call matched. Warm samples moved 0 bytes. Cold sequential samples moved exactly 67,112,960 bytes, so the hint did not read the file twice.

| Id | Result | 9.2 |
| --- | --- | --- |
| R1 | Depth 1 median 133.4 ms. Depth 16 median 15.8 ms (8.46×). Device bytes 5.0% to 5.6% over 16 MiB. | Met |
| S1 | Cold depth 1 median 27.6 ms, warm 21.9 ms, ratio 1.26. Every warm sample is above 21.4 ms (21.8 ms to 22.1 ms). Device bytes equal the file. | Not met |
| F1 | Serial reread 4.5 ms, one pass 1.2 ms (26% of the reread). Process bytes 192,512 on both. | Met |
| I1 | Every timed call matched. The long-run, gapped, duplicate, and corrupt-page tests passed. | Met |
| P1 | `read_page` is still one pread. Storage library tests: 277 passed, 1 ignored. Clippy `-D warnings` passed. | Met |

The assigned grade stays 7.5. Cold is 0.8 ms faster than the single `pread` of the whole file, and warm is 2.7 ms slower, which is why the ratio fell from 1.48 to 1.26. The hint did not hide the device read behind the checksum. The window code is not kept. Do not retune 1.25. Do not try a third window size.

## Ext4 library run, whole-run hint

Evidence: `docs/benchmarks/2026-11-26-library-stream-hint.json`, ext4 on `nvme0n1`, commit `8956ef3`. A run longer than 128 KiB was hinted with `POSIX_FADV_SEQUENTIAL` and `POSIX_FADV_WILLNEED`, then read a page at a time. The fixture directory was removed. Every timed call matched. Warm samples moved 0 bytes. Cold sequential samples moved exactly 67,112,960 bytes.

| Id | Result | 9.2 |
| --- | --- | --- |
| R1 | Depth 1 median 133.0 ms. Depth 16 median 15.6 ms (8.52×). Device bytes 5.0% to 5.7% over 16 MiB. | Met |
| S1 | Cold depth 1 median 32.0 ms, warm 25.8 ms, ratio 1.24. Every warm sample is 25.4 ms to 26.3 ms, over 21.4 ms. Device bytes equal the file. | Not met |
| F1 | Serial reread 4.8 ms, one pass 1.0 ms (22% of the reread). Process bytes 258,048 on both, up from 192,512 before the sticky sequential advice. | Met on this run's own comparison |
| I1 | Every timed call matched. The streamed-run corrupt-page test passed. | Met |
| P1 | `read_page` is still one pread. Storage library tests: 277 passed, 1 ignored. Clippy `-D warnings` passed. | Met |

The assigned grade stays 7.5. The ratio is under 1.25 because warm got slower, not because cold got faster. Cold is 32.0 ms, worse than the single `pread` at 28.4 ms. `POSIX_FADV_SEQUENTIAL` also stayed set on the heap file and increased later readahead. That hint is not kept. Do not retune 1.25.

## Ext4 library run, page at a time

Evidence: `docs/benchmarks/2026-11-26-library-page-stream.json`, ext4 on `nvme0n1`, commit `5677f07`. A long run is `read_page` and a checksum per page, with no `fadvise`. The fixture directory was removed. Every timed call matched. Warm samples moved 0 bytes. Cold sequential samples moved exactly 67,112,960 bytes. Fanout process bytes are back to 192,512.

| Id | Result | 9.2 |
| --- | --- | --- |
| R1 | Depth 1 median 134.3 ms. Depth 16 median 15.6 ms (8.63×). Device bytes 5.2% to 6.1% over 16 MiB. Depth 1 process bytes are 17,854,464 again. | Met |
| S1 | Cold depth 1 median 32.9 ms, warm 24.9 ms, ratio 1.32. Every warm sample is 24.7 ms to 25.2 ms, over 21.4 ms. Device bytes equal the file. | Not met |
| F1 | Serial reread 4.6 ms, one pass 1.0 ms (22% of the reread). Process bytes 192,512 on both. | Met |
| I1 | Every timed call matched. | Met |
| P1 | `read_page` is still one pread. | Met |

The assigned grade stays 7.5. Reading one page at a time inside `read_pages_verified` is slower than one `pread` of the file (warm 19.2 ms, cold 28.4 ms). Do not retune 1.25.

## Rules for the push

- Side branch only. Do not merge to `main` and do not push unless Kirby asks.
- One heavy job at a time. Do not fill `/mnt/models`. A verification fixture stays at or under 512 MiB and is deleted before the next one.
- Do not retune a constant because a sample moved a few percent. The next change has to be able to move R1, F1, or I1 on the bars above.
- Queue depth in the bench thread pool is not a 9.2. The timed path has to be code in `powdb-storage` or `powdb-query`.
- `O_DIRECT` is not the default. It would throw away the warm hits that already cost 0 device bytes.
