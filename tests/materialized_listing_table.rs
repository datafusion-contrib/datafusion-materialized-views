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

use std::{any::Any, borrow::Cow, collections::HashMap, ops::Deref, sync::Arc};

use anyhow::{bail, Context, Result};
use arrow::{array::StringArray, compute::concat_batches, util::pretty};
use arrow_schema::{DataType, SchemaRef};
use datafusion::{
    catalog::{Session, TableProvider},
    datasource::{
        file_format::{parquet::ParquetFormat, FileFormat},
        listing::{ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl},
    },
    execution::{
        object_store::{DefaultObjectStoreRegistry, ObjectStoreRegistry},
        runtime_env::RuntimeEnvBuilder,
    },
    prelude::{SessionConfig, SessionContext},
};
use datafusion_common::{
    metadata::ScalarAndMetadata, Constraints, DataFusionError, ParamValues, ScalarValue, Statistics,
};
use datafusion_expr::{
    col, dml::InsertOp, Expr, JoinType, LogicalPlan, LogicalPlanBuilder, SortExpr,
    TableProviderFilterPushDown, TableType,
};
use datafusion_materialized_views::materialized::{
    dependencies::{mv_dependencies, stale_files},
    file_metadata::{DefaultFileMetadataProvider, FileMetadata},
    register_materialized,
    row_metadata::RowMetadataRegistry,
    ListingTableLike, Materialized,
};
use datafusion_physical_plan::{collect, ExecutionPlan};
use datafusion_sql::TableReference;
use futures::{StreamExt, TryStreamExt};
use itertools::{Either, Itertools};
use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjectPath;
use tempfile::TempDir;
use url::Url;

/// Configuration for a materialized listing table
struct MaterializedListingConfig {
    /// Path in object storage where the materialized data will be stored
    table_path: ListingTableUrl,
    /// The query defining the view to be materialized
    query: LogicalPlan,
    /// Additional configuration options
    options: Option<MaterializedListingOptions>,
}

struct MaterializedListingOptions {
    file_extension: String,
    format: Arc<dyn FileFormat>,
    table_partition_cols: Vec<String>,
    collect_stat: bool,
    target_partitions: usize,
    file_sort_order: Vec<Vec<SortExpr>>,
}

/// A materialized [`ListingTable`].
#[derive(Debug)]
struct MaterializedListingTable {
    /// The underlying [`ListingTable`]
    inner: ListingTable,
    /// The query defining this materialized view
    query: LogicalPlan,
    /// The Arrow schema of the query
    schema: SchemaRef,
}

impl ListingTableLike for MaterializedListingTable {
    fn table_paths(&self) -> Vec<ListingTableUrl> {
        <ListingTable as ListingTableLike>::table_paths(&self.inner)
    }

    fn partition_columns(&self) -> Vec<String> {
        <ListingTable as ListingTableLike>::partition_columns(&self.inner)
    }

    fn file_ext(&self) -> String {
        self.inner.file_ext()
    }
}

impl Materialized for MaterializedListingTable {
    fn query(&self) -> LogicalPlan {
        self.query.clone()
    }
}

struct TestContext {
    _dir: TempDir,
    ctx: SessionContext,
}

impl Deref for TestContext {
    type Target = SessionContext;

    fn deref(&self) -> &Self::Target {
        &self.ctx
    }
}

