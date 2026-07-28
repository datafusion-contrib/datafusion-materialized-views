// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Readiness-related types and helpers used by the query-rewrite optimizer.
//!
//! Two-stage readiness contract:
//!
//! 1. At LP-rewrite time, `ViewMatcher` consults
//!    [`Materialized::rewrite_readiness`](crate::materialized::Materialized::rewrite_readiness)
//!    and drops candidates whose readiness is [`RewriteReadiness::NotReady`].
//!    `Ready` and `Unknown` both enter the candidate set and their raw
//!    readiness is recorded on [`CandidateMetadata::Materialized::readiness`].
//! 2. At physical-planning time (once `TableProvider::scan()` has run on every
//!    branch), `ViewExploitationPlanner` refreshes the readiness on each
//!    candidate. Providers that opt into [`ReadinessAnnotatedExec`] have
//!    their readiness read straight off the plan tree (atomically captured
//!    at scan time); others fall back to sampling the current provider
//!    state, which is best-effort but racy under concurrent snapshot swaps.
//!
//! The cost function only ever sees `Base` or `Materialized { Ready | Unknown }`
//! entries — `NotReady` is filtered by both gates before it can reach cost
//! policy.

use std::sync::Arc;

use datafusion::catalog::CatalogProviderList;
use datafusion::execution::context::SessionState;
use datafusion::execution::TaskContext;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, PlanProperties, SendableRecordBatchStream,
};
use datafusion_common::tree_node::{Transformed, TreeNode};
use datafusion_common::{DataFusionError, Result, TableReference};

use crate::materialized::cast_to_materialized;

use super::exploitation::RewriteContext;

/// Whether a materialized view is currently safe to route queries to. Reported
/// by [`Materialized::rewrite_readiness`](crate::materialized::Materialized::rewrite_readiness)
/// and consulted by
/// [`ViewMatcher`](crate::rewrite::exploitation::ViewMatcher) during LP rewrite
/// so that unpopulated / in-flight MVs are excluded from the candidate set
/// upstream of the cost function.
///
/// Keeping this a lifecycle abstraction (rather than a proxy such as file
/// count) means the trait doesn't couple to any specific storage layout —
/// providers describe their own readiness however they want (index loaded,
/// snapshot published, migration complete, staleness threshold satisfied,
/// etc.) and only report the answer.
// `PartialOrd`/`Ord` are derived so types that embed `RewriteReadiness` (e.g.
// candidate metadata) can derive ordering traits when needed. The variant
// ordering has no lifecycle meaning — callers must not depend on
// `Ready < NotReady < Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RewriteReadiness {
    /// The MV is populated and can safely answer the query. The
    /// `ViewMatchingRewriter` will include it as a rewrite candidate.
    Ready,
    /// The MV should not be used yet (index not loaded, ingest task never
    /// ran after a version bump, snapshot rebuild in progress, etc.).
    /// The `ViewMatchingRewriter` will drop the MV from the candidate set
    /// so a query never gets routed to it and silently returns empty.
    NotReady,
    /// The provider cannot cheaply determine readiness. The
    /// `ViewMatchingRewriter` treats this as "include as candidate" and
    /// propagates the `Unknown` value to the cost function via
    /// `CandidateMetadata::Materialized { readiness, .. }`, so the caller
    /// can pick whatever policy fits — e.g. fall back to the base scan
    /// cost rather than trust an EmptyExec candidate as predicate-pruned.
    /// Default value returned by the trait's blanket impl so
    /// backward-compatible providers keep the pre-existing "always a
    /// candidate" behaviour.
    Unknown,
}

