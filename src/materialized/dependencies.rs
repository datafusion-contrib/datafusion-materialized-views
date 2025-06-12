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

use datafusion::{
    catalog::{CatalogProviderList, TableFunctionImpl},
    config::{CatalogOptions, ConfigOptions},
    datasource::{provider_as_source, TableProvider, ViewTable},
    prelude::{flatten, get_field, make_array},
};
use datafusion_common::{
    alias::AliasGenerator,
    internal_err,
    tree_node::{Transformed, TreeNode},
    DFSchema, DataFusionError, Result, ScalarValue,
};
use datafusion_expr::{
    col, lit, utils::split_conjunction, Expr, LogicalPlan, LogicalPlanBuilder, TableScan,
};
use datafusion_functions::string::expr_fn::{concat, concat_ws};
use datafusion_sql::TableReference;
use itertools::{Either, Itertools};
use std::{collections::HashSet, sync::Arc};

use crate::materialized::META_COLUMN;

use super::{cast_to_materialized, row_metadata::RowMetadataRegistry, util, Materialized};

/// A table function that shows build targets and dependencies for a materialized view:
///
/// ```ignore
/// fn mv_dependencies(table_ref: Utf8) -> Table
/// ```
///
/// `table_ref` should point to a table provider registered for the current session
/// that implements [`Materialized`]. Otherwise the function will throw an error.
///
/// # Example
///
/// ```sql
/// SELECT * FROM mv_dependencies('datafusion.public.m1');
///
/// +--------------------------------+----------------------+---------------------+-------------------+--------------------------------------+----------------------+
/// | target                         | source_table_catalog | source_table_schema | source_table_name | source_uri                           | source_last_modified |
/// +--------------------------------+----------------------+---------------------+-------------------+--------------------------------------+----------------------+
/// | s3://m1/partition_column=2021/ | datafusion           | public              | t1                | s3://t1/column1=2021/data.01.parquet | 2023-07-11T16:29:26  |
/// | s3://m1/partition_column=2022/ | datafusion           | public              | t1                | s3://t1/column1=2022/data.01.parquet | 2023-07-11T16:45:22  |
/// | s3://m1/partition_column=2023/ | datafusion           | public              | t1                | s3://t1/column1=2023/data.01.parquet | 2023-07-11T16:45:44  |
/// +--------------------------------+----------------------+---------------------+-------------------+--------------------------------------+----------------------+
/// ```
pub fn mv_dependencies(
    catalog_list: Arc<dyn CatalogProviderList>,
    row_metadata_registry: Arc<RowMetadataRegistry>,
    options: &ConfigOptions,
) -> Arc<dyn TableFunctionImpl + 'static> {
    Arc::new(FileDependenciesUdtf::new(
        catalog_list,
        row_metadata_registry,
        options,
    ))
}

#[derive(Debug)]
struct FileDependenciesUdtf {
    catalog_list: Arc<dyn CatalogProviderList>,
    row_metadata_registry: Arc<RowMetadataRegistry>,
    config_options: ConfigOptions,
}

impl FileDependenciesUdtf {
    fn new(
        catalog_list: Arc<dyn CatalogProviderList>,
        row_metadata_registry: Arc<RowMetadataRegistry>,
        config_options: &ConfigOptions,
    ) -> Self {
        Self {
            catalog_list,
            config_options: config_options.clone(),
            row_metadata_registry,
        }
    }
}

impl TableFunctionImpl for FileDependenciesUdtf {
    fn call(&self, args: &[Expr]) -> Result<Arc<dyn TableProvider>> {
        let table_name = get_table_name(args)?;

        let table_ref = TableReference::from(table_name).resolve(
            &self.config_options.catalog.default_catalog,
            &self.config_options.catalog.default_schema,
        );

        let table = util::get_table(self.catalog_list.as_ref(), &table_ref)
            .map_err(|e| DataFusionError::Plan(e.to_string()))?;

        let mv = cast_to_materialized(table.as_ref()).ok_or(DataFusionError::Plan(format!(
            "mv_dependencies: table '{table_name} is not a materialized view. (Materialized TableProviders must be registered using register_materialized"),
        ))?;

        Ok(Arc::new(ViewTable::new(
            mv_dependencies_plan(
                mv,
                self.row_metadata_registry.as_ref(),
                &self.config_options,
            )?,
            None,
        )))
    }
}

/// A table function that shows which files need to be regenerated.
/// Checks `last_modified` timestamps from the file metadata table
/// and deems a target stale if any of its sources are newer than it.
///
/// # `file_metadata`
///
/// Accepts a [`TableProvider`] whose schema matches that of [`FileMetadata`](super::file_metadata::FileMetadata).
/// Normally, a `FileMetadata` may be passed in as normal, but custom file metadata sources or mock data can be passed in
/// with a user-provided `TableProvider`.
pub fn stale_files(
    catalog_list: Arc<dyn CatalogProviderList>,
    row_metadata_registry: Arc<RowMetadataRegistry>,
    file_metadata: Arc<dyn TableProvider>,
    config_options: &ConfigOptions,
) -> Arc<dyn TableFunctionImpl + 'static> {
    Arc::new(StaleFilesUdtf {
        mv_dependencies: FileDependenciesUdtf {
            catalog_list,
            row_metadata_registry,
            config_options: config_options.clone(),
        },
        file_metadata,
    })
}

#[derive(Debug)]
struct StaleFilesUdtf {
    mv_dependencies: FileDependenciesUdtf,
    file_metadata: Arc<dyn TableProvider>,
}

impl TableFunctionImpl for StaleFilesUdtf {
    fn call(&self, args: &[Expr]) -> Result<Arc<dyn TableProvider>> {
        use datafusion::prelude::*;
        use datafusion_functions_aggregate::min_max::max;

        let dependencies = provider_as_source(self.mv_dependencies.call(args)?);

        let table_name = get_table_name(args)?;

        let table_ref = TableReference::from(table_name).resolve(
            &self.mv_dependencies.config_options.catalog.default_catalog,
            &self.mv_dependencies.config_options.catalog.default_schema,
        );

        let logical_plan =
            LogicalPlanBuilder::scan_with_filters("dependencies", dependencies, None, vec![])?
                .aggregate(
                    vec![col("dependencies.target").alias("expected_target")],
                    vec![max(col("source_last_modified")).alias("sources_last_modified")],
                )?
                .join(
                    LogicalPlanBuilder::scan_with_filters(
                        "file_metadata",
                        provider_as_source(
                            Arc::clone(&self.file_metadata) as Arc<dyn TableProvider>
                        ),
                        None,
                        vec![
                            col("table_catalog").eq(lit(table_ref.catalog.as_ref())),
                            col("table_schema").eq(lit(table_ref.schema.as_ref())),
                            col("table_name").eq(lit(table_ref.table.as_ref())),
                        ],
                    )?
                    .aggregate(
                        vec![
                            // Trim the final path element
                            regexp_replace(col("file_path"), lit(r"/[^/]*$"), lit("/"), None)
                                .alias("existing_target"),
                        ],
                        vec![max(col("last_modified")).alias("target_last_modified")],
                    )?
                    .project(vec![col("existing_target"), col("target_last_modified")])?
                    .build()?,
                    JoinType::Left,
                    (vec!["expected_target"], vec!["existing_target"]),
                    None,
                )?
                .project(vec![
                    col("expected_target").alias("target"),
                    col("target_last_modified"),
                    col("sources_last_modified"),
                    nvl(
                        col("target_last_modified"),
                        lit(ScalarValue::TimestampNanosecond(
                            Some(0),
                            Some(Arc::from("UTC")),
                        )),
                    )
                    .lt(col("sources_last_modified"))
                    .alias("is_stale"),
                ])?
                .build()?;

        Ok(Arc::new(ViewTable::new(logical_plan, None)))
    }
}

/// Extract table name from args passed to TableFunctionImpl::call()
fn get_table_name(args: &[Expr]) -> Result<&String> {
    match &args[0] {
        Expr::Literal(ScalarValue::Utf8(Some(table_name)), _) => Ok(table_name),
        _ => Err(DataFusionError::Plan(
            "expected a single string literal argument to mv_dependencies".to_string(),
        )),
    }
}