async fn setup() -> Result<TestContext> {
    let _ = env_logger::builder().is_test(true).try_init();

    // All custom table providers must be registered using the `register_materialized` and/or `register_listing_table` APIs.
    register_materialized::<MaterializedListingTable>();

    // Override the object store with one rooted in a temporary directory

    let dir = TempDir::new().context("create tempdir")?;
    let store = LocalFileSystem::new_with_prefix(&dir)
        .map(Arc::new)
        .context("create local file system object store")?;

    let registry = Arc::new(DefaultObjectStoreRegistry::new());
    registry
        .register_store(&Url::parse("file://").unwrap(), store)
        .context("register file system store")
        .expect("should replace existing object store at file://");

    let ctx = SessionContext::new_with_config_rt(
        SessionConfig::new(),
        RuntimeEnvBuilder::new()
            .with_object_store_registry(registry)
            .build_arc()
            .context("create RuntimeEnv")?,
    );

    // Now register the `mv_dependencies` and `stale_files` UDTFs
    // They have `FileMetadata` and `RowMetadataRegistry` as dependencies.

    let file_metadata = Arc::new(FileMetadata::new(
        Arc::clone(ctx.state().catalog_list()),
        Arc::new(DefaultFileMetadataProvider),
    ));
    let row_metadata_registry = Arc::new(RowMetadataRegistry::new(Arc::clone(&file_metadata)));

    ctx.register_udtf(
        "mv_dependencies",
        mv_dependencies(
            Arc::clone(ctx.state().catalog_list()),
            Arc::clone(&row_metadata_registry),
            ctx.state().config_options(),
        ),
    );
    ctx.register_udtf(
        "stale_files",
        stale_files(
            Arc::clone(ctx.state().catalog_list()),
            Arc::clone(&row_metadata_registry),
            Arc::clone(&file_metadata) as Arc<dyn TableProvider>,
            ctx.state().config_options(),
        ),
    );

    // Create a table with some data
    // Our materialized view will be derived from this table
    ctx.sql(
        "
        CREATE EXTERNAL TABLE t1 (num INTEGER, date TEXT, feed CHAR)
        STORED AS CSV
        PARTITIONED BY (date, feed)
        LOCATION 'file:///t1/'
    ",
    )
    .await?
    .show()
    .await?;

    ctx.sql(
        "INSERT INTO t1 VALUES
        (1, '2023-01-01', 'A'),
        (2, '2023-01-02', 'B'),
        (3, '2023-01-03', 'C'),
        (4, '2024-12-04', 'X'),
        (5, '2024-12-05', 'Y'),
        (6, '2024-12-06', 'Z')
    ",
    )
    .await?
    .collect()
    .await?;

    Ok(TestContext { _dir: dir, ctx })
}

#[tokio::test]
async fn test_materialized_listing_table_incremental_maintenance() -> Result<()> {
    let ctx = setup().await.context("setup")?;

    let query = ctx
        .sql(
            "
        SELECT
            COUNT(*) AS count,
            CAST(date_part('YEAR', date) AS INT) AS year
        FROM t1
        GROUP BY year
    ",
        )
        .await?
        .into_unoptimized_plan();

    let mv = Arc::new(MaterializedListingTable::try_new(
        MaterializedListingConfig {
            table_path: ListingTableUrl::parse("file:///m1/")?,
            query,
            options: Some(MaterializedListingOptions {
                file_extension: ".parquet".to_string(),
                format: Arc::<ParquetFormat>::default(),
                table_partition_cols: vec!["year".into()],
                collect_stat: false,
                target_partitions: 1,
                file_sort_order: vec![vec![SortExpr {
                    expr: col("count"),
                    asc: true,
                    nulls_first: false,
                }]],
            }),
        },
    )?);

    ctx.register_table("m1", Arc::clone(&mv) as Arc<dyn TableProvider>)?;

    assert_eq!(
        refresh_materialized_listing_table(&ctx, "m1".into()).await?,
        vec![
            // The initial call should refresh all Hive partitions
            "file:///m1/year=2023/".to_string(),
            "file:///m1/year=2024/".to_string()
        ]
    );
    assert!(materialized_view_up_to_date(&ctx, &mv, "m1").await?);

    // Insert another row into the source table
    ctx.sql(
        "INSERT INTO t1 VALUES
        (7, '2024-12-07', 'W')",
    )
    .await?
    .collect()
    .await?;

    // Materialized view should be out of date now
    assert!(!materialized_view_up_to_date(&ctx, &mv, "m1").await?);

    // Refresh the materialized view and check that it's up to date again
    assert_eq!(
        refresh_materialized_listing_table(&ctx, "m1".into()).await?,
        // Only the Hive partition for 2024 should be refreshed,
        // since the row we added was in 2024.
        vec!["file:///m1/year=2024/".to_string()]
    );
    assert!(materialized_view_up_to_date(&ctx, &mv, "m1").await?);

    Ok(())
}

