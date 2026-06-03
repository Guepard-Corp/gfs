/*
 * gfs — PoC Table Access Method for the GFS lazy clone (RFC 008).
 *
 * A custom routine over heap storage: a real relkind='r' table, full CRUD, plus
 * transparent COPY-ON-READ. The table behaves like an ordinary table, but rows
 * that exist in the remote source yet not in local storage are fetched on read,
 * served to the triggering query, and WRITTEN THROUGH into the local heap so
 * future reads are 100% local — "a replica, but lazy / copy-on-read".
 *
 *   - scan_begin/scan_end/scan_rescan : own GfsScanDesc (rs_base first) wrapping a
 *     real heap scan for local rows + per-scan federation state. At scan_begin we
 *     fetch the missing rows from gfs_remote.<t> (SPI), materialize them for this
 *     scan, and heap_insert them locally (write-through). The current command's
 *     snapshot does NOT see the written rows (same command id) — which is exactly
 *     why we also emit them synthetically from scan_getnextslot.
 *   - scan_getnextslot : serve local heap rows, then emit the fetched rows.
 *   - index_fetch_tuple : PK/index hook (delegates via the heap routine pointer).
 *   - index_build_range_scan : slot-API reimpl over a PRIVATE heap scan (a custom
 *     rd_tableam is rejected by legacy heap_getnext; fetched rows are real heap
 *     rows so the normal index path covers them).
 *   - index_validate_scan : stub (CONCURRENTLY out of PoC scope).
 *   - relation_needs_toast_table=false (PoC: avoids the TOAST index build path).
 * Everything else is heap's.
 *
 * PoC scope: federated tables have shape (id bigint, name text), key=id, and a
 * same-named source table gfs_remote.<t>; identifiers are used unquoted; no index
 * upkeep on write-through (fed/fed2 have no index); writes assume a writable
 * (non-standby) cluster. Federation is wired on the seq-scan path; other
 * scan-desc-consuming callbacks (tidrange/bitmap/sample) remain heap's and are
 * not exercised here. All of these are hardening items, not design blockers.
 */
#include "postgres.h"

#include "access/heapam.h"
#include "access/htup_details.h"
#include "access/table.h"
#include "access/tableam.h"
#include "access/xact.h"
#include "catalog/index.h"
#include "catalog/namespace.h"
#include "executor/executor.h"
#include "executor/spi.h"
#include "executor/tuptable.h"
#include "fmgr.h"
#include "lib/stringinfo.h"
#include "miscadmin.h"
#include "nodes/execnodes.h"
#include "utils/builtins.h"
#include "utils/lsyscache.h"
#include "utils/rel.h"
#include "utils/snapmgr.h"

PG_MODULE_MAGIC;
PG_FUNCTION_INFO_V1(gfs_handler);

static TableAmRoutine gfs_methods;
static const TableAmRoutine *gfs_heap = NULL;
static bool gfs_ready = false;

/*
 * Reentrancy guard: gfs_load_federation runs SPI whose NOT EXISTS subquery
 * scans this same gfs table. That nested scan must read LOCAL rows only and not
 * recurse into federation (else infinite recursion). Single-threaded backend →
 * a static bool is sufficient; reset via PG_FINALLY so an error can't wedge it.
 */
static bool gfs_in_federation = false;

/*
 * Per-scan descriptor. rs_base MUST be first so the executor can treat a
 * GfsScanDesc* as a TableScanDesc. We hold a pointer to a REAL heap scan (over
 * local rows) plus the rows fetched from the source for this scan. Allocated in
 * the same memory context as the desc (scan lifetime); reused across rescans.
 */
typedef struct GfsScanDescData
{
	TableScanDescData rs_base;		/* AM-independent part the executor sees */
	TableScanDesc	heap_scan;		/* underlying heap scan over local rows */
	bool			heap_done;		/* local heap exhausted? */
	int64			fed_n;			/* # rows fetched from the source */
	int64			fed_emitted;	/* # emitted so far in this scan */
	HeapTuple	   *fed_tuples;		/* the fetched rows (materialized) */
}			GfsScanDescData;
typedef struct GfsScanDescData *GfsScanDesc;

static bool
gfs_no_toast(Relation rel)
{
	return false;
}