/// Returns a logical plan that, when executed, lists expected build targets
/// for this materialized view, together with the dependencies for each target.
pub fn mv_dependencies_plan(
    materialized_view: &dyn Materialized,
    row_metadata_registry: &RowMetadataRegistry,
    config_options: &ConfigOptions,
) -> Result<LogicalPlan> {
    use datafusion_expr::logical_plan::*;

    let plan = materialized_view.query().clone();

    let partition_cols = materialized_view.partition_columns();
    let partition_col_indices = plan
        .schema()
        .fields()
        .iter()
        .enumerate()
        .filter_map(|(i, f)| partition_cols.contains(f.name()).then_some(i))
        .collect();

    let pruned_plan_with_source_files = if partition_cols.is_empty() {
        get_source_files_all_partitions(
            materialized_view,
            &config_options.catalog,
            row_metadata_registry,
        )
    } else {
        // Prune non-partition columns from all table scans
        let pruned_plan = pushdown_projection_inexact(plan, &partition_col_indices)?;

        // Now bubble up file metadata to the top of the plan
        push_up_file_metadata(pruned_plan, &config_options.catalog, row_metadata_registry)
    }?;

    // We now have data in the following form:
    // (partition_col0, partition_col1, ..., __meta)
    // The last column is a list of structs containing the row metadata
    // We need to unnest it

    // Find the single column with the name '__meta'
    let files = pruned_plan_with_source_files
        .schema()
        .columns()
        .into_iter()
        .find(|c| c.name.starts_with(META_COLUMN))
        .ok_or_else(|| DataFusionError::Plan(format!("Plan contains no {META_COLUMN} column")))?;
    let files_col = Expr::Column(files.clone());

    LogicalPlanBuilder::from(pruned_plan_with_source_files)
        .unnest_column(files)?
        .project(vec![
            construct_target_path_from_partition_columns(materialized_view).alias("target"),
            get_field(files_col.clone(), "table_catalog").alias("source_table_catalog"),
            get_field(files_col.clone(), "table_schema").alias("source_table_schema"),
            get_field(files_col.clone(), "table_name").alias("source_table_name"),
            get_field(files_col.clone(), "source_uri").alias("source_uri"),
            get_field(files_col.clone(), "last_modified").alias("source_last_modified"),
        ])?
        .distinct()?
        .build()
}

fn construct_target_path_from_partition_columns(materialized_view: &dyn Materialized) -> Expr {
    let table_path = lit(materialized_view.table_paths()[0]
        .as_str()
        // Trim the / (we'll add it back later if we need it)
        .trim_end_matches("/"));
    // Construct the paths for the build targets
    let mut hive_column_path_elements = materialized_view
        .partition_columns()
        .iter()
        .map(|column_name| concat([lit(column_name.as_str()), lit("="), col(column_name)].to_vec()))
        .collect::<Vec<_>>();
    hive_column_path_elements.insert(0, table_path);

    concat(vec![
        // concat_ws doesn't work if there are < 2 elements to concat
        if hive_column_path_elements.len() == 1 {
            hive_column_path_elements.pop().unwrap()
        } else {
            concat_ws(lit("/"), hive_column_path_elements)
        },
        // Always need a trailing slash on directory paths
        lit("/"),
    ])
}

/// An implementation of "inexact" projection pushdown that eliminates aggregations, windows, sorts, & limits.
/// Does not preserve order or row multiplicity and may return rows outside of the original projection.
/// However, it has the following property:
/// Let P be a projection operator.
/// If A is the original plan and A' is the result of "inexact" projection pushdown, we have PA ⊆ A'.
///
/// The purpose is to be as aggressive as possible with projection pushdown at the sacrifice of exactness.
fn pushdown_projection_inexact(plan: LogicalPlan, indices: &HashSet<usize>) -> Result<LogicalPlan> {
    use datafusion_expr::logical_plan::*;

    let plan_formatted = format!("{}", plan.display());
    match plan {
        LogicalPlan::Projection(Projection { expr, input, .. }) => {
            let new_exprs = expr
                .into_iter()
                .enumerate()
                .filter_map(|(i, expr)| indices.contains(&i).then_some(expr))
                .collect_vec();

            let child_indices = new_exprs
                .iter()
                .flat_map(|e| e.column_refs().into_iter())
                .map(|c| input.schema().index_of_column(c).unwrap())
                .collect::<HashSet<_>>();

            Projection::try_new(
                new_exprs,
                pushdown_projection_inexact(Arc::unwrap_or_clone(input), &child_indices)
                    .map(Arc::new)?,
            )
            .map(LogicalPlan::Projection)
        }
        LogicalPlan::Filter(ref filter) => {
            let mut indices = indices.clone();

            let new_filter = widen_filter(&filter.predicate, &mut indices, &plan)?;

            let filter = match plan {
                LogicalPlan::Filter(filter) => filter,
                _ => unreachable!(),
            };

            Filter::try_new(
                new_filter,
                pushdown_projection_inexact(Arc::unwrap_or_clone(filter.input), &indices)
                    .map(Arc::new)?,
            )
            .map(LogicalPlan::Filter)
        }
        LogicalPlan::Window(Window {
            input,
            window_expr: _,
            ..
        }) => {
            // Window nodes take their input and append window expressions to the end.
            // If our projection doesn't include window expressions, we can just turn
            // the window into a regular projection.
            let num_non_window_cols = input.schema().fields().len();
            if indices.iter().any(|&i| i >= num_non_window_cols) {
                return internal_err!("Can't push down projection through window functions");
            }

            pushdown_projection_inexact(Arc::unwrap_or_clone(input), indices)
        }
        LogicalPlan::Aggregate(Aggregate {
            input, group_expr, ..
        }) => {
            // Aggregate node schemas are the GROUP BY expressions followed by the aggregate expressions.
            let num_group_exprs = group_expr.len();
            if indices.iter().any(|&i| i >= num_group_exprs) {
                return internal_err!("Can't push down projection through aggregate functions");
            }

            let new_exprs = group_expr
                .into_iter()
                .enumerate()
                .filter_map(|(i, expr)| indices.contains(&i).then_some(expr))
                .collect_vec();

            let child_indices = new_exprs
                .iter()
                .flat_map(|e| e.column_refs().into_iter())
                .map(|c| input.schema().index_of_column(c).unwrap())
                .collect::<HashSet<_>>();

            Projection::try_new(
                new_exprs,
                pushdown_projection_inexact(Arc::unwrap_or_clone(input), &child_indices)
                    .map(Arc::new)?,
            )
            .map(LogicalPlan::Projection)
        }
        LogicalPlan::Join(ref join) => {
            let join_type = join.join_type;
            match join_type {
                JoinType::Inner | JoinType::Left | JoinType::Right | JoinType::Full => {}
                _ => {
                    return Err(DataFusionError::Internal(format!(
                        "unsupported join type: {join_type}"
                    )))
                }
            };

            let mut indices = indices.clone();

            // Relax the filter so that it can be computed from the
            // "pruned" children
            let filter = join
                .filter
                .as_ref()
                .map(|f| widen_filter(f, &mut indices, &plan))
                .transpose()?;

            let (mut left_child_indices, mut right_child_indices) =
                indices.iter().partition_map(|&i| {
                    if i < join.left.schema().fields().len() {
                        Either::Left(i)
                    } else {
                        Either::Right(i - join.left.schema().fields().len())
                    }
                });

            let on = join.on.iter().try_fold(vec![], |mut v, (lexpr, rexpr)| {
                // The ON clause includes filters like `lexpr = rexpr`
                // If either side is considered 'relevant', we include it.
                // See documentation for [`expr_is_relevant`].
                if expr_is_relevant(lexpr, &left_child_indices, &join.left)?
                    || expr_is_relevant(rexpr, &right_child_indices, &join.right)?
                {
                    add_all_columns_to_indices(lexpr, &mut left_child_indices, &join.left)?;
                    add_all_columns_to_indices(rexpr, &mut right_child_indices, &join.right)?;
                    v.push((lexpr.clone(), rexpr.clone()))
                }

                Ok::<_, DataFusionError>(v)
            })?;

            let join = match plan {
                LogicalPlan::Join(join) => join,
                _ => unreachable!(),
            };

            let left =
                pushdown_projection_inexact(Arc::unwrap_or_clone(join.left), &left_child_indices)
                    .map(Arc::new)?;
            let right =
                pushdown_projection_inexact(Arc::unwrap_or_clone(join.right), &right_child_indices)
                    .map(Arc::new)?;

            let schema = project_dfschema(join.schema.as_ref(), &indices).map(Arc::new)?;

            Ok(LogicalPlan::Join(Join {
                left,
                right,
                on,
                filter,
                join_type,
                schema,
                ..join
            }))
        }
        LogicalPlan::Union(Union { inputs, schema, .. }) => {
            let inputs = inputs
                .into_iter()
                .map(Arc::unwrap_or_clone)
                .map(|plan| pushdown_projection_inexact(plan, indices))
                .map_ok(Arc::new)
                .collect::<Result<Vec<_>>>()?;

            Ok(LogicalPlan::Union(Union {
                inputs,
                schema: project_dfschema(schema.as_ref(), indices).map(Arc::new)?,
            }))
        }
        LogicalPlan::TableScan(ref scan) => {
            let mut indices = indices.clone();
            let filters = scan
                .filters
                .iter()
                .map(|f| widen_filter(f, &mut indices, &plan))
                .collect::<Result<Vec<_>>>()?;

            let new_projection = scan
                .projection
                .clone()
                .unwrap_or((0..scan.source.schema().fields().len()).collect())
                .into_iter()
                .enumerate()
                .filter_map(|(i, j)| indices.contains(&i).then_some(j))
                .collect_vec();

            let scan = match plan {
                LogicalPlan::TableScan(scan) => scan,
                _ => unreachable!(),
            };

            TableScan::try_new(
                scan.table_name,
                scan.source,
                Some(new_projection),
                filters,
                None,
            )
            .map(LogicalPlan::TableScan)
        }
        LogicalPlan::EmptyRelation(EmptyRelation {
            produce_one_row,
            schema,
        }) => Ok(LogicalPlan::EmptyRelation(EmptyRelation {
            produce_one_row,
            schema: project_dfschema(schema.as_ref(), indices).map(Arc::new)?,
        })),
        LogicalPlan::SubqueryAlias(SubqueryAlias { input, alias, .. }) => SubqueryAlias::try_new(
            pushdown_projection_inexact(Arc::unwrap_or_clone(input), indices).map(Arc::new)?,
            alias,
        )
        .map(LogicalPlan::SubqueryAlias),
        LogicalPlan::Limit(Limit { input, .. }) | LogicalPlan::Sort(Sort { input, .. }) => {
            // Ignore sorts/limits entirely and remove them from the plan
            pushdown_projection_inexact(Arc::unwrap_or_clone(input), indices)
        }
        LogicalPlan::Values(Values { schema, values }) => {
            let schema = project_dfschema(&schema, indices).map(Arc::new)?;
            let values = values
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .enumerate()
                        .filter_map(|(i, v)| indices.contains(&i).then_some(v))
                        .collect_vec()
                })
                .collect_vec();

            Ok(LogicalPlan::Values(Values { schema, values }))
        }
        LogicalPlan::Distinct(Distinct::All(input)) => {
            pushdown_projection_inexact(Arc::unwrap_or_clone(input), indices)
                .map(Arc::new)
                .map(Distinct::All)
                .map(LogicalPlan::Distinct)
        }
        LogicalPlan::Unnest(unnest) => {
            // Map parent indices to child indices.
            // The columns of an unnest node have a many-to-one relation
            // to the columns of the input.
            let child_indices = indices
                .iter()
                .map(|&i| unnest.dependency_indices[i])
                .collect::<HashSet<_>>();

            let input_using_columns = unnest.input.using_columns()?;
            let input_schema = unnest.input.schema();
            let columns_to_unnest =
                unnest
                    .exec_columns
                    .into_iter()
                    .try_fold(vec![], |mut v, c| {
                        let c = c.normalize_with_schemas_and_ambiguity_check(
                            &[&[input_schema.as_ref()]],
                            &input_using_columns,
                        )?;
                        let idx = input_schema.index_of_column(&c)?;
                        if child_indices.contains(&idx) {
                            v.push(c);
                        }

                        Ok::<_, DataFusionError>(v)
                    })?;

            let columns_to_project = unnest
                .schema
                .columns()
                .into_iter()
                .enumerate()
                .filter_map(|(i, c)| indices.contains(&i).then_some(c))
                .map(Expr::Column)
                .collect_vec();

            LogicalPlanBuilder::from(pushdown_projection_inexact(
                Arc::unwrap_or_clone(unnest.input),
                &child_indices,
            )?)
            .unnest_columns_with_options(columns_to_unnest, unnest.options)?
            .project(columns_to_project)?
            .build()
        }

        _ => internal_err!("Unsupported logical plan node: {}", plan.display()),
    }
    .map_err(|e| e.context(format!("plan: \n{plan_formatted}")))
}