/// Check that the table's materialized data the same rows as its query.
async fn materialized_view_up_to_date(
    ctx: &TestContext,
    mv: &MaterializedListingTable,
    table_name: impl Into<TableReference>,
) -> Result<bool> {
    let table_name = table_name.into();
    // Using anti-joins, verify A ⊆ B and B ⊆ A (therefore A = B)
    for join_type in [JoinType::LeftAnti, JoinType::RightAnti] {
        let num_rows_not_matching = ctx
            .sql(&format!("SELECT * FROM {table_name}"))
            .await?
            .join(
                ctx.execute_logical_plan(mv.query.clone()).await?,
                join_type,
                &["count", "year"],
                &["count", "year"],
                None,
            )?
            .count()
            .await?;
        if num_rows_not_matching != 0 {
            return Ok(false);
        }
    }

    Ok(true)
}

impl MaterializedListingTable {
    fn try_new(config: MaterializedListingConfig) -> Result<Self, DataFusionError> {
        let schema = config.query.schema().as_arrow();
        let (partition_indices, file_indices): (Vec<usize>, Vec<usize>) = schema
            .fields()
            .iter()
            .enumerate()
            .partition_map(|(i, field)| {
                if config
                    .options
                    .as_ref()
                    .is_some_and(|opts| opts.table_partition_cols.contains(field.name()))
                {
                    Either::Left(i)
                } else {
                    Either::Right(i)
                }
            });

        let file_schema = schema.project(&file_indices)?;

        // Rewrite the query to have the partition columns at the end.
        // It's convention for Hive-partitioned tables to have the partition columns at the end.
        let normalizing_projection = file_indices
            .iter()
            .copied()
            .chain(partition_indices.iter().copied())
            .collect_vec();

        let normalized_query = LogicalPlanBuilder::new(config.query.clone())
            .select(normalizing_projection)?
            .build()?;
        let normalized_schema = Arc::new(normalized_query.schema().as_arrow().clone());

        let table_partition_cols = schema
            .project(&partition_indices)?
            .fields
            .into_iter()
            .map(|f| (f.name().clone(), f.data_type().clone()))
            .collect_vec();

        let options = config.options.map(|opts| ListingOptions {
            file_extension: opts.file_extension,
            format: opts.format,
            table_partition_cols,
            collect_stat: opts.collect_stat,
            target_partitions: opts.target_partitions,
            file_sort_order: opts.file_sort_order,
        });

        let mut listing_table_config = ListingTableConfig::new(config.table_path);
        if let Some(options) = options {
            listing_table_config = listing_table_config.with_listing_options(options);
        }
        listing_table_config = listing_table_config.with_schema(Arc::new(file_schema));
        Ok(MaterializedListingTable {
            inner: ListingTable::try_new(listing_table_config)?,
            query: normalized_query,
            schema: normalized_schema,
        })
    }
}

/// Attempt to refresh the data in this materialized view.
async fn refresh_materialized_listing_table(
    ctx: &TestContext,
    table_name: TableReference,
) -> Result<Vec<String>> {
    let stale_targets = ctx
        .sql(&format!(
            "SELECT target FROM stale_files('{table_name}') WHERE is_stale ORDER BY target"
        ))
        .await?
        .collect()
        .await?;

    let batches = concat_batches(&stale_targets[0].schema(), &stale_targets)?;
    let targets = batches
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap()
        .into_iter()
        .flatten()
        .collect_vec();

    let table = ctx.table_provider(table_name.clone()).await?;

    for target in &targets {
        refresh_mv_target(ctx, table.as_any().downcast_ref().unwrap(), target).await?;
    }

    if ctx
        .sql(&format!(
            "SELECT target FROM stale_files('{table_name}') WHERE is_stale"
        ))
        .await?
        .count()
        .await?
        != 0
    {
        bail!("Expected no stale targets after materialization");
    }

    Ok(targets.iter().map(|s| s.to_string()).collect())
}