/*
 * Copy-on-read, driven entirely by the extension's catalog (gfs.clone_source) —
 * the TAM hard-codes nothing about the source or the schema. For a registered
 * clone whose source still exists, we (1) READ the rows present in the source
 * but not yet local, (2) keep them to emit for THIS scan (the current command's
 * snapshot can't see freshly written rows — newer command id — so we emit), and
 * (3) WRITE THEM THROUGH with low-level heap_insert + manual index maintenance.
 *
 * Why heap_insert and not INSERT…SELECT: a faithful clone's tables carry the
 * source's FOREIGN KEYS / triggers. The source already enforced them; lazily
 * copying a child row whose parent isn't local yet must NOT re-fire the FK (it
 * would error). heap_insert bypasses triggers/RI (like the overlay warming did
 * under session_replication_role=replica) while we still maintain indexes so
 * lookups stay consistent. Unique/PK is still enforced by the index insert; the
 * NOT EXISTS guarantees no key conflict anyway.
 *
 * Future commands read the rows locally; the NOT EXISTS set shrinks to empty; if
 * the source is later dropped the to_regclass gate turns federation off and the
 * clone stands alone. Activity is recorded in gfs.clone_stats.
 */
static void
gfs_load_federation(GfsScanDesc gscan)
{
	Relation		rel = gscan->rs_base.rs_rd;
	Oid				relid = RelationGetRelid(rel);
	MemoryContext	oldcxt = CurrentMemoryContext;	/* the desc's context */
	char		   *source_ref = NULL;
	char		   *key_col = NULL;
	char			meta[200];

	/* Nested scan during our own SPI (the NOT EXISTS below): local rows only. */
	if (gfs_in_federation)
		return;
	/* Cheap gate: no gfs catalog schema → extension metadata absent. */
	if (!OidIsValid(get_namespace_oid("gfs", true)))
		return;

	/* Registered clone whose source still exists? (drop source → stand alone) */
	snprintf(meta, sizeof(meta),
			 "SELECT source_ref, key_col FROM gfs.clone_source "
			 "WHERE relid::oid = %u AND to_regclass(source_ref) IS NOT NULL",
			 relid);

	gfs_in_federation = true;
	PG_TRY();
	{
		if (SPI_connect() == SPI_OK_CONNECT)
		{
			if (SPI_execute(meta, true, 1) == SPI_OK_SELECT && SPI_processed == 1)
			{
				/* keep the strings past SPI_finish */
				MemoryContextSwitchTo(oldcxt);
				source_ref = SPI_getvalue(SPI_tuptable->vals[0],
										  SPI_tuptable->tupdesc, 1);
				key_col = SPI_getvalue(SPI_tuptable->vals[0],
									   SPI_tuptable->tupdesc, 2);
			}

			if (source_ref != NULL && key_col != NULL)
			{
				TupleDesc		td = RelationGetDescr(rel);
				StringInfoData	cols;
				StringInfoData	q;
				char		   *local_ref;
				const char	   *kq;
				int				a;

				/* column list = the table's own live columns, by name */
				initStringInfo(&cols);
				for (a = 0; a < td->natts; a++)
				{
					Form_pg_attribute att = TupleDescAttr(td, a);

					if (att->attisdropped)
						continue;
					if (cols.len > 0)
						appendStringInfoString(&cols, ", ");
					appendStringInfoString(&cols,
										   quote_identifier(NameStr(att->attname)));
				}
				local_ref = quote_qualified_identifier(
								get_namespace_name(RelationGetNamespace(rel)),
								RelationGetRelationName(rel));
				kq = quote_identifier(key_col);

				/* READ-ONLY: the missing rows (no write here → no FK fire) */
				initStringInfo(&q);
				appendStringInfo(&q,
					"SELECT %s FROM %s src "
					"WHERE NOT EXISTS (SELECT 1 FROM %s l WHERE l.%s = src.%s)",
					cols.data, source_ref, local_ref, kq, kq);

				if (SPI_execute(q.data, true /* read-only */, 0) == SPI_OK_SELECT &&
					SPI_processed > 0)
				{
					int64			n = (int64) SPI_processed;
					int64			i;
					HeapTuple	   *arr;
					Relation		wrel;
					EState		   *estate;
					ResultRelInfo  *rri;
					TupleTableSlot *wslot;
					CommandId		cid;

					/* capture rows to emit (out of the SPI context) */
					MemoryContextSwitchTo(oldcxt);
					arr = (HeapTuple *) palloc(n * sizeof(HeapTuple));
					for (i = 0; i < n; i++)
						arr[i] = heap_copytuple(SPI_tuptable->vals[i]);
					gscan->fed_n = n;
					gscan->fed_tuples = arr;

					/* write-through: heap_insert (bypasses FK/triggers) + indexes */
					wrel = table_open(relid, RowExclusiveLock);
					estate = CreateExecutorState();
					rri = makeNode(ResultRelInfo);
					InitResultRelInfo(rri, wrel, 1, NULL, 0);
					ExecOpenIndices(rri, false);
					wslot = MakeSingleTupleTableSlot(RelationGetDescr(wrel),
													 &TTSOpsHeapTuple);
					cid = GetCurrentCommandId(true);
					estate->es_output_cid = cid;

					for (i = 0; i < n; i++)
					{
						HeapTuple	wt = heap_copytuple(arr[i]);

						heap_insert(wrel, wt, cid, 0, NULL);
						ExecStoreHeapTuple(wt, wslot, false);
						ExecInsertIndexTuples(rri, wslot, estate, false, false,
											  NULL, NIL, false);
					}

					ExecCloseIndices(rri);
					ExecDropSingleTupleTableSlot(wslot);
					FreeExecutorState(estate);
					table_close(wrel, RowExclusiveLock);
				}

				/* observability */
				if (gscan->fed_n > 0)
				{
					char	stat[220];

					snprintf(stat, sizeof(stat),
						"UPDATE gfs.clone_stats "
						"SET fetch_calls = fetch_calls + 1, "
						"rows_fetched = rows_fetched + %lld, last_fetch = now() "
						"WHERE relid::oid = %u",
						(long long) gscan->fed_n, relid);
					SPI_execute(stat, false, 0);
				}
			}
			SPI_finish();
		}
	}
	PG_FINALLY();
	{
		gfs_in_federation = false;
	}
	PG_END_TRY();
}