/// Per-branch metadata inside a `OneOf` / `OneOfExec`. One variant per branch,
/// aligned by index with the containing `branches` / `candidates` vector.
///
/// Lifecycle judgement (populated / unpopulated / stale / ...) is made in two
/// stages via [`Materialized::rewrite_readiness`](crate::materialized::Materialized::rewrite_readiness):
///
/// 1. At LP-rewrite time,
///    [`ViewMatcher`](crate::rewrite::exploitation::ViewMatcher) drops `NotReady`
///    providers and admits `Ready` + `Unknown` as candidates.
/// 2. At physical-planning time, once DataFusion has invoked each provider's
///    `scan()` (giving lazy indexes a chance to warm),
///    [`ViewExploitationPlanner`](crate::rewrite::exploitation::ViewExploitationPlanner)
///    re-consults `rewrite_readiness()` and updates the metadata to the current
///    value; providers that transitioned to `NotReady` between the two stages are
///    dropped here.
///
/// So any `Materialized` variant that reaches the cost function reflects the
/// **post-scan** readiness, and `NotReady` is guaranteed to have been filtered
/// (upstream or downstream of scan) before the cost function runs. `Ready`
/// and `Unknown` remain distinguishable so cost policy for the two can diverge
/// (typically: trust `Ready` as safe to route, and fall back to a conservative
/// estimate for `Unknown`).
///
/// The invariant enforced at construction inside
/// [`ViewMatcher`](crate::rewrite::exploitation::ViewMatcher):
///
/// * Index 0 is always [`CandidateMetadata::Base`] — the query's original LP,
///   with no MV rewrite applied. `OneOf::schema()` reads `branches[0].schema()`
///   and therefore always exposes the query's schema (not an MV's).
/// * Indices 1..N are [`CandidateMetadata::Materialized`] entries, sorted
///   deterministically by `table_ref`. Sorting on the MV's registered
///   `TableReference` (rather than on the branch LP) keeps the order stable
///   under downstream LP transformations that happen between LP rewrite and
///   physical planning — identity projection elimination, always-true filter
///   folding, etc. change the LP but never the MV's registered name, so the
///   alignment between this metadata and the physical candidate the cost
///   function receives survives every optimizer pass.
#[derive(Debug, Clone, PartialEq, PartialOrd, Eq, Hash)]
pub enum CandidateMetadata {
    /// The query's original branch (index 0 in every OneOf).
    Base,
    /// A materialized-view rewrite of the query. Only MVs that reported
    /// `Ready` or `Unknown` readiness reach this state; `NotReady` MVs
    /// are filtered upstream by
    /// [`ViewMatcher`](crate::rewrite::exploitation::ViewMatcher).
    /// Cost functions consult the `readiness` field to distinguish the
    /// two cases: `Unknown` (the trait default returned by
    /// [`RewriteReadiness`]) must not be silently treated as `Ready`,
    /// since that would regress providers that haven't declared a
    /// lifecycle.
    Materialized {
        /// Registered `TableReference` of the source MV. Stored directly
        /// rather than as `.to_string()` so identifiers with dots or
        /// quoting round-trip losslessly through the physical-time
        /// catalog resolution. Stable across LP transformations; safe to
        /// use as a sort key.
        table_ref: TableReference,
        /// Provider readiness as observed at physical-planning time — i.e.
        /// after DataFusion has invoked `TableProvider::scan()` on this
        /// branch. Set once at LP rewrite from the provider's initial
        /// report and refreshed from the same provider inside
        /// [`ViewExploitationPlanner::plan_extension`](crate::rewrite::exploitation::ViewExploitationPlanner)
        /// so lazy-index providers surface their warmed-up value.
        /// Downstream cost functions branch on this to implement their
        /// `Unknown` policy (e.g. fall back to the base scan cost rather
        /// than trust an EmptyExec as predicate-pruned).
        readiness: RewriteReadiness,
    },
}

/// Wraps the `ExecutionPlan` returned by a `Materialized` provider's
/// `scan()` together with the readiness value captured at scan time.
///
/// Providers use this to close the race between "which snapshot did
/// scan read from" and "what does `rewrite_readiness()` return now".
/// Sampling `rewrite_readiness()` again after `scan()` has returned is
/// racy — a concurrent snapshot swap can publish new state between the
/// two calls, letting an `EmptyExec` from the old snapshot be labelled
/// with readiness from the new one. If the provider computes both
/// atomically (from the same state Arc, under the same read lock, etc.)
/// and wraps the returned plan with `ReadinessAnnotatedExec`, the
/// physical-time refresh in
/// [`ViewExploitationPlanner::plan_extension`](crate::rewrite::exploitation::ViewExploitationPlanner)
/// reads the annotated value verbatim instead of re-sampling.
///
/// The wrapper is transparent: all `ExecutionPlan` methods delegate to
/// the inner plan, so downstream operators see the same shape as if the
/// provider returned `inner` directly.
///
/// Providers that don't wrap fall back to the pre-existing (racy)
/// sampling path in the refresh, which remains supported for backward
/// compatibility.
#[derive(Debug)]
pub struct ReadinessAnnotatedExec {
    inner: Arc<dyn ExecutionPlan>,
    readiness: RewriteReadiness,
    properties: PlanProperties,
}

impl ReadinessAnnotatedExec {
    /// Wrap `inner` with the readiness captured atomically at scan time.
    pub fn new(inner: Arc<dyn ExecutionPlan>, readiness: RewriteReadiness) -> Self {
        let properties = inner.properties().clone();
        Self {
            inner,
            readiness,
            properties,
        }
    }