/// 'Widen' a filter, i.e. given a predicate P,
/// compute P' such that P' is true whenever P is.
/// In particular, P' should be computed using columns whose indices are in `indices`.
///
/// # Mutating `indices`
///
/// Currently under some conditions this function will add new entries to `indices`.
/// This is particularly important in some cases involving joins. For example,
/// consider the following plan:
///
/// ```ignore
/// Projection: t2.year, t2.month, t2.day, t2.feed, t2.column2, t3.column1
///  Inner Join: Using t2.year = t3.year
///  TableScan: t2
///  TableScan: t3
/// ```
///
/// If we want to prune all parts of the plan not related to t2.year, we'd get something like this:
///
/// ```ignore
/// Projection: t2.year
///  Inner Join: Using
///  TableScan: t2 projection=[year]
///  TableScan: t3 projection=[]
/// ```
///
/// Notice that the filter in the inner join is gone. This is because `t3.year` is not obviously referenced in the definition of `t2.year`;
/// it is only implicitly used in the join filter.
///
/// To get around this, we look at filter expressions, and if they contain a _single_ column in the index set,
/// we add the rest of the columns from the filter to the index set, to ensure all of the filter's inputs
/// will be present.
fn widen_filter(
    predicate: &Expr,
    indices: &mut HashSet<usize>,
    parent: &LogicalPlan,
) -> Result<Expr> {
    let conjunctions = split_conjunction(predicate);

    conjunctions.into_iter().try_fold(lit(true), |a, b| {
        Ok(if expr_is_relevant(b, indices, parent)? {
            add_all_columns_to_indices(b, indices, parent)?;
            a.and(b.clone())
        } else {
            a
        })
    })
}