static TableScanDesc
gfs_scan_begin(Relation rel, Snapshot snapshot, int nkeys, ScanKey key,
			   ParallelTableScanDesc pscan, uint32 flags)
{
	GfsScanDesc gscan = (GfsScanDesc) palloc0(sizeof(GfsScanDescData));

	gscan->rs_base.rs_rd = rel;
	gscan->rs_base.rs_snapshot = snapshot;
	gscan->rs_base.rs_nkeys = nkeys;
	gscan->rs_base.rs_key = key;
	gscan->rs_base.rs_flags = flags;
	gscan->rs_base.rs_parallel = pscan;

	/* The real heap scan owns the snapshot lifecycle (incl. SO_TEMP_SNAPSHOT). */
	gscan->heap_scan = heap_beginscan(rel, snapshot, nkeys, key, pscan, flags);
	gscan->heap_done = false;
	gscan->fed_n = 0;
	gscan->fed_emitted = 0;
	gscan->fed_tuples = NULL;

	/*
	 * Copy-on-read rides the SEQ-SCAN path only. Other scan types (bitmap,
	 * sample, tidrange, analyze) read local rows via the delegated callbacks
	 * below; they don't self-drive federation (a missing row has no index entry
	 * / block to visit). The warming layer materializes via a seq scan.
	 */
	if (flags & SO_TYPE_SEQSCAN)
		gfs_load_federation(gscan);
	return (TableScanDesc) gscan;
}

static void
gfs_scan_end(TableScanDesc scan)
{
	GfsScanDesc gscan = (GfsScanDesc) scan;

	heap_endscan(gscan->heap_scan);	/* unregisters temp snapshot if any */
	pfree(gscan);
}

static void
gfs_scan_rescan(TableScanDesc scan, ScanKey key, bool set_params,
				bool allow_strat, bool allow_sync, bool allow_pagemode)
{
	GfsScanDesc gscan = (GfsScanDesc) scan;

	/* Re-emit the already-fetched rows; do NOT re-fetch / re-write-through. */
	gscan->heap_done = false;
	gscan->fed_emitted = 0;
	heap_rescan(gscan->heap_scan, key, set_params, allow_strat, allow_sync,
				allow_pagemode);
}