    /// Readiness captured at the moment the provider produced `inner`.
    pub fn readiness(&self) -> RewriteReadiness {
        self.readiness
    }

    /// The wrapped plan.
    pub fn inner(&self) -> &Arc<dyn ExecutionPlan> {
        &self.inner
    }
}

impl DisplayAs for ReadinessAnnotatedExec {
    fn fmt_as(&self, t: DisplayFormatType, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match t {
            DisplayFormatType::Default | DisplayFormatType::Verbose => {
                write!(f, "ReadinessAnnotatedExec: readiness={:?}", self.readiness)
            }
            DisplayFormatType::TreeRender => Ok(()),
        }
    }
}

impl ExecutionPlan for ReadinessAnnotatedExec {
    fn name(&self) -> &str {
        "ReadinessAnnotatedExec"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn properties(&self) -> &PlanProperties {
        &self.properties
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![&self.inner]
    }

    fn with_new_children(
        self: Arc<Self>,
        children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        if children.len() != 1 {
            return Err(DataFusionError::Plan(format!(
                "ReadinessAnnotatedExec expects exactly one child, got {}",
                children.len()
            )));
        }
        Ok(Arc::new(ReadinessAnnotatedExec::new(
            children.into_iter().next().unwrap(),
            self.readiness,
        )))
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        self.inner.execute(partition, context)
    }

    fn partition_statistics(
        &self,
        partition: Option<usize>,
    ) -> Result<datafusion_common::Statistics> {
        self.inner.partition_statistics(partition)
    }
}

/// Depth-first search for the nearest `ReadinessAnnotatedExec` in a
/// physical plan tree. Returns its readiness value, or `None` if the
/// tree doesn't contain one (e.g. the provider hasn't opted in). Used
/// by the physical-time refresh to prefer atomically-captured readiness
/// over racy re-sampling.
pub fn readiness_from_plan(plan: &Arc<dyn ExecutionPlan>) -> Option<RewriteReadiness> {
    if let Some(annotated) = plan.as_any().downcast_ref::<ReadinessAnnotatedExec>() {
        return Some(annotated.readiness());
    }
    for child in plan.children() {
        if let Some(readiness) = readiness_from_plan(child) {
            return Some(readiness);
        }
    }
    None
}

/// Remove every `ReadinessAnnotatedExec` from a physical plan tree and
/// return the plan with each wrapper collapsed to its inner. Used by
/// `ViewExploitationPlanner::plan_extension` after `readiness_from_plan`
/// has extracted the annotation — the wrapper's job is done at that
/// point and it shouldn't leak into the plan handed to downstream
/// optimizers, cost functions, `OneOfExec`, or physical-plan codecs.
///
/// The wrapper is deliberately minimal (it only delegates `properties`,
/// `execute`, and `partition_statistics`); leaving it in place would
/// silently block optimizer passes such as limit pushdown, projection
/// pushdown, or `with_preserve_order`, which look at
/// `ExecutionPlan` trait methods we don't proxy.
pub fn strip_readiness_annotation(plan: Arc<dyn ExecutionPlan>) -> Result<Arc<dyn ExecutionPlan>> {
    plan.transform(&|node: Arc<dyn ExecutionPlan>| {
        if let Some(annotated) = node.as_any().downcast_ref::<ReadinessAnnotatedExec>() {
            Ok(Transformed::yes(Arc::clone(annotated.inner())))
        } else {
            Ok(Transformed::no(node))
        }
    })
    .map(|t| t.data)
}

/// Pair-wise filter that drops every `Materialized` candidate whose
/// refreshed readiness is `NotReady`, along with its aligned entry in
/// `physical_inputs`. Base is never filtered.
///
/// If `candidates` is empty or is not aligned with `physical_inputs`
/// (older callsites that constructed the `OneOf` without per-branch
/// metadata, or a caller that supplied a mismatched vector), the inputs
/// are passed through unchanged and the metadata is dropped entirely —
/// preserving a misaligned slice would let it flow into `OneOfExec` and
/// attribute readiness to the wrong branch inside the cost function.
/// Callsites without metadata keep their original behaviour (the empty
/// slice is what the pre-readiness code path also carried).
pub(super) fn drop_not_ready_after_refresh(
    physical_inputs: &[Arc<dyn ExecutionPlan>],
    candidates: &[CandidateMetadata],
) -> (Vec<Arc<dyn ExecutionPlan>>, Vec<CandidateMetadata>) {
    if candidates.len() != physical_inputs.len() {
        return (physical_inputs.to_vec(), Vec::new());
    }
    let mut kept_inputs = Vec::with_capacity(physical_inputs.len());
    let mut kept_candidates = Vec::with_capacity(candidates.len());
    for (input, cand) in physical_inputs.iter().zip(candidates.iter()) {
        let drop = matches!(
            cand,
            CandidateMetadata::Materialized {
                readiness: RewriteReadiness::NotReady,
                ..
            }
        );
        if drop {
            continue;
        }
        kept_inputs.push(Arc::clone(input));
        kept_candidates.push(cand.clone());
    }
    (kept_inputs, kept_candidates)
}

/// Re-consult every `Materialized` candidate's `rewrite_readiness()` and
/// return a `RewriteContext` whose metadata reflects the current values.
///
/// Called from `ViewExploitationPlanner::plan_extension` once
/// physical inputs have been planned (i.e. after `TableProvider::scan()`
/// has run on every candidate branch). Providers whose readiness only
/// becomes definite after scan initialization (lazy indexes, warmup jobs,
/// snapshot swaps that happen inside `scan`) surface their new value here
/// so the cost function sees the definitive readiness rather than the
/// stale LP-time snapshot.
///
/// For each `Materialized` candidate the refresh prefers the atomic
/// readiness captured at scan time (via `ReadinessAnnotatedExec` in the
/// candidate's `physical_input`) over racy re-sampling. Providers that
/// haven't opted into the wrapper fall back to sampling the current
/// `rewrite_readiness()` through the catalog — this is best-effort and
/// can be stale under concurrent snapshot swaps (see `ReadinessAnnotatedExec`).
///
/// Lookup failures on the fallback path (table not found in the
/// catalog, provider is no longer a `Materialized`, `cast_to_materialized`
/// error) preserve the LP-time value — never downgrade what we already
/// observed.
pub(super) async fn refresh_candidate_readiness(
    context: RewriteContext,
    physical_inputs: &[Arc<dyn ExecutionPlan>],
    session_state: &SessionState,
) -> RewriteContext {
    let catalog_list = session_state.catalog_list();
    let default_catalog = &session_state.config().options().catalog.default_catalog;
    let default_schema = &session_state.config().options().catalog.default_schema;

    let candidates = context.candidates();
    // Alignment-tolerant zipping: if a caller supplied metadata whose
    // length differs from `physical_inputs`, we can't safely pair them,
    // so we skip the annotated lookup for the misaligned indices. The
    // downstream `drop_not_ready_after_refresh` filter handles the same
    // case defensively.
    let aligned = candidates.len() == physical_inputs.len();

    let refreshed: Vec<CandidateMetadata> =
        futures::future::join_all(candidates.iter().enumerate().map(|(idx, c)| async move {
            match c {
                CandidateMetadata::Base => CandidateMetadata::Base,
                CandidateMetadata::Materialized {
                    table_ref,
                    readiness,
                } => {
                    // Prefer atomically-captured readiness from the physical
                    // input over sampling — closes the race between the
                    // snapshot scan read from and the current provider state.
                    let annotated = if aligned {
                        readiness_from_plan(&physical_inputs[idx])
                    } else {
                        None
                    };

                    let new_readiness = match annotated {
                        Some(r) => r,
                        None => resolve_current_readiness(
                            catalog_list.as_ref(),
                            default_catalog,
                            default_schema,
                            table_ref,
                        )
                        .await
                        .unwrap_or(*readiness),
                    };

                    CandidateMetadata::Materialized {
                        table_ref: table_ref.clone(),
                        readiness: new_readiness,
                    }
                }
            }
        }))
        .await;

    context.with_candidates(refreshed)
}

/// Resolve `table_ref` through the catalog list and re-consult the
/// provider's `rewrite_readiness()`. Returns `None` on any lookup failure
/// so the caller can fall back to the LP-time value.
async fn resolve_current_readiness(
    catalog_list: &dyn CatalogProviderList,
    default_catalog: &str,
    default_schema: &str,
    table_ref: &TableReference,
) -> Option<RewriteReadiness> {
    // Resolve the (possibly bare or partial) reference against the
    // session's default catalog/schema without ever going through
    // `Display`/`parse_str`, which is not lossless for quoted or dotted
    // identifiers.
    let resolved = table_ref.clone().resolve(default_catalog, default_schema);
    let catalog = catalog_list.catalog(resolved.catalog.as_ref())?;
    let schema = catalog.schema(resolved.schema.as_ref())?;
    let table = match schema.table(resolved.table.as_ref()).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            log::trace!("refresh_candidate_readiness: no table {table_ref} in catalog");
            return None;
        }
        Err(e) => {
            log::warn!("refresh_candidate_readiness: catalog lookup failed for {table_ref}: {e}");
            return None;
        }
    };
    match cast_to_materialized(table.as_ref()) {
        Ok(Some(mv)) => Some(mv.rewrite_readiness()),
        Ok(None) => None,
        Err(e) => {
            log::warn!(
                "refresh_candidate_readiness: cast_to_materialized failed for {table_ref}: {e}"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_variants_are_distinct_and_comparable() {
        assert_ne!(RewriteReadiness::Ready, RewriteReadiness::NotReady);
        assert_ne!(RewriteReadiness::Ready, RewriteReadiness::Unknown);
        assert_ne!(RewriteReadiness::NotReady, RewriteReadiness::Unknown);
        assert_eq!(RewriteReadiness::Ready, RewriteReadiness::Ready);
    }

    #[test]
    fn readiness_is_copy_and_hashable() {
        // Ensure the variants can be stored / matched cheaply from the
        // rewrite path without cloning.
        fn requires_copy<T: Copy>() {}
        fn requires_hash<T: std::hash::Hash>() {}
        requires_copy::<RewriteReadiness>();
        requires_hash::<RewriteReadiness>();
    }

    #[test]
    fn readiness_annotated_exec_delegates_properties_and_children() {
        use arrow_schema::Schema;
        use datafusion::physical_plan::empty::EmptyExec;
        let schema = Arc::new(Schema::empty());
        let inner: Arc<dyn ExecutionPlan> = Arc::new(EmptyExec::new(Arc::clone(&schema)));
        let annotated = ReadinessAnnotatedExec::new(Arc::clone(&inner), RewriteReadiness::Ready);

        assert_eq!(annotated.readiness(), RewriteReadiness::Ready);
        // Same schema/partitioning as inner — the wrapper is transparent.
        assert_eq!(
            format!("{:?}", annotated.properties()),
            format!("{:?}", inner.properties())
        );
        // Inner exposed as the sole child.
        let children = annotated.children();
        assert_eq!(children.len(), 1);
        assert!(Arc::ptr_eq(children[0], &inner));
    }

    #[test]
    fn strip_returns_inner_for_bare_annotated_exec() {
        use arrow_schema::Schema;
        use datafusion::physical_plan::empty::EmptyExec;
        let schema = Arc::new(Schema::empty());
        let inner: Arc<dyn ExecutionPlan> = Arc::new(EmptyExec::new(schema));
        let annotated: Arc<dyn ExecutionPlan> = Arc::new(ReadinessAnnotatedExec::new(
            Arc::clone(&inner),
            RewriteReadiness::Ready,
        ));

        let stripped = strip_readiness_annotation(annotated).expect("strip");
        assert!(
            stripped
                .as_any()
                .downcast_ref::<ReadinessAnnotatedExec>()
                .is_none(),
            "top-level wrapper must be gone after strip"
        );
        assert!(Arc::ptr_eq(&stripped, &inner));
    }

    #[test]
    fn strip_replaces_annotated_exec_inside_nested_plan() {
        // Providers wrap their scan output, but DataFusion may put a
        // Filter/Projection/Repartition on top before `plan_extension` sees
        // the plan. Strip must find the wrapper wherever it sits.
        use arrow_schema::Schema;
        use datafusion::physical_plan::empty::EmptyExec;
        use datafusion::physical_plan::repartition::RepartitionExec;
        use datafusion_physical_expr::Partitioning;
        let schema = Arc::new(Schema::empty());
        let leaf: Arc<dyn ExecutionPlan> = Arc::new(EmptyExec::new(schema));
        let annotated: Arc<dyn ExecutionPlan> = Arc::new(ReadinessAnnotatedExec::new(
            Arc::clone(&leaf),
            RewriteReadiness::Ready,
        ));
        let wrapped: Arc<dyn ExecutionPlan> = Arc::new(
            RepartitionExec::try_new(annotated, Partitioning::RoundRobinBatch(1))
                .expect("repartition"),
        );

        let stripped = strip_readiness_annotation(wrapped).expect("strip");
        assert!(
            readiness_from_plan(&stripped).is_none(),
            "no ReadinessAnnotatedExec should remain anywhere in the tree"
        );
    }

    #[test]
    fn strip_is_noop_when_no_annotation_present() {
        use arrow_schema::Schema;
        use datafusion::physical_plan::empty::EmptyExec;
        let schema = Arc::new(Schema::empty());
        let plan: Arc<dyn ExecutionPlan> = Arc::new(EmptyExec::new(schema));

        let stripped = strip_readiness_annotation(Arc::clone(&plan)).expect("strip");
        assert!(Arc::ptr_eq(&stripped, &plan));
    }
}
