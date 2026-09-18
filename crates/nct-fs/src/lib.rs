// nc-tools fs family crate: fs.*, patch.*, search.* and workspace snapshots.
// Behavior-parity port of src/kernel/{fs,patch,search,snapshot}.mjs — graded
// by conformance/golden/tools.json + tools/conformance.mjs.
pub mod diff;
pub mod fs_tools;
pub mod graph;
pub mod patch;
pub mod search;
pub mod snapshot;
pub mod symbols;
pub mod watch;

use std::sync::Arc;

use nct_core::kernel::Kernel;
use nct_core::schema::schema_for;

use crate::diff::*;
use crate::fs_tools::*;
use crate::graph::*;
use crate::patch::*;
use crate::search::*;
use crate::snapshot::*;
use crate::symbols::*;
use crate::watch::*;

pub fn register(k: &mut Kernel) {
    // fs.*
    k.register(
        "fs.read",
        READ_DESC,
        schema_for::<ReadArgs>(),
        Arc::new(ReadHandler),
    );
    k.register(
        "fs.readMany",
        READ_MANY_DESC,
        schema_for::<ReadManyArgs>(),
        Arc::new(ReadManyHandler),
    );
    k.register(
        "fs.write",
        WRITE_DESC,
        schema_for::<WriteArgs>(),
        Arc::new(WriteHandler),
    );
    k.register(
        "fs.writeMany",
        WRITE_MANY_DESC,
        schema_for::<WriteManyArgs>(),
        Arc::new(WriteManyHandler),
    );
    k.register(
        "fs.append",
        APPEND_DESC,
        schema_for::<WriteArgs>(),
        Arc::new(AppendHandler),
    );
    k.register(
        "fs.copy",
        COPY_DESC,
        schema_for::<CopyArgs>(),
        Arc::new(CopyHandler),
    );
    k.register(
        "fs.list",
        LIST_DESC,
        schema_for::<ListArgs>(),
        Arc::new(ListHandler),
    );
    k.register(
        "fs.stat",
        STAT_DESC,
        schema_for::<PathArgs>(),
        Arc::new(StatHandler),
    );
    k.register(
        "fs.mkdir",
        MKDIR_DESC,
        schema_for::<MkdirArgs>(),
        Arc::new(MkdirHandler),
    );
    k.register(
        "fs.delete",
        DELETE_DESC,
        schema_for::<DeleteArgs>(),
        Arc::new(DeleteHandler),
    );
    k.register(
        "fs.move",
        MOVE_DESC,
        schema_for::<MoveArgs>(),
        Arc::new(MoveHandler),
    );
    // patch.*
    k.register(
        "patch.apply",
        PATCH_APPLY_DESC,
        schema_for::<ApplyArgs>(),
        Arc::new(ApplyHandler),
    );
    k.register(
        "patch.applyMany",
        PATCH_APPLY_MANY_DESC,
        schema_for::<ApplyManyArgs>(),
        Arc::new(ApplyManyHandler),
    );
    // search.*
    k.register(
        "search.grep",
        GREP_DESC,
        schema_for::<GrepArgs>(),
        Arc::new(GrepHandler),
    );
    k.register(
        "search.files",
        FILES_DESC,
        schema_for::<FilesArgs>(),
        Arc::new(FilesHandler),
    );
    // sys snapshots (content lives here; sys.journal/sys.workspace/batch live in nct-core)
    k.register(
        "sys.snapshot",
        SNAPSHOT_DESC,
        schema_for::<SnapshotArgs>(),
        Arc::new(SnapshotHandler),
    );
    k.register(
        "sys.rollback",
        ROLLBACK_DESC,
        schema_for::<RollbackArgs>(),
        Arc::new(RollbackHandler),
    );
    k.register(
        "sys.listSnapshots",
        LIST_SNAPSHOTS_DESC,
        schema_for::<ListSnapshotsArgs>(),
        Arc::new(ListSnapshotsHandler),
    );
    k.register(
        "sys.snapshotDiff",
        SNAPSHOT_DIFF_DESC,
        schema_for::<SnapshotDiffArgs>(),
        Arc::new(SnapshotDiffHandler),
    );
    // phase 2 additions
    k.register(
        "fs.readRange",
        READ_RANGE_DESC,
        schema_for::<ReadRangeArgs>(),
        Arc::new(ReadRangeHandler),
    );
    k.register(
        "fs.tree",
        TREE_DESC,
        schema_for::<TreeArgs>(),
        Arc::new(TreeHandler),
    );
    k.register(
        "search.replace",
        REPLACE_DESC,
        schema_for::<ReplaceArgs>(),
        Arc::new(ReplaceHandler),
    );
    k.register(
        "code.symbols",
        SYMBOLS_DESC,
        schema_for::<SymbolsArgs>(),
        Arc::new(SymbolsHandler),
    );
    k.register(
        "code.graph",
        GRAPH_DESC,
        schema_for::<GraphArgs>(),
        Arc::new(GraphHandler),
    );
    k.register(
        "fs.watch",
        WATCH_DESC,
        schema_for::<WatchArgs>(),
        Arc::new(WatchSemanticHandler),
    );
    k.register(
        "text.diff",
        DIFF_DESC,
        schema_for::<DiffArgs>(),
        Arc::new(DiffHandler),
    );
}