static bool
gfs_getnextslot(TableScanDesc scan, ScanDirection direction,
				TupleTableSlot *slot)
{
	GfsScanDesc gscan = (GfsScanDesc) scan;

	/* Phase 1: serve LOCAL heap rows. */
	if (!gscan->heap_done)
	{
		if (heap_getnextslot(gscan->heap_scan, direction, slot))
			return true;
		gscan->heap_done = true;
	}

	/* Phase 2: emit the rows fetched from the source (forward scans only). */
	if (direction == ForwardScanDirection && gscan->fed_emitted < gscan->fed_n)
	{
		ExecForceStoreHeapTuple(gscan->fed_tuples[gscan->fed_emitted], slot,
								false);
		gscan->fed_emitted++;
		return true;
	}

	ExecClearTuple(slot);
	return false;
}

/*
 * All other scan-desc-consuming callbacks just delegate to the inner heap scan
 * (federation rides the seq-scan path only). Without these, heap's own versions
 * would mis-cast our GfsScanDesc as a HeapScanDesc — e.g. a bitmap scan, which
 * becomes possible as soon as a federated table has an index.
 */
static void
gfs_set_tidrange(TableScanDesc scan, ItemPointer mintid, ItemPointer maxtid)
{
	gfs_heap->scan_set_tidrange(((GfsScanDesc) scan)->heap_scan, mintid, maxtid);
}

static bool
gfs_getnextslot_tidrange(TableScanDesc scan, ScanDirection direction,
						 TupleTableSlot *slot)
{
	return gfs_heap->scan_getnextslot_tidrange(((GfsScanDesc) scan)->heap_scan,
											   direction, slot);
}

static bool
gfs_bitmap_next_block(TableScanDesc scan, struct TBMIterateResult *tbmres)
{
	return gfs_heap->scan_bitmap_next_block(((GfsScanDesc) scan)->heap_scan,
											tbmres);
}

static bool
gfs_bitmap_next_tuple(TableScanDesc scan, struct TBMIterateResult *tbmres,
					  TupleTableSlot *slot)
{
	return gfs_heap->scan_bitmap_next_tuple(((GfsScanDesc) scan)->heap_scan,
											tbmres, slot);
}

static bool
gfs_sample_next_block(TableScanDesc scan, struct SampleScanState *scanstate)
{
	return gfs_heap->scan_sample_next_block(((GfsScanDesc) scan)->heap_scan,
											scanstate);
}

static bool
gfs_sample_next_tuple(TableScanDesc scan, struct SampleScanState *scanstate,
					  TupleTableSlot *slot)
{
	return gfs_heap->scan_sample_next_tuple(((GfsScanDesc) scan)->heap_scan,
											scanstate, slot);
}

static bool
gfs_analyze_next_block(TableScanDesc scan, BlockNumber blockno,
					   BufferAccessStrategy bstrategy)
{
	return gfs_heap->scan_analyze_next_block(((GfsScanDesc) scan)->heap_scan,
											 blockno, bstrategy);
}

static bool
gfs_analyze_next_tuple(TableScanDesc scan, TransactionId OldestXmin,
					   double *liverows, double *deadrows, TupleTableSlot *slot)
{
	return gfs_heap->scan_analyze_next_tuple(((GfsScanDesc) scan)->heap_scan,
											 OldestXmin, liverows, deadrows, slot);
}

static bool
gfs_index_fetch_tuple(struct IndexFetchTableData *scan, ItemPointer tid,
					  Snapshot snapshot, TupleTableSlot *slot,
					  bool *call_again, bool *all_dead)
{
	/* interception point for PK/index copy-on-read; delegates for now */
	return gfs_heap->index_fetch_tuple(scan, tid, snapshot, slot, call_again,
									   all_dead);
}