/// An expression is considered 'relevant' if a single column is inside our index set.
fn expr_is_relevant(expr: &Expr, indices: &HashSet<usize>, parent: &LogicalPlan) -> Result<bool> {
    let schemas = parent
        .inputs()
        .iter()
        .map(|input| input.schema().as_ref())
        .collect_vec();
    let using_columns = parent.using_columns()?;

    for c in expr.column_refs() {
        let normalized_column = c
            .clone()
            .normalize_with_schemas_and_ambiguity_check(&[&schemas], &using_columns)?;
        let column_idx = parent.schema().index_of_column(&normalized_column)?;

        if indices.contains(&column_idx) {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Get all referenced columns in the expression,
/// and add them to the index set.
fn add_all_columns_to_indices(
    expr: &Expr,
    indices: &mut HashSet<usize>,
    parent: &LogicalPlan,
) -> Result<()> {
    let schemas = parent
        .inputs()
        .iter()
        .map(|input| input.schema().as_ref())
        .collect_vec();
    let using_columns = parent.using_columns()?;

    for c in expr.column_refs() {
        let normalized_column = c
            .clone()
            .normalize_with_schemas_and_ambiguity_check(&[&schemas], &using_columns)?;
        let column_idx = parent.schema().index_of_column(&normalized_column)?;

        indices.insert(column_idx);
    }

    Ok(())
}

fn project_dfschema(schema: &DFSchema, indices: &HashSet<usize>) -> Result<DFSchema> {
    let qualified_fields = (0..schema.fields().len())
        .filter_map(|i| {
            indices.contains(&i).then_some({
                let (reference, field) = schema.qualified_field(i);
                (reference.cloned(), Arc::new(field.clone()))
            })
        })
        .collect_vec();

    // todo: handle functional dependencies
    DFSchema::new_with_metadata(qualified_fields, schema.metadata().clone())
}

/// Rewrite TableScans on top of the file metadata table,
/// assuming the query only uses the S3 partition columns.
/// Then push up the file metadata to the output of this plan.
///
/// The result will have a single new column with an autogenerated name "__meta_<id>"
/// which contains the source file metadata for a given row in the output.
fn push_up_file_metadata(
    plan: LogicalPlan,
    catalog_options: &CatalogOptions,
    row_metadata_registry: &RowMetadataRegistry,
) -> Result<LogicalPlan> {
    let alias_generator = AliasGenerator::new();
    plan.transform_up(|plan| {
        match plan {
            LogicalPlan::TableScan(scan) => {
                scan_columns_from_row_metadata(scan, catalog_options, row_metadata_registry)
            }
            plan => project_row_metadata_from_input(plan, &alias_generator),
        }
        .and_then(LogicalPlan::recompute_schema)
        .map(Transformed::yes)
    })
    .map(|t| t.data)
}

/// Assuming the input has any columns of the form "__meta_<id>",
/// push up the file columns through the output of this LogicalPlan node.
/// The output will have a single new column of the form "__meta_<id>".
fn project_row_metadata_from_input(
    plan: LogicalPlan,
    alias_generator: &AliasGenerator,
) -> Result<LogicalPlan> {
    use datafusion_expr::logical_plan::*;

    // find all file metadata columns and collapse them into one concatenated list
    match plan {
        LogicalPlan::Projection(Projection { expr, input, .. }) => {
            let file_md_columns = input
                .schema()
                .columns()
                .into_iter()
                .filter_map(|c| c.name.starts_with(META_COLUMN).then_some(Expr::Column(c)))
                .collect_vec();
            Projection::try_new(
                expr.into_iter()
                    .chain(Some(
                        flatten(make_array(file_md_columns))
                            .alias(alias_generator.next(META_COLUMN)),
                    ))
                    .collect_vec(),
                input,
            )
            .map(LogicalPlan::Projection)
        }
        _ => {
            let plan = plan.recompute_schema()?;
            let (file_md_columns, original_columns) = plan
                .schema()
                .columns()
                .into_iter()
                .partition::<Vec<_>, _>(|c| c.name.starts_with(META_COLUMN));

            Projection::try_new(
                original_columns
                    .into_iter()
                    .map(Expr::Column)
                    .chain(Some(
                        flatten(make_array(
                            file_md_columns.into_iter().map(Expr::Column).collect_vec(),
                        ))
                        .alias(alias_generator.next(META_COLUMN)),
                    ))
                    .collect_vec(),
                Arc::new(plan),
            )
            .map(LogicalPlan::Projection)
        }
    }
}

/// Turn a TableScan into an equivalent scan on the row metadata source,
/// assuming that every column in the table scan is a partition column;
/// also adds a new column to the TableScan, "__meta"
/// which is a List of Struct column including the row metadata.
fn scan_columns_from_row_metadata(
    scan: TableScan,
    catalog_options: &CatalogOptions,
    row_metadata_registry: &RowMetadataRegistry,
) -> Result<LogicalPlan> {
    let table_ref = scan.table_name.clone().resolve(
        &catalog_options.default_catalog,
        &catalog_options.default_schema,
    );

    let source = row_metadata_registry.get_source(&table_ref)?;

    // [`RowMetadataSource`] returns a Struct,
    // but the MV algorithm expects a list of structs at each node in the plan.
    let mut exprs = scan
        .projected_schema
        .fields()
        .iter()
        .map(|f| col((None, f)))
        .collect_vec();
    exprs.push(make_array(vec![col(META_COLUMN)]).alias(META_COLUMN));

    source
        .row_metadata(table_ref, &scan)?
        .project(exprs)?
        .alias(scan.table_name.clone())?
        .filter(
            scan.filters
                .clone()
                .into_iter()
                .fold(lit(true), |a, b| a.and(b)),
        )?
        .build()
}

/// Assemble sources irrespective of partitions
/// This is more efficient when the materialized view has no partitions,
/// but less intelligent -- it may return additional dependencies not present in the
/// usual algorithm.
//
// TODO: see if we can optimize the normal logic for no partitions.
// It seems that joins get transformed into cross joins, which can become extremely inefficient.
// Hence we had to implement this alternate, simpler but less precise algorithm.
// Notably, it may include more false positives.
fn get_source_files_all_partitions(
    materialized_view: &dyn Materialized,
    catalog_options: &CatalogOptions,
    row_metadata_registry: &RowMetadataRegistry,
) -> Result<LogicalPlan> {
    use datafusion_common::tree_node::TreeNodeRecursion;

    let mut tables = std::collections::HashMap::<TableReference, _>::new();

    materialized_view
        .query()
        .apply(|plan| {
            if let LogicalPlan::TableScan(scan) = plan {
                tables.insert(scan.table_name.clone(), Arc::clone(&scan.source));
            }

            Ok(TreeNodeRecursion::Continue)
        })
        .unwrap();

    tables
        .into_iter()
        .try_fold(
            None::<LogicalPlanBuilder>,
            |maybe_plan, (table_ref, source)| {
                let resolved_ref = table_ref.clone().resolve(
                    &catalog_options.default_catalog,
                    &catalog_options.default_schema,
                );

                let row_metadata = row_metadata_registry.get_source(&resolved_ref)?;
                let row_metadata_scan = row_metadata
                    .row_metadata(
                        resolved_ref,
                        &TableScan {
                            table_name: table_ref.clone(),
                            source,
                            projection: Some(vec![]), // no columns relevant
                            projected_schema: Arc::new(DFSchema::empty()),
                            filters: vec![],
                            fetch: None,
                        },
                    )?
                    .build()?;

                if let Some(previous) = maybe_plan {
                    previous.union(row_metadata_scan)
                } else {
                    Ok(LogicalPlanBuilder::from(row_metadata_scan))
                }
                .map(Some)
            },
        )?
        .ok_or_else(|| DataFusionError::Plan("materialized view has no source tables".into()))?
        // [`RowMetadataSource`] returns a Struct,
        // but the MV algorithm expects a list of structs at each node in the plan.
        .project(vec![make_array(vec![col(META_COLUMN)]).alias(META_COLUMN)])?
        .build()
}

#[cfg(test)]
mod test {
    use std::{any::Any, collections::HashSet, sync::Arc};

    use arrow::util::pretty::pretty_format_batches;
    use arrow_schema::SchemaRef;
    use datafusion::{
        assert_batches_eq, assert_batches_sorted_eq,
        catalog::{Session, TableProvider},
        datasource::listing::ListingTableUrl,
        execution::session_state::SessionStateBuilder,
        prelude::{DataFrame, SessionConfig, SessionContext},
    };
    use datafusion_common::{Column, Result, ScalarValue};
    use datafusion_expr::{Expr, JoinType, LogicalPlan, TableType};
    use datafusion_physical_plan::ExecutionPlan;
    use itertools::Itertools;

    use crate::materialized::{
        dependencies::pushdown_projection_inexact,
        register_decorator, register_materialized,
        row_metadata::{ObjectStoreRowMetadataSource, RowMetadataRegistry},
        Decorator, ListingTableLike, Materialized,
    };

    use super::{mv_dependencies, stale_files};

    /// A mock materialized view.
    #[derive(Debug)]
    struct MockMaterializedView {
        table_path: ListingTableUrl,
        partition_columns: Vec<String>,
        query: LogicalPlan,
        file_ext: &'static str,
    }

    #[async_trait::async_trait]
    impl TableProvider for MockMaterializedView {
        fn as_any(&self) -> &dyn Any {
            self
        }

        fn schema(&self) -> SchemaRef {
            Arc::new(self.query.schema().as_arrow().clone())
        }

        fn table_type(&self) -> TableType {
            TableType::Base
        }

        async fn scan(
            &self,
            _state: &dyn Session,
            _projection: Option<&Vec<usize>>,
            _filters: &[Expr],
            _limit: Option<usize>,
        ) -> Result<Arc<dyn ExecutionPlan>> {
            unimplemented!()
        }
    }

    impl ListingTableLike for MockMaterializedView {
        fn table_paths(&self) -> Vec<ListingTableUrl> {
            vec![self.table_path.clone()]
        }

        fn partition_columns(&self) -> Vec<String> {
            self.partition_columns.clone()
        }

        fn file_ext(&self) -> String {
            self.file_ext.to_string()
        }
    }

    impl Materialized for MockMaterializedView {
        fn query(&self) -> LogicalPlan {
            self.query.clone()
        }
    }

    #[derive(Debug)]
    struct DecoratorTable {
        inner: Arc<dyn TableProvider>,
    }

    #[async_trait::async_trait]
    impl TableProvider for DecoratorTable {
        fn as_any(&self) -> &dyn Any {
            self
        }

        fn schema(&self) -> SchemaRef {
            self.inner.schema()
        }

        fn table_type(&self) -> TableType {
            self.inner.table_type()
        }

        async fn scan(
            &self,
            state: &dyn Session,
            projection: Option<&Vec<usize>>,
            filters: &[Expr],
            limit: Option<usize>,
        ) -> Result<Arc<dyn ExecutionPlan>> {
            self.inner.scan(state, projection, filters, limit).await
        }
    }

    impl Decorator for DecoratorTable {
        fn base(&self) -> &dyn TableProvider {
            self.inner.as_ref()
        }
    }

    async fn setup() -> Result<SessionContext> {
        let _ = env_logger::builder().is_test(true).try_init();

        register_materialized::<MockMaterializedView>();
        register_decorator::<DecoratorTable>();

        let state = SessionStateBuilder::new()
            .with_default_features()
            .with_config(
                SessionConfig::new()
                    .with_default_catalog_and_schema("datafusion", "test")
                    .set(
                        "datafusion.explain.logical_plan_only",
                        &ScalarValue::Boolean(Some(true)),
                    )
                    .set(
                        "datafusion.sql_parser.dialect",
                        &ScalarValue::Utf8(Some("duckdb".into())),
                    )
                    .set(
                        // See discussion in this issue:
                        // https://github.com/apache/datafusion/issues/13065
                        "datafusion.execution.skip_physical_aggregate_schema_check",
                        &ScalarValue::Boolean(Some(true)),
                    ),
            )
            .build();

        let ctx = SessionContext::new_with_state(state);

        ctx.sql(
            "CREATE TABLE t1 AS VALUES
            ('2021', 3, 'A'),
            ('2022', 4, 'B'),
            ('2023', 5, 'C')",
        )
        .await?
        .collect()
        .await?;

        ctx.sql(
            "CREATE TABLE t2 (
                year STRING,
                month STRING,
                day STRING,
                feed CHAR,
                column2 INTEGER
            ) AS VALUES
            ('2023', '01', '01', 'A', 1),
            ('2023', '01', '02', 'B', 2),
            ('2023', '01', '03', 'C', 3),
            ('2024', '12', '04', 'X', 4),
            ('2024', '12', '05', 'Y', 5),
            ('2024', '12', '06', 'Z', 6)",
        )
        .await?
        .collect()
        .await?;

        ctx.sql(
            "CREATE TABLE t3 (
                year STRING,
                column1 INTEGER
            ) AS VALUES
            (2023, 1),
            (2024, 2)",
        )
        .await?
        .collect()
        .await?;

        ctx.sql(
            // create a fake file metadata table to use as a mock
            "CREATE TABLE file_metadata (
                table_catalog STRING,
                table_schema STRING,
                table_name STRING,
                file_path STRING,
                last_modified TIMESTAMP,
                size BIGINT UNSIGNED
            ) AS VALUES
                ('datafusion', 'test', 't1', 's3://t1/column1=2021/data.01.parquet', '2023-07-11T16:29:26Z', 0),
                ('datafusion', 'test', 't1', 's3://t1/column1=2022/data.01.parquet', '2023-07-11T16:45:22Z', 0),
                ('datafusion', 'test', 't1', 's3://t1/column1=2023/data.01.parquet', '2023-07-11T16:45:44Z', 0),
                ('datafusion', 'test', 't2', 's3://t2/year=2023/month=01/day=01/feed=A/data.01.parquet', '2023-07-11T16:29:26Z', 0),
                ('datafusion', 'test', 't2', 's3://t2/year=2023/month=01/day=02/feed=B/data.01.parquet', '2023-07-11T16:45:22Z', 0),
                ('datafusion', 'test', 't2', 's3://t2/year=2023/month=01/day=03/feed=C/data.01.parquet', '2023-07-11T16:45:44Z', 0),
                ('datafusion', 'test', 't2', 's3://t2/year=2024/month=12/day=04/feed=X/data.01.parquet', '2023-07-11T16:29:26Z', 0),
                ('datafusion', 'test', 't2', 's3://t2/year=2024/month=12/day=05/feed=Y/data.01.parquet', '2023-07-11T16:45:22Z', 0),
                ('datafusion', 'test', 't2', 's3://t2/year=2024/month=12/day=06/feed=Z/data.01.parquet', '2023-07-11T16:45:44Z', 0),
                ('datafusion', 'test', 't3', 's3://t3/year=2023/data.01.parquet', '2023-07-11T16:45:44Z', 0),
                ('datafusion', 'test', 't3', 's3://t3/year=2024/data.01.parquet', '2023-07-11T16:45:44Z', 0)
            "
        )
        .await?
        .collect()
        .await?;

        let metadata_table = ctx.table_provider("file_metadata").await?;
        let object_store_metadata_source = Arc::new(
            ObjectStoreRowMetadataSource::with_file_metadata(Arc::clone(&metadata_table)),
        );

        let row_metadata_registry = Arc::new(RowMetadataRegistry::new_with_default_source(
            object_store_metadata_source,
        ));

        ctx.register_udtf(
            "mv_dependencies",
            mv_dependencies(
                Arc::clone(ctx.state().catalog_list()),
                row_metadata_registry.clone(),
                ctx.copied_config().options(),
            ),
        );

        ctx.register_udtf(
            "stale_files",
            stale_files(
                Arc::clone(ctx.state().catalog_list()),
                Arc::clone(&row_metadata_registry),
                metadata_table,
                ctx.copied_config().options(),
            ),
        );

        Ok(ctx)
    }

    #[tokio::test]
    async fn test_deps() {
        struct TestCase {
            name: &'static str,
            query_to_analyze: &'static str,
            table_name: &'static str,
            table_path: ListingTableUrl,
            partition_cols: Vec<&'static str>,
            file_extension: &'static str,
            expected_output: Vec<&'static str>,
            file_metadata: &'static str,
            expected_stale_files_output: Vec<&'static str>,
        }

        let cases = &[
            TestCase {
                name: "un-transformed partition column",
                query_to_analyze:
                    "SELECT column1 AS partition_column, concat(column2, column3) AS some_value FROM t1",
                table_name: "m1",
                table_path: ListingTableUrl::parse("s3://m1/").unwrap(),
                partition_cols: vec!["partition_column"],
                file_extension: ".parquet",
                expected_output: vec![
                    "+--------------------------------+----------------------+---------------------+-------------------+--------------------------------------+----------------------+",
                    "| target                         | source_table_catalog | source_table_schema | source_table_name | source_uri                           | source_last_modified |",
                    "+--------------------------------+----------------------+---------------------+-------------------+--------------------------------------+----------------------+",
                    "| s3://m1/partition_column=2021/ | datafusion           | test                | t1                | s3://t1/column1=2021/data.01.parquet | 2023-07-11T16:29:26  |",
                    "| s3://m1/partition_column=2022/ | datafusion           | test                | t1                | s3://t1/column1=2022/data.01.parquet | 2023-07-11T16:45:22  |",
                    "| s3://m1/partition_column=2023/ | datafusion           | test                | t1                | s3://t1/column1=2023/data.01.parquet | 2023-07-11T16:45:44  |",
                    "+--------------------------------+----------------------+---------------------+-------------------+--------------------------------------+----------------------+",
                ],
                // second file is old
                file_metadata: "
                    ('datafusion', 'test', 'm1', 's3://m1/partition_column=2021/data.01.parquet', '2023-07-12T16:00:00Z', 0),
                    ('datafusion', 'test', 'm1', 's3://m1/partition_column=2022/data.01.parquet', '2023-07-10T16:00:00Z', 0),
                    ('datafusion', 'test', 'm1', 's3://m1/partition_column=2023/data.01.parquet', '2023-07-12T16:00:00Z', 0)
                ",
                expected_stale_files_output: vec![
                    "+--------------------------------+----------------------+-----------------------+----------+",
                    "| target                         | target_last_modified | sources_last_modified | is_stale |",
                    "+--------------------------------+----------------------+-----------------------+----------+",
                    "| s3://m1/partition_column=2021/ | 2023-07-12T16:00:00  | 2023-07-11T16:29:26   | false    |",
                    "| s3://m1/partition_column=2022/ | 2023-07-10T16:00:00  | 2023-07-11T16:45:22   | true     |",
                    "| s3://m1/partition_column=2023/ | 2023-07-12T16:00:00  | 2023-07-11T16:45:44   | false    |",
                    "+--------------------------------+----------------------+-----------------------+----------+",
                ],
            },
            TestCase {
                name: "transform year/month/day partition into timestamp partition",
                query_to_analyze: "
                SELECT DISTINCT
                    to_timestamp_nanos(concat_ws('-', year, month, day)) AS timestamp,
                    feed
                FROM t2",
                table_name: "m2",
                table_path: ListingTableUrl::parse("s3://m2/").unwrap(),
                partition_cols: vec!["timestamp", "feed"],
                file_extension: ".parquet",
                expected_output: vec![
                    "+-----------------------------------------------+----------------------+---------------------+-------------------+----------------------------------------------------------+----------------------+",
                    "| target                                        | source_table_catalog | source_table_schema | source_table_name | source_uri                                               | source_last_modified |",
                    "+-----------------------------------------------+----------------------+---------------------+-------------------+----------------------------------------------------------+----------------------+",
                    "| s3://m2/timestamp=2023-01-01T00:00:00/feed=A/ | datafusion           | test                | t2                | s3://t2/year=2023/month=01/day=01/feed=A/data.01.parquet | 2023-07-11T16:29:26  |",
                    "| s3://m2/timestamp=2023-01-02T00:00:00/feed=B/ | datafusion           | test                | t2                | s3://t2/year=2023/month=01/day=02/feed=B/data.01.parquet | 2023-07-11T16:45:22  |",
                    "| s3://m2/timestamp=2023-01-03T00:00:00/feed=C/ | datafusion           | test                | t2                | s3://t2/year=2023/month=01/day=03/feed=C/data.01.parquet | 2023-07-11T16:45:44  |",
                    "| s3://m2/timestamp=2024-12-04T00:00:00/feed=X/ | datafusion           | test                | t2                | s3://t2/year=2024/month=12/day=04/feed=X/data.01.parquet | 2023-07-11T16:29:26  |",
                    "| s3://m2/timestamp=2024-12-05T00:00:00/feed=Y/ | datafusion           | test                | t2                | s3://t2/year=2024/month=12/day=05/feed=Y/data.01.parquet | 2023-07-11T16:45:22  |",
                    "| s3://m2/timestamp=2024-12-06T00:00:00/feed=Z/ | datafusion           | test                | t2                | s3://t2/year=2024/month=12/day=06/feed=Z/data.01.parquet | 2023-07-11T16:45:44  |",
                    "+-----------------------------------------------+----------------------+---------------------+-------------------+----------------------------------------------------------+----------------------+",
                ],
                file_metadata: "
                    ('datafusion', 'test', 'm2', 's3://m2/timestamp=2023-01-01T00:00:00/feed=A/data.01.parquet', '2023-07-12T16:00:00Z', 0),
                    ('datafusion', 'test', 'm2', 's3://m2/timestamp=2023-01-02T00:00:00/feed=B/data.01.parquet', '2023-07-12T16:00:00Z', 0),
                    ('datafusion', 'test', 'm2', 's3://m2/timestamp=2023-01-03T00:00:00/feed=C/data.01.parquet', '2023-07-10T16:00:00Z', 0),
                    ('datafusion', 'test', 'm2', 's3://m2/timestamp=2024-12-04T00:00:00/feed=X/data.01.parquet', '2023-07-12T16:00:00Z', 0),
                    ('datafusion', 'test', 'm2', 's3://m2/timestamp=2024-12-05T00:00:00/feed=Y/data.01.parquet', '2023-07-12T16:00:00Z', 0),
                    ('datafusion', 'test', 'm2', 's3://m2/timestamp=2024-12-06T00:00:00/feed=Z/data.01.parquet', '2023-07-10T16:00:00Z', 0)
                ",
                expected_stale_files_output: vec![
                    "+-----------------------------------------------+----------------------+-----------------------+----------+",
                    "| target                                        | target_last_modified | sources_last_modified | is_stale |",
                    "+-----------------------------------------------+----------------------+-----------------------+----------+",
                    "| s3://m2/timestamp=2023-01-01T00:00:00/feed=A/ | 2023-07-12T16:00:00  | 2023-07-11T16:29:26   | false    |",
                    "| s3://m2/timestamp=2023-01-02T00:00:00/feed=B/ | 2023-07-12T16:00:00  | 2023-07-11T16:45:22   | false    |",
                    "| s3://m2/timestamp=2023-01-03T00:00:00/feed=C/ | 2023-07-10T16:00:00  | 2023-07-11T16:45:44   | true     |",
                    "| s3://m2/timestamp=2024-12-04T00:00:00/feed=X/ | 2023-07-12T16:00:00  | 2023-07-11T16:29:26   | false    |",
                    "| s3://m2/timestamp=2024-12-05T00:00:00/feed=Y/ | 2023-07-12T16:00:00  | 2023-07-11T16:45:22   | false    |",
                    "| s3://m2/timestamp=2024-12-06T00:00:00/feed=Z/ | 2023-07-10T16:00:00  | 2023-07-11T16:45:44   | true     |",
                    "+-----------------------------------------------+----------------------+-----------------------+----------+",
                ],
            },
            TestCase {
                name: "materialized view has no partitions",
                query_to_analyze: "SELECT column1 AS output FROM t3",
                table_name: "m3",
                table_path: ListingTableUrl::parse("s3://m3/").unwrap(),
                partition_cols: vec![],
                file_extension: ".parquet",
                expected_output: vec![
                    "+----------+----------------------+---------------------+-------------------+-----------------------------------+----------------------+",
                    "| target   | source_table_catalog | source_table_schema | source_table_name | source_uri                        | source_last_modified |",
                    "+----------+----------------------+---------------------+-------------------+-----------------------------------+----------------------+",
                    "| s3://m3/ | datafusion           | test                | t3                | s3://t3/year=2023/data.01.parquet | 2023-07-11T16:45:44  |",
                    "| s3://m3/ | datafusion           | test                | t3                | s3://t3/year=2024/data.01.parquet | 2023-07-11T16:45:44  |",
                    "+----------+----------------------+---------------------+-------------------+-----------------------------------+----------------------+",
                ],
                file_metadata: "
                    ('datafusion', 'test', 'm3', 's3://m3/data.01.parquet', '2023-07-12T16:00:00Z', 0)
                ",
                expected_stale_files_output: vec![
                    "+----------+----------------------+-----------------------+----------+",
                    "| target   | target_last_modified | sources_last_modified | is_stale |",
                    "+----------+----------------------+-----------------------+----------+",
                    "| s3://m3/ | 2023-07-12T16:00:00  | 2023-07-11T16:45:44   | false    |",
                    "+----------+----------------------+-----------------------+----------+",
                ],
            },
            TestCase {
                name: "simple equijoin on year",
                query_to_analyze: "SELECT * FROM t2 INNER JOIN t3 USING (year)",
                table_name: "m4",
                table_path: ListingTableUrl::parse("s3://m4/").unwrap(),
                partition_cols: vec!["year"],
                file_extension: ".parquet",
                expected_output: vec![
                    "+--------------------+----------------------+---------------------+-------------------+----------------------------------------------------------+----------------------+",
                    "| target             | source_table_catalog | source_table_schema | source_table_name | source_uri                                               | source_last_modified |",
                    "+--------------------+----------------------+---------------------+-------------------+----------------------------------------------------------+----------------------+",
                    "| s3://m4/year=2023/ | datafusion           | test                | t2                | s3://t2/year=2023/month=01/day=01/feed=A/data.01.parquet | 2023-07-11T16:29:26  |",
                    "| s3://m4/year=2023/ | datafusion           | test                | t2                | s3://t2/year=2023/month=01/day=02/feed=B/data.01.parquet | 2023-07-11T16:45:22  |",
                    "| s3://m4/year=2023/ | datafusion           | test                | t2                | s3://t2/year=2023/month=01/day=03/feed=C/data.01.parquet | 2023-07-11T16:45:44  |",
                    "| s3://m4/year=2023/ | datafusion           | test                | t3                | s3://t3/year=2023/data.01.parquet                        | 2023-07-11T16:45:44  |",
                    "| s3://m4/year=2024/ | datafusion           | test                | t2                | s3://t2/year=2024/month=12/day=04/feed=X/data.01.parquet | 2023-07-11T16:29:26  |",
                    "| s3://m4/year=2024/ | datafusion           | test                | t2                | s3://t2/year=2024/month=12/day=05/feed=Y/data.01.parquet | 2023-07-11T16:45:22  |",
                    "| s3://m4/year=2024/ | datafusion           | test                | t2                | s3://t2/year=2024/month=12/day=06/feed=Z/data.01.parquet | 2023-07-11T16:45:44  |",
                    "| s3://m4/year=2024/ | datafusion           | test                | t3                | s3://t3/year=2024/data.01.parquet                        | 2023-07-11T16:45:44  |",
                    "+--------------------+----------------------+---------------------+-------------------+----------------------------------------------------------+----------------------+",
                ],
                file_metadata: "
                    ('datafusion', 'test', 'm4', 's3://m4/year=2023/data.01.parquet', '2023-07-12T16:00:00Z', 0),
                    ('datafusion', 'test', 'm4', 's3://m4/year=2024/data.01.parquet', '2023-07-12T16:00:00Z', 0)
                ",
                expected_stale_files_output: vec![
                    "+--------------------+----------------------+-----------------------+----------+",
                    "| target             | target_last_modified | sources_last_modified | is_stale |",
                    "+--------------------+----------------------+-----------------------+----------+",
                    "| s3://m4/year=2023/ | 2023-07-12T16:00:00  | 2023-07-11T16:45:44   | false    |",
                    "| s3://m4/year=2024/ | 2023-07-12T16:00:00  | 2023-07-11T16:45:44   | false    |",
                    "+--------------------+----------------------+-----------------------+----------+",
                ],
            },
            TestCase {
                name: "triangular join on year",
                query_to_analyze: "
                    SELECT
                        t2.*,
                        t3.* EXCLUDE(year),
                        t3.year AS \"t3.year\"
                    FROM t2
                    INNER JOIN t3
                    ON (t2.year <= t3.year)",
                table_name: "m4",
                table_path: ListingTableUrl::parse("s3://m4/").unwrap(),
                partition_cols: vec!["year"],
                file_extension: ".parquet",
                expected_output: vec![
                    "+--------------------+----------------------+---------------------+-------------------+----------------------------------------------------------+----------------------+",
                    "| target             | source_table_catalog | source_table_schema | source_table_name | source_uri                                               | source_last_modified |",
                    "+--------------------+----------------------+---------------------+-------------------+----------------------------------------------------------+----------------------+",
                    "| s3://m4/year=2023/ | datafusion           | test                | t2                | s3://t2/year=2023/month=01/day=01/feed=A/data.01.parquet | 2023-07-11T16:29:26  |",
                    "| s3://m4/year=2023/ | datafusion           | test                | t2                | s3://t2/year=2023/month=01/day=02/feed=B/data.01.parquet | 2023-07-11T16:45:22  |",
                    "| s3://m4/year=2023/ | datafusion           | test                | t2                | s3://t2/year=2023/month=01/day=03/feed=C/data.01.parquet | 2023-07-11T16:45:44  |",
                    "| s3://m4/year=2023/ | datafusion           | test                | t3                | s3://t3/year=2023/data.01.parquet                        | 2023-07-11T16:45:44  |",
                    "| s3://m4/year=2023/ | datafusion           | test                | t3                | s3://t3/year=2024/data.01.parquet                        | 2023-07-11T16:45:44  |",
                    "| s3://m4/year=2024/ | datafusion           | test                | t2                | s3://t2/year=2024/month=12/day=04/feed=X/data.01.parquet | 2023-07-11T16:29:26  |",
                    "| s3://m4/year=2024/ | datafusion           | test                | t2                | s3://t2/year=2024/month=12/day=05/feed=Y/data.01.parquet | 2023-07-11T16:45:22  |",
                    "| s3://m4/year=2024/ | datafusion           | test                | t2                | s3://t2/year=2024/month=12/day=06/feed=Z/data.01.parquet | 2023-07-11T16:45:44  |",
                    "| s3://m4/year=2024/ | datafusion           | test                | t3                | s3://t3/year=2024/data.01.parquet                        | 2023-07-11T16:45:44  |",
                    "+--------------------+----------------------+---------------------+-------------------+----------------------------------------------------------+----------------------+",
                ],
                file_metadata: "
                    ('datafusion', 'test', 'm4', 's3://m4/year=2023/data.01.parquet', '2023-07-12T16:00:00Z', 0),
                    ('datafusion', 'test', 'm4', 's3://m4/year=2024/data.01.parquet', '2023-07-12T16:00:00Z', 0)
                ",
                expected_stale_files_output: vec![
                    "+--------------------+----------------------+-----------------------+----------+",
                    "| target             | target_last_modified | sources_last_modified | is_stale |",
                    "+--------------------+----------------------+-----------------------+----------+",
                    "| s3://m4/year=2023/ | 2023-07-12T16:00:00  | 2023-07-11T16:45:44   | false    |",
                    "| s3://m4/year=2024/ | 2023-07-12T16:00:00  | 2023-07-11T16:45:44   | false    |",
                    "+--------------------+----------------------+-----------------------+----------+",
                ],
            },
            TestCase {
                name: "triangular left join, strict <",
                query_to_analyze: "
                    SELECT
                        t2.*,
                        t3.* EXCLUDE(year),
                        t3.year AS \"t3.year\"
                    FROM t2
                    LEFT JOIN t3
                    ON (t2.year < t3.year)",
                table_name: "m4",
                table_path: ListingTableUrl::parse("s3://m4/").unwrap(),
                partition_cols: vec!["year"],
                file_extension: ".parquet",
                expected_output: vec![
                    "+--------------------+----------------------+---------------------+-------------------+----------------------------------------------------------+----------------------+",
                    "| target             | source_table_catalog | source_table_schema | source_table_name | source_uri                                               | source_last_modified |",
                    "+--------------------+----------------------+---------------------+-------------------+----------------------------------------------------------+----------------------+",
                    "| s3://m4/year=2023/ | datafusion           | test                | t2                | s3://t2/year=2023/month=01/day=01/feed=A/data.01.parquet | 2023-07-11T16:29:26  |",
                    "| s3://m4/year=2023/ | datafusion           | test                | t2                | s3://t2/year=2023/month=01/day=02/feed=B/data.01.parquet | 2023-07-11T16:45:22  |",
                    "| s3://m4/year=2023/ | datafusion           | test                | t2                | s3://t2/year=2023/month=01/day=03/feed=C/data.01.parquet | 2023-07-11T16:45:44  |",
                    "| s3://m4/year=2023/ | datafusion           | test                | t3                | s3://t3/year=2024/data.01.parquet                        | 2023-07-11T16:45:44  |",
                    "| s3://m4/year=2024/ | datafusion           | test                | t2                | s3://t2/year=2024/month=12/day=04/feed=X/data.01.parquet | 2023-07-11T16:29:26  |",
                    "| s3://m4/year=2024/ | datafusion           | test                | t2                | s3://t2/year=2024/month=12/day=05/feed=Y/data.01.parquet | 2023-07-11T16:45:22  |",
                    "| s3://m4/year=2024/ | datafusion           | test                | t2                | s3://t2/year=2024/month=12/day=06/feed=Z/data.01.parquet | 2023-07-11T16:45:44  |",
                    "+--------------------+----------------------+---------------------+-------------------+----------------------------------------------------------+----------------------+",
                ],
                file_metadata: "
                    ('datafusion', 'test', 'm4', 's3://m4/year=2023/data.01.parquet', '2023-07-12T16:00:00Z', 0),
                    ('datafusion', 'test', 'm4', 's3://m4/year=2024/data.01.parquet', '2023-07-12T16:00:00Z', 0)
                ",
                expected_stale_files_output: vec![
                    "+--------------------+----------------------+-----------------------+----------+",
                    "| target             | target_last_modified | sources_last_modified | is_stale |",
                    "+--------------------+----------------------+-----------------------+----------+",
                    "| s3://m4/year=2023/ | 2023-07-12T16:00:00  | 2023-07-11T16:45:44   | false    |",
                    "| s3://m4/year=2024/ | 2023-07-12T16:00:00  | 2023-07-11T16:45:44   | false    |",
                    "+--------------------+----------------------+-----------------------+----------+",
                ],
            },
        ];

        async fn run_test(case: &TestCase) -> Result<()> {
            let context = setup().await.unwrap();

            let plan = context
                .sql(case.query_to_analyze)
                .await?
                .into_optimized_plan()?;

            println!("original plan: \n{}", plan.display_indent());

            let partition_col_indices = plan
                .schema()
                .columns()
                .into_iter()
                .enumerate()
                .filter_map(|(i, c)| case.partition_cols.contains(&c.name.as_str()).then_some(i))
                .collect();
            println!("indices: {:?}", partition_col_indices);
            let analyzed = pushdown_projection_inexact(plan.clone(), &partition_col_indices)?;
            println!(
                "inexact projection pushdown:\n{}",
                analyzed.display_indent()
            );

            context
                .register_table(
                    case.table_name,
                    // Register table with a decorator to exercise this functionality
                    Arc::new(DecoratorTable {
                        inner: Arc::new(MockMaterializedView {
                            table_path: case.table_path.clone(),
                            partition_columns: case
                                .partition_cols
                                .iter()
                                .map(|s| s.to_string())
                                .collect(),
                            query: plan,
                            file_ext: case.file_extension,
                        }),
                    }),
                )
                .expect("couldn't register materialized view");

            context
                .sql(&format!(
                    "INSERT INTO file_metadata VALUES {}",
                    case.file_metadata,
                ))
                .await?
                .collect()
                .await?;

            let df = context
                .sql(&format!(
                    "SELECT * FROM mv_dependencies('{}', 'v2')",
                    case.table_name,
                ))
                .await
                .map_err(|e| e.context("get file dependencies"))?;
            df.clone().explain(false, false)?.show().await?;
            df.clone().show().await?;

            assert_batches_sorted_eq!(case.expected_output, &df.collect().await?);

            let df = context
                .sql(&format!(
                    "SELECT * FROM stale_files('{}', 'v2')",
                    case.table_name
                ))
                .await
                .map_err(|e| e.context("get stale files"))?;
            df.clone().explain(false, false)?.show().await?;
            df.clone().show().await?;

            assert_batches_sorted_eq!(case.expected_stale_files_output, &df.collect().await?);

            Ok(())
        }

        for case in cases {
            run_test(case)
                .await
                .unwrap_or_else(|e| panic!("{} failed: {e}", case.name));
        }
    }

    #[tokio::test]
    async fn test_projection_pushdown_inexact() -> Result<()> {
        struct TestCase {
            name: &'static str,
            query_to_analyze: &'static str,
            projection: &'static [&'static str],
            expected_plan: Vec<&'static str>,
            expected_output: Vec<&'static str>,
        }

        let cases = &[
            TestCase {
                name: "simple projection",
                query_to_analyze:
                    "SELECT column1 AS partition_column, concat(column2, column3) AS some_value FROM t1",
                projection: &["partition_column"],
                expected_plan: vec![
                    "+--------------+--------------------------------------------+",
                    "| plan_type    | plan                                       |",
                    "+--------------+--------------------------------------------+",
                    "| logical_plan | Projection: t1.column1 AS partition_column |",
                    "|              |   TableScan: t1 projection=[column1]       |",
                    "+--------------+--------------------------------------------+",
                ],
                expected_output: vec![
                    "+------------------+",
                    "| partition_column |",
                    "+------------------+",
                    "| 2021             |",
                    "| 2022             |",
                    "| 2023             |",
                    "+------------------+",
                ],
            },
            TestCase {
                name: "compound expressions",
                query_to_analyze: "
                    SELECT DISTINCT
                        to_timestamp_nanos(concat_ws('-', year, month, day)) AS timestamp,
                        feed
                    FROM t2",
                projection: &["timestamp", "feed"],
                expected_plan: vec![
                    "+--------------+-------------------------------------------------------------------------------------------------------+",
                    "| plan_type    | plan                                                                                                  |",
                    "+--------------+-------------------------------------------------------------------------------------------------------+",
                    "| logical_plan | Projection: to_timestamp_nanos(concat_ws(Utf8(\"-\"), t2.year, t2.month, t2.day)) AS timestamp, t2.feed |",
                    "|              |   TableScan: t2 projection=[year, month, day, feed]                                                   |",
                    "+--------------+-------------------------------------------------------------------------------------------------------+",
                ]
                ,
                expected_output: vec![
                    "+---------------------+------+",
                    "| timestamp           | feed |",
                    "+---------------------+------+",
                    "| 2023-01-01T00:00:00 | A    |",
                    "| 2023-01-02T00:00:00 | B    |",
                    "| 2023-01-03T00:00:00 | C    |",
                    "| 2024-12-04T00:00:00 | X    |",
                    "| 2024-12-05T00:00:00 | Y    |",
                    "| 2024-12-06T00:00:00 | Z    |",
                    "+---------------------+------+",
                ],
            },
            TestCase {
                name: "empty projection",
                query_to_analyze: "SELECT column1 AS output FROM t3",
                projection: &[],
                expected_plan: vec![
                    "+--------------+-----------------------------+",
                    "| plan_type    | plan                        |",
                    "+--------------+-----------------------------+",
                    "| logical_plan | TableScan: t3 projection=[] |",
                    "+--------------+-----------------------------+",
                ],
                expected_output: vec![
                    "++",
                    "++",
                    "++",
                ],
            },
            TestCase {
                name: "simple equijoin on year",
                query_to_analyze: "SELECT * FROM t2 INNER JOIN t3 USING (year)",
                projection: &["year"],
                expected_plan: vec![
                    "+--------------+-------------------------------------+",
                    "| plan_type    | plan                                |",
                    "+--------------+-------------------------------------+",
                    "| logical_plan | Projection: t2.year                 |",
                    "|              |   Inner Join: t2.year = t3.year     |",
                    "|              |     TableScan: t2 projection=[year] |",
                    "|              |     TableScan: t3 projection=[year] |",
                    "+--------------+-------------------------------------+",
                ],
                expected_output: vec![
                    "+------+",
                    "| year |",
                    "+------+",
                    "| 2023 |",
                    "| 2023 |",
                    "| 2023 |",
                    "| 2024 |",
                    "| 2024 |",
                    "| 2024 |",
                    "+------+",
                ],
            },
            TestCase {
                name: "triangular join on year",
                query_to_analyze: "
                    SELECT
                        t2.*,
                        t3.* EXCLUDE(year),
                        t3.year AS \"t3.year\"
                    FROM t2
                    INNER JOIN t3
                    ON (t2.year <= t3.year)",
                projection: &["year"],
                expected_plan: vec![
                    "+--------------+-------------------------------------------+",
                    "| plan_type    | plan                                      |",
                    "+--------------+-------------------------------------------+",
                    "| logical_plan | Projection: t2.year                       |",
                    "|              |   Inner Join:  Filter: t2.year <= t3.year |",
                    "|              |     TableScan: t2 projection=[year]       |",
                    "|              |     TableScan: t3 projection=[year]       |",
                    "+--------------+-------------------------------------------+",
                ],
                expected_output: vec![
                    "+------+",
                    "| year |",
                    "+------+",
                    "| 2023 |",
                    "| 2023 |",
                    "| 2023 |",
                    "| 2023 |",
                    "| 2023 |",
                    "| 2023 |",
                    "| 2024 |",
                    "| 2024 |",
                    "| 2024 |",
                    "+------+",
                ],
            },
            TestCase {
                name: "window & unnest",
                query_to_analyze: "
                SELECT
                    \"__unnest_placeholder(date).year\" AS year,
                    \"__unnest_placeholder(date).month\" AS month,
                    \"__unnest_placeholder(date).day\" AS day,
                    arr
                FROM (
                    SELECT
                        unnest(date),
                        unnest(arr) AS arr
                    FROM (
                        SELECT
                            named_struct('year', year, 'month', month, 'day', day) AS date,
                            array_agg(column2)
                                OVER (ORDER BY year, month, day)
                                AS arr
                        FROM t2
                    )
                )",
                projection: &["year", "month", "day"],
                expected_plan: vec![
                    "+--------------+---------------------------------------------------------------------------------------------------------------------------------------+",
                    "| plan_type    | plan                                                                                                                                  |",
                    "+--------------+---------------------------------------------------------------------------------------------------------------------------------------+",
                    "| logical_plan | Projection: __unnest_placeholder(date).year AS year, __unnest_placeholder(date).month AS month, __unnest_placeholder(date).day AS day |",
                    "|              |   Unnest: lists[] structs[__unnest_placeholder(date)]                                                                                 |",
                    "|              |     Projection: named_struct(Utf8(\"year\"), t2.year, Utf8(\"month\"), t2.month, Utf8(\"day\"), t2.day) AS __unnest_placeholder(date)       |",
                    "|              |       TableScan: t2 projection=[year, month, day]                                                                                     |",
                    "+--------------+---------------------------------------------------------------------------------------------------------------------------------------+",
                ],
                expected_output: vec![
                    "+------+-------+-----+",
                    "| year | month | day |",
                    "+------+-------+-----+",
                    "| 2023 | 01    | 01  |",
                    "| 2023 | 01    | 02  |",
                    "| 2023 | 01    | 03  |",
                    "| 2024 | 12    | 04  |",
                    "| 2024 | 12    | 05  |",
                    "| 2024 | 12    | 06  |",
                    "+------+-------+-----+",
                ],
            },
            TestCase {
                name: "outer join + union",
                query_to_analyze: "
                SELECT
                    COALESCE(t1.year, t2.year) AS year,
                    t1.column2
                FROM (SELECT column1 AS year, column2 FROM t1) t1
                FULL OUTER JOIN (SELECT year, column2 FROM t2) t2
                USING (year)
                UNION ALL
                SELECT year, column1 AS column2 FROM t3
                ",
                projection: &["year"],
                expected_plan: vec![
                    "+--------------+--------------------------------------------------+",
                    "| plan_type    | plan                                             |",
                    "+--------------+--------------------------------------------------+",
                    "| logical_plan | Union                                            |",
                    "|              |   Projection: coalesce(t1.year, t2.year) AS year |",
                    "|              |     Full Join: Using t1.year = t2.year           |",
                    "|              |       SubqueryAlias: t1                          |",
                    "|              |         Projection: t1.column1 AS year           |",
                    "|              |           TableScan: t1 projection=[column1]     |",
                    "|              |       SubqueryAlias: t2                          |",
                    "|              |         TableScan: t2 projection=[year]          |",
                    "|              |   TableScan: t3 projection=[year]                |",
                    "+--------------+--------------------------------------------------+",
                ],
                expected_output: vec![
                    "+------+",
                    "| year |",
                    "+------+",
                    "| 2021 |",
                    "| 2022 |",
                    "| 2023 |",
                    "| 2023 |",
                    "| 2023 |",
                    "| 2023 |",
                    "| 2024 |",
                    "| 2024 |",
                    "| 2024 |",
                    "| 2024 |",
                    "+------+",
                ],
            }
        ];

        async fn run_test(case: &TestCase) -> Result<()> {
            let context = setup().await?;

            let df = context.sql(case.query_to_analyze).await?;
            df.clone().explain(false, false)?.show().await?;

            let plan = df.clone().into_optimized_plan()?;

            let indices = case
                .projection
                .iter()
                .map(|&name| {
                    plan.schema()
                        .index_of_column(&Column::new_unqualified(name))
                })
                .collect::<Result<HashSet<_>>>()?;

            let analyzed = DataFrame::new(
                context.state(),
                pushdown_projection_inexact(plan.clone(), &indices)?,
            );
            analyzed.clone().explain(false, false)?.show().await?;

            // Check the following property of pushdown_projection_inexact:
            // if A' = pushdown_projection_inexact(A, P), where P is the projection,
            // then PA ⊆ A'.
            if !case.projection.is_empty() {
                let select_original = df
                    .clone()
                    .select(
                        case.projection
                            .iter()
                            .map(|&name| Expr::Column(Column::new_unqualified(name)))
                            .collect_vec(),
                    )
                    .map_err(|e| e.context("select projection from original plan"))?
                    .distinct()?;

                let excess = analyzed
                    .clone()
                    .distinct()?
                    .join(
                        select_original.clone(),
                        JoinType::RightAnti,
                        case.projection,
                        case.projection,
                        None,
                    )
                    .map_err(|e| e.context("join in subset inclusion test"))?;

                assert_eq!(
                    excess
                        .clone()
                        .count()
                        .await
                        .map_err(|e| e.context("execute subset inclusion test"))?,
                    0,
                    "unexpected extra rows in transformed query:\n{}
                            original:\n{}
                            inexact pushdown:\n{}
                            ",
                    pretty_format_batches(&excess.collect().await?)?,
                    pretty_format_batches(&select_original.collect().await?)?,
                    pretty_format_batches(&analyzed.clone().distinct()?.collect().await?)?
                );
            }

            assert_batches_eq!(
                case.expected_plan,
                &analyzed.clone().explain(false, false)?.collect().await?
            );
            assert_batches_sorted_eq!(case.expected_output, &analyzed.collect().await?);

            Ok(())
        }

        for case in cases {
            run_test(case)
                .await
                .unwrap_or_else(|e| panic!("{} failed: {e}", case.name));
        }

        Ok(())
    }
}