/// Refresh a particular target directory for a materialized view.
///
/// More robust implementations should perform this atomically.
/// For the sake of ease, we just delete the data then use the `ExecutionPlan::insert_into` API.
async fn refresh_mv_target(
    ctx: &TestContext,
    table: &MaterializedListingTable,
    target: &str,
) -> Result<()> {
    let url = ListingTableUrl::parse(target)?;
    let store = ctx.state().runtime_env().object_store(url.object_store())?;

    // Delete all of the data in this directory
    store
        .delete_stream(
            store
                .list(Some(url.prefix()))
                .map_ok(|meta| meta.location)
                .boxed(),
        )
        .try_collect::<Vec<_>>()
        .await?;

    // Insert fresh data
    let writer_plan = table
        .insert_into(
            &ctx.state(),
            ctx.execute_logical_plan(table.job_for_target(url)?)
                .await?
                .create_physical_plan()
                .await?,
            InsertOp::Append,
        )
        .await?;

    pretty::print_batches(&collect(writer_plan, ctx.task_ctx()).await?)?;

    Ok(())
}

impl MaterializedListingTable {
    /// Returns a query in the form of a logical plan, with placeholder parameters corresponding to the
    /// partition columns of this table.
    /// Data for a single partition can be generated by executing this query with the partition values as parameters.
    fn job_for_partition(&self) -> Result<LogicalPlan, DataFusionError> {
        use datafusion::prelude::*;
        let part_cols = self.partition_columns();

        LogicalPlanBuilder::new(self.query.clone())
            .filter(
                part_cols
                    .iter()
                    .enumerate()
                    .map(|(i, pc)| col(pc).eq(placeholder(format!("${}", i + 1))))
                    .fold(lit(true), |a, b| a.and(b)),
            )?
            .sort(self.inner.options().file_sort_order[0].clone())?
            .build()
    }

    /// Extracts the partition columns from the target URL and returns a query that can be executed
    /// to regenerate the data for this target directory.
    fn job_for_target(&self, target: ListingTableUrl) -> Result<LogicalPlan, DataFusionError> {
        let partition_columns_with_data_types = self.inner.options().table_partition_cols.clone();

        self.job_for_partition()?
            .with_param_values(ParamValues::List(parse_partition_values(
                target.prefix(),
                &partition_columns_with_data_types,
            )?))
    }
}

#[async_trait::async_trait]
impl TableProvider for MaterializedListingTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    fn constraints(&self) -> Option<&Constraints> {
        self.inner.constraints()
    }

    fn table_type(&self) -> TableType {
        self.inner.table_type()
    }

    fn get_table_definition(&self) -> Option<&str> {
        self.inner.get_table_definition()
    }

    fn get_logical_plan(&self) -> Option<Cow<'_, LogicalPlan>> {
        // We _could_ return the LogicalPlan here,
        // but it will cause this table to be treated like a regular view
        // and the materialized results will not be used.
        None
    }

    fn get_column_default(&self, column: &str) -> Option<&Expr> {
        self.inner.get_column_default(column)
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>, DataFusionError> {
        self.inner.scan(state, projection, filters, limit).await
    }

    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> Result<Vec<TableProviderFilterPushDown>, DataFusionError> {
        self.inner.supports_filters_pushdown(filters)
    }

    fn statistics(&self) -> Option<Statistics> {
        self.inner.statistics()
    }

    async fn insert_into(
        &self,
        state: &dyn Session,
        input: Arc<dyn ExecutionPlan>,
        insert_op: InsertOp,
    ) -> Result<Arc<dyn ExecutionPlan>, DataFusionError> {
        self.inner.insert_into(state, input, insert_op).await
    }
}

/// Parse partition column values from an object path.
fn parse_partition_values(
    path: &ObjectPath,
    partition_columns: &[(String, DataType)],
) -> Result<Vec<ScalarAndMetadata>, DataFusionError> {
    let parts = path.parts().map(|part| part.to_owned()).collect::<Vec<_>>();

    let pairs = parts
        .iter()
        .filter_map(|part| part.as_ref().split_once('='))
        .collect::<HashMap<_, _>>();

    let partition_values = partition_columns
        .iter()
        .map(|(column, datatype)| {
            let value = pairs.get(column.as_str()).copied().map(String::from);
            ScalarAndMetadata::from(ScalarValue::Utf8(value)).cast_storage_to(datatype)
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(partition_values)
}