static double
gfs_index_build_range_scan(Relation heapRel, Relation indexRel,
						   IndexInfo *indexInfo, bool allow_sync,
						   bool anyvisible, bool progress,
						   BlockNumber start_blockno, BlockNumber numblocks,
						   IndexBuildCallback callback, void *callback_state,
						   TableScanDesc scan)
{
	Datum			values[INDEX_MAX_KEYS];
	bool			isnull[INDEX_MAX_KEYS];
	double			reltuples = 0;
	ExprState	   *predicate;
	TupleTableSlot *slot;
	EState		   *estate;
	ExprContext	   *econtext;
	Snapshot		snapshot;
	bool			need_unregister_snapshot = false;
	TableScanDesc	heap_scan;
	bool			own_scan = false;

	estate = CreateExecutorState();
	econtext = GetPerTupleExprContext(estate);
	slot = table_slot_create(heapRel, NULL);
	econtext->ecxt_scantuple = slot;
	predicate = ExecPrepareQual(indexInfo->ii_Predicate, estate);

	if (scan == NULL)
	{
		uint32		flags = SO_TYPE_SEQSCAN | SO_ALLOW_PAGEMODE | SO_ALLOW_STRAT;

		if (allow_sync)
			flags |= SO_ALLOW_SYNC;
		snapshot = RegisterSnapshot(GetTransactionSnapshot());
		need_unregister_snapshot = true;
		/*
		 * Build over PHYSICAL heap storage only: a private heap scan, never our
		 * federating AM scan. heap_setscanlimits below requires a real
		 * HeapScanDesc, and we must not re-trigger copy-on-read during a build.
		 */
		heap_scan = heap_beginscan(heapRel, snapshot, 0, NULL, NULL, flags);
		own_scan = true;
	}
	else
	{
		/* Parallel build handed us a GfsScanDesc; index the underlying heap. */
		heap_scan = ((GfsScanDesc) scan)->heap_scan;
		snapshot = heap_scan->rs_snapshot;
	}

	if (!allow_sync)
		heap_setscanlimits(heap_scan, start_blockno, numblocks);

	while (heap_getnextslot(heap_scan, ForwardScanDirection, slot))
	{
		ItemPointerData tid;

		CHECK_FOR_INTERRUPTS();
		MemoryContextReset(econtext->ecxt_per_tuple_memory);
		if (predicate != NULL && !ExecQual(predicate, econtext))
			continue;
		FormIndexDatum(indexInfo, slot, estate, values, isnull);
		tid = slot->tts_tid;
		callback(indexRel, &tid, values, isnull, true, callback_state);
		reltuples++;
	}

	if (own_scan)
		heap_endscan(heap_scan);
	if (need_unregister_snapshot)
		UnregisterSnapshot(snapshot);
	ExecDropSingleTupleTableSlot(slot);
	FreeExecutorState(estate);
	return reltuples;
}

static void
gfs_index_validate_scan(Relation heapRel, Relation indexRel,
						IndexInfo *indexInfo, Snapshot snapshot,
						struct ValidateIndexState *state)
{
	ereport(ERROR,
			(errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
			 errmsg("CREATE INDEX CONCURRENTLY is not supported on gfs tables (PoC)")));
}

Datum
gfs_handler(PG_FUNCTION_ARGS)
{
	if (!gfs_ready)
	{
		gfs_heap = GetHeapamTableAmRoutine();
		memcpy(&gfs_methods, gfs_heap, sizeof(TableAmRoutine));
		gfs_methods.relation_needs_toast_table = gfs_no_toast;
		gfs_methods.scan_begin = gfs_scan_begin;
		gfs_methods.scan_end = gfs_scan_end;
		gfs_methods.scan_rescan = gfs_scan_rescan;
		gfs_methods.scan_getnextslot = gfs_getnextslot;
		gfs_methods.scan_set_tidrange = gfs_set_tidrange;
		gfs_methods.scan_getnextslot_tidrange = gfs_getnextslot_tidrange;
		gfs_methods.scan_bitmap_next_block = gfs_bitmap_next_block;
		gfs_methods.scan_bitmap_next_tuple = gfs_bitmap_next_tuple;
		gfs_methods.scan_sample_next_block = gfs_sample_next_block;
		gfs_methods.scan_sample_next_tuple = gfs_sample_next_tuple;
		gfs_methods.scan_analyze_next_block = gfs_analyze_next_block;
		gfs_methods.scan_analyze_next_tuple = gfs_analyze_next_tuple;
		gfs_methods.index_fetch_tuple = gfs_index_fetch_tuple;
		gfs_methods.index_build_range_scan = gfs_index_build_range_scan;
		gfs_methods.index_validate_scan = gfs_index_validate_scan;
		gfs_ready = true;
	}
	PG_RETURN_POINTER(&gfs_methods);
}
