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

use arrow::array::{StringBuilder, TimestampNanosecondBuilder, UInt64Builder};
use arrow::record_batch::RecordBatch;
use arrow_schema::{DataType, Field, TimeUnit};
use async_trait::async_trait;
use datafusion::arrow::datatypes::{Schema, SchemaRef};
use datafusion::catalog::SchemaProvider;
use datafusion::catalog::{CatalogProvider, Session};
use datafusion::datasource::listing::ListingTableUrl;
use datafusion::datasource::TableProvider;
use datafusion::execution::object_store::ObjectStoreUrl;
use datafusion::physical_expr::{create_physical_expr, EquivalenceProperties};
use datafusion::physical_plan::expressions::{BinaryExpr, Column, Literal};
use datafusion::physical_plan::limit::LimitStream;
use datafusion::physical_plan::metrics::{BaselineMetrics, ExecutionPlanMetricsSet, MetricsSet};
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, Partitioning, PhysicalExpr, PlanProperties,
};
use datafusion::{
    catalog::CatalogProviderList, execution::TaskContext, physical_plan::SendableRecordBatchStream,
};
use datafusion_common::{DFSchema, DataFusionError, Result, ScalarValue};
use datafusion_expr::{Expr, Operator, TableProviderFilterPushDown, TableType};
use datafusion_physical_plan::execution_plan::{Boundedness, EmissionType};
use futures::stream::{self, BoxStream};
use futures::{future, Future, FutureExt, StreamExt, TryStreamExt};
use itertools::Itertools;
use log::debug;
use object_store::{ObjectMeta, ObjectStore};
use std::any::Any;
use std::sync::Arc;

use crate::materialized::cast_to_listing_table;

/// A virtual file metadata table, inspired by the information schema column table.
#[derive(Debug, Clone)]
pub struct FileMetadata {
    table_schema: SchemaRef,
    catalog_list: Arc<dyn CatalogProviderList>,
    metadata_provider: Arc<dyn FileMetadataProvider>,
}

impl FileMetadata {
    /// Construct a new [`FileMetadata`] table provider that lists files for all
    /// tables in the provided catalog list.
    pub fn new(
        catalog_list: Arc<dyn CatalogProviderList>,
        metadata_provider: Arc<dyn FileMetadataProvider>,
    ) -> Self {
        Self {
            table_schema: Arc::new(Schema::new(vec![
                Field::new("table_catalog", DataType::Utf8, false),
                Field::new("table_schema", DataType::Utf8, false),
                Field::new("table_name", DataType::Utf8, false),
                Field::new("file_path", DataType::Utf8, false),
                Field::new(
                    "last_modified",
                    DataType::Timestamp(TimeUnit::Nanosecond, Some(Arc::from("UTC"))),
                    false,
                ),
                Field::new("size", DataType::UInt64, false),
            ])),
            catalog_list,
            metadata_provider,
        }
    }
}

#[async_trait]
impl TableProvider for FileMetadata {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.table_schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        session_state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let dfschema = DFSchema::try_from(self.table_schema.as_ref().clone())?;

        let filters = filters
            .iter()
            .map(|expr| {
                create_physical_expr(expr, &dfschema, session_state.execution_props())
                    .map_err(|e| e.context("failed to create file metadata physical expr"))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let exec = FileMetadataExec::try_new(
            self.table_schema.clone(),
            projection.cloned(),
            filters,
            limit,
            self.catalog_list.clone(),
            self.metadata_provider.clone(),
        )?;

        Ok(Arc::new(exec))
    }

    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> Result<Vec<TableProviderFilterPushDown>> {
        Ok(vec![TableProviderFilterPushDown::Inexact; filters.len()])
    }
}

/// An [`ExecutionPlan`] that scans object store metadata.
pub struct FileMetadataExec {
    table_schema: SchemaRef,
    plan_properties: PlanProperties,
    projection: Option<Vec<usize>>,
    filters: Vec<Arc<dyn PhysicalExpr>>,
    limit: Option<usize>,
    metrics: ExecutionPlanMetricsSet,
    catalog_list: Arc<dyn CatalogProviderList>,
    metadata_provider: Arc<dyn FileMetadataProvider>,
}

impl FileMetadataExec {
    fn try_new(
        table_schema: SchemaRef,
        projection: Option<Vec<usize>>,
        filters: Vec<Arc<dyn PhysicalExpr>>,
        limit: Option<usize>,
        catalog_list: Arc<dyn CatalogProviderList>,
        metadata_provider: Arc<dyn FileMetadataProvider>,
    ) -> Result<Self> {
        let projected_schema = match projection.as_ref() {
            Some(projection) => Arc::new(table_schema.project(projection)?),
            None => table_schema.clone(),
        };
        let eq_properties = EquivalenceProperties::new(projected_schema);
        let partitioning = Partitioning::UnknownPartitioning(1);
        let plan_properties = PlanProperties::new(
            eq_properties,
            partitioning,
            EmissionType::Final,
            Boundedness::Bounded,
        );

        let exec = Self {
            table_schema,
            plan_properties,
            projection,
            filters,
            limit,
            metrics: ExecutionPlanMetricsSet::new(),
            catalog_list,
            metadata_provider,
        };

        Ok(exec)
    }
}

impl ExecutionPlan for FileMetadataExec {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "FileMetadataExec"
    }

    fn properties(&self) -> &PlanProperties {
        &self.plan_properties
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![]
    }

    fn with_new_children(
        self: Arc<Self>,
        _children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        Ok(self)
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        let projection = self.projection.clone();
        let record_batches = self.build_record_batch(context)?;

        let projected_record_batches = record_batches.map(move |record_batches| {
            let record_batches = match record_batches {
                Ok(record_batches) => record_batches,
                Err(err) => return vec![Err(err)],
            };

            if let Some(projection) = projection {
                return record_batches
                    .into_iter()
                    .map(|record_batch| {
                        record_batch
                            .project(&projection)
                            .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))
                    })
                    .collect::<Vec<_>>();
            }

            record_batches.into_iter().map(Ok).collect::<Vec<_>>()
        });

        let mut record_batch_stream: SendableRecordBatchStream =
            Box::pin(RecordBatchStreamAdapter::new(
                self.schema(),
                futures::stream::once(projected_record_batches)
                    .map(stream::iter)
                    .flatten(),
            ));

        if let Some(limit) = self.limit {
            let baseline_metrics = BaselineMetrics::new(&self.metrics, partition);
            let limit_stream =
                LimitStream::new(record_batch_stream, 0, Some(limit), baseline_metrics);
            record_batch_stream = Box::pin(limit_stream);
        }

        Ok(record_batch_stream)
    }

    fn metrics(&self) -> Option<MetricsSet> {
        Some(self.metrics.clone_inner())
    }
}

impl FileMetadataExec {
    fn get_column_index(&self, column_name: &str) -> Result<usize> {
        let (index, _) = self
            .table_schema
            .column_with_name(column_name)
            .ok_or_else(|| {
                DataFusionError::Internal(format!("column '{column_name}' does not exists"))
            })?;
        Ok(index)
    }

    /// Get the string literal value from an 'equals' BinaryExpr with a column.
    fn get_column_literal(column_idx: usize, filter: &Arc<dyn PhysicalExpr>) -> Option<String> {
        let binary_expr = filter.as_any().downcast_ref::<BinaryExpr>()?;

        if !matches!(binary_expr.op(), Operator::Eq) {
            return None;
        }

        let (column, literal) = if let Some(left_column) =
            binary_expr.left().as_any().downcast_ref::<Column>()
        {
            let right_literal = binary_expr.right().as_any().downcast_ref::<Literal>()?;
            (left_column, right_literal)
        } else if let Some(right_column) = binary_expr.right().as_any().downcast_ref::<Column>() {
            let left_literal = binary_expr.left().as_any().downcast_ref::<Literal>()?;
            (right_column, left_literal)
        } else {
            return None;
        };

        if column.index() != column_idx {
            return None;
        }

        match literal.value() {
            ScalarValue::Utf8(str) => str.clone(),
            ScalarValue::LargeUtf8(str) => str.clone(),
            _ => None,
        }
    }

    /// Builds a RecordBatch containing file metadata that satisfies the provided filters.
    fn build_record_batch(
        &self,
        context: Arc<TaskContext>,
    ) -> Result<impl Future<Output = Result<Vec<RecordBatch>>>> {
        let catalog_column = self.get_column_index("table_catalog")?;
        let schema_column = self.get_column_index("table_schema")?;
        let table_column = self.get_column_index("table_name")?;

        let catalog_name = self
            .filters
            .iter()
            .filter_map(|filter| Self::get_column_literal(catalog_column, filter))
            .next();

        let schema_name = self
            .filters
            .iter()
            .filter_map(|filter| Self::get_column_literal(schema_column, filter))
            .next();

        let table_name = self
            .filters
            .iter()
            .filter_map(|filter| Self::get_column_literal(table_column, filter))
            .next();

        let table_schema = self.table_schema.clone();
        let catalog_list = self.catalog_list.clone();
        let metadata_provider = self.metadata_provider.clone();

        let record_batch = async move {
            // If we cannot determine the catalog, build from the entire catalog list.
            let catalog_name = match catalog_name {
                Some(catalog_name) => catalog_name,
                None => {
                    debug!("No catalog filter exists, returning entire catalog list.");
                    return FileMetadataBuilder::build_from_catalog_list(
                        catalog_list,
                        metadata_provider,
                        table_schema,
                        context,
                    )
                    .await;
                }
            };

            // If the specified catalog doesn't exist, return an empty result;
            let catalog_provider = match catalog_list.catalog(&catalog_name) {
                Some(catalog_provider) => catalog_provider,
                None => {
                    debug!("No catalog named '{catalog_name}' exists, returning an empty result.");
                    return Ok(vec![]);
                }
            };

            // If we cannot determine the schema, build from the catalog.
            let schema_name = match schema_name {
                Some(schema_name) => schema_name,
                None => {
                    debug!("No schema filter exists, returning catalog '{catalog_name}'.");
                    return FileMetadataBuilder::build_from_catalog(
                        &catalog_name,
                        catalog_provider,
                        metadata_provider,
                        table_schema,
                        context,
                    )
                    .await;
                }
            };

            // If the specified schema doesn't exist, return an empty result.
            let schema_provider = match catalog_provider.schema(&schema_name) {
                Some(schema_provider) => schema_provider,
                None => {
                    debug!("No schema named '{catalog_name}.{schema_name}' exists, returning an empty result.");
                    return Ok(vec![]);
                }
            };

            // If we cannot determine a table , build from the schema.
            let table_name = match table_name {
                Some(table_name) => table_name,
                None => {
                    debug!(
                        "No table filter exists, returning schema '{catalog_name}.{schema_name}'."
                    );
                    return FileMetadataBuilder::build_from_schema(
                        &catalog_name,
                        &schema_name,
                        schema_provider,
                        metadata_provider,
                        table_schema,
                        context,
                    )
                    .await;
                }
            };

            // If the specified table doesn't exist, return an empty result;
            let table_provider = match schema_provider.table(&table_name).await? {
                Some(table_provider) => table_provider,
                None => {
                    debug!("No table named '{catalog_name}.{schema_name}.{table_name}' exists, returning an empty result.");
                    return Ok(vec![]);
                }
            };

            debug!("Returning table '{catalog_name}.{schema_name}.{table_name}'.");

            let record_batch = FileMetadataBuilder::build_from_table(
                &catalog_name,
                &schema_name,
                &table_name,
                table_provider,
                metadata_provider,
                table_schema,
                context,
            )
            .await?;

            Ok(record_batch.into_iter().collect_vec())
        };

        Ok(record_batch)
    }
}

impl std::fmt::Debug for FileMetadataExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileMetadataExec")
            .field("plan_properties", &self.plan_properties)
            .field("filters", &self.filters)
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}

impl DisplayAs for FileMetadataExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "FileMetadataExec: ")?;

        write!(f, "filters=[")?;
        let mut filters = self.filters.iter().peekable();
        while let Some(filter) = filters.next() {
            std::fmt::Display::fmt(filter, f)?;
            if filters.peek().is_some() {
                write!(f, ", ")?;
            }
        }
        write!(f, "]")?;

        if let Some(limit) = &self.limit {
            write!(f, ", limit={limit}")?;
        }

        Ok(())
    }
}

struct FileMetadataBuilder {
    schema: SchemaRef,
    metadata_provider: Arc<dyn FileMetadataProvider>,
    catalog_names: StringBuilder,
    schema_names: StringBuilder,
    table_names: StringBuilder,
    file_paths: StringBuilder,
    last_modified: TimestampNanosecondBuilder,
    size: UInt64Builder,
}

impl FileMetadataBuilder {
    fn new(schema: SchemaRef, metadata_provider: Arc<dyn FileMetadataProvider>) -> Self {
        Self {
            schema,
            metadata_provider,
            catalog_names: StringBuilder::new(),
            schema_names: StringBuilder::new(),
            table_names: StringBuilder::new(),
            file_paths: StringBuilder::new(),
            last_modified: TimestampNanosecondBuilder::new().with_timezone("UTC"),
            size: UInt64Builder::new(),
        }
    }

    async fn build_from_catalog_list(
        catalog_list: Arc<dyn CatalogProviderList>,
        metadata_provider: Arc<dyn FileMetadataProvider>,
        schema: SchemaRef,
        context: Arc<TaskContext>,
    ) -> Result<Vec<RecordBatch>> {
        let mut tasks = vec![];

        for catalog_name in catalog_list.catalog_names() {
            let catalog_provider = match catalog_list.catalog(&catalog_name) {
                Some(catalog_provider) => catalog_provider,
                None => continue,
            };
            let metadata_provider = metadata_provider.clone();
            let schema = schema.clone();
            let context = context.clone();

            tasks.push(async move {
                Self::build_from_catalog(
                    &catalog_name,
                    catalog_provider,
                    metadata_provider,
                    schema,
                    context,
                )
                .await
            });
        }

        let results = future::join_all(tasks).await;

        let record_batches = results
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect();

        Ok(record_batches)
    }

    async fn build_from_catalog(
        catalog_name: &str,
        catalog_provider: Arc<dyn CatalogProvider>,
        metadata_provider: Arc<dyn FileMetadataProvider>,
        schema: SchemaRef,
        context: Arc<TaskContext>,
    ) -> Result<Vec<RecordBatch>> {
        let mut tasks = vec![];

        for schema_name in catalog_provider.schema_names() {
            let schema_provider = match catalog_provider.schema(&schema_name) {
                Some(schema_provider) => schema_provider,
                None => continue,
            };
            let metadata_provider = metadata_provider.clone();
            let schema = schema.clone();
            let context = context.clone();

            tasks.push(async move {
                Self::build_from_schema(
                    catalog_name,
                    &schema_name,
                    schema_provider,
                    metadata_provider,
                    schema,
                    context,
                )
                .await
            });
        }

        let results = future::join_all(tasks).await;

        let record_batches = results
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect();

        Ok(record_batches)
    }

    async fn build_from_schema(
        catalog_name: &str,
        schema_name: &str,
        schema_provider: Arc<dyn SchemaProvider>,
        metadata_provider: Arc<dyn FileMetadataProvider>,
        schema: SchemaRef,
        context: Arc<TaskContext>,
    ) -> Result<Vec<RecordBatch>> {
        let mut tasks = vec![];

        for table_name in schema_provider.table_names() {
            let table_provider = match schema_provider.table(&table_name).await? {
                Some(table_provider) => table_provider,
                None => continue,
            };
            let metadata_provider = metadata_provider.clone();
            let schema = schema.clone();
            let context = context.clone();

            tasks.push(async move {
                Self::build_from_table(
                    catalog_name,
                    schema_name,
                    &table_name,
                    table_provider,
                    metadata_provider,
                    schema,
                    context,
                )
                .await
            })
        }

        let results = future::join_all(tasks).await;
        let record_batches = results
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect();

        Ok(record_batches)
    }

    async fn build_from_table(
        catalog_name: &str,
        schema_name: &str,
        table_name: &str,
        table_provider: Arc<dyn TableProvider>,
        metadata_provider: Arc<dyn FileMetadataProvider>,
        schema: SchemaRef,
        context: Arc<TaskContext>,
    ) -> Result<Option<RecordBatch>> {
        let mut builder = Self::new(schema.clone(), metadata_provider.clone());

        let listing_table_like = match cast_to_listing_table(table_provider.as_ref()) {
            None => return Ok(None),
            Some(t) => t,
        };

        let table_paths = listing_table_like.table_paths();
        let file_extension = listing_table_like.file_ext();

        for table_path in table_paths {
            builder
                .read_table_files(
                    catalog_name,
                    schema_name,
                    table_name,
                    &table_path,
                    &file_extension,
                    &context,
                )
                .await?;
        }

        builder.finish().map(Some)
    }

    async fn read_table_files(
        &mut self,
        catalog_name: &str,
        schema_name: &str,
        table_name: &str,
        table_path: &ListingTableUrl,
        file_ext: &str,
        context: &TaskContext,
    ) -> Result<()> {
        let store_url = table_path.object_store();
        let store = context.runtime_env().object_store(table_path)?;

        let mut file_stream = self
            .metadata_provider
            .list_all_files(
                store.clone(),
                table_path.clone(),
                file_ext.to_string(),
                context
                    .session_config()
                    .options()
                    .execution
                    .listing_table_ignore_subdirectory,
            )
            .await;

        while let Some(file_meta) = file_stream.try_next().await? {
            self.append(
                catalog_name,
                schema_name,
                table_name,
                &store_url,
                &file_meta,
            );
        }

        Ok(())
    }

    fn append(
        &mut self,
        catalog_name: &str,
        schema_name: &str,
        table_name: &str,
        store_url: &ObjectStoreUrl,
        meta: &ObjectMeta,
    ) {
        self.catalog_names.append_value(catalog_name);
        self.schema_names.append_value(schema_name);
        self.table_names.append_value(table_name);
        self.file_paths
            .append_value(format!("{store_url}{}", meta.location));
        self.last_modified
            .append_option(meta.last_modified.timestamp_nanos_opt());
        self.size.append_value(meta.size); // this is not lossy assuming we're on a 64-bit platform
    }

    fn finish(mut self) -> Result<RecordBatch> {
        RecordBatch::try_new(
            self.schema,
            vec![
                Arc::new(self.catalog_names.finish()),
                Arc::new(self.schema_names.finish()),
                Arc::new(self.table_names.finish()),
                Arc::new(self.file_paths.finish()),
                Arc::new(self.last_modified.finish()),
                Arc::new(self.size.finish()),
            ],
        )
        .map_err(From::from)
    }
}

/// Provides [`ObjectMetadata`] data to the [`FileMetadata`] table provider.
#[async_trait]
pub trait FileMetadataProvider: std::fmt::Debug + Send + Sync {
    /// List all files in the store for the given `url` prefix.
    async fn list_all_files(
        &self,
        store: Arc<dyn ObjectStore>,
        url: ListingTableUrl,
        file_extension: String,
        ignore_subdirectory: bool,
    ) -> BoxStream<'static, Result<ObjectMeta>>;
}

/// Default implementation of the [`FileMetadataProvider`].
#[derive(Debug)]
pub struct DefaultFileMetadataProvider;

#[async_trait]
impl FileMetadataProvider for DefaultFileMetadataProvider {
    // Mostly copied from ListingTableUrl::list_all_files, which is private to that crate
    // Modified to handle empty tables
    async fn list_all_files(
        &self,
        store: Arc<dyn ObjectStore>,
        url: ListingTableUrl,
        file_extension: String,
        ignore_subdirectory: bool,
    ) -> BoxStream<'static, Result<ObjectMeta>> {
        // Check if the directory exists yet
        if let Err(object_store::Error::NotFound { path, .. }) =
            store.list_with_delimiter(Some(url.prefix())).await
        {
            debug!(
                "attempted to list empty table at {path} during file_metadata listing, returning empty list"
            );
            return Box::pin(stream::empty());
        }

        let is_dir = url.as_str().ends_with('/');
        let prefix = url.prefix().clone();

        let list = match is_dir {
            true => store.list(Some(&prefix)),
            false => futures::stream::once(async move { store.head(&prefix).await }).boxed(),
        };

        list.map_err(Into::into)
            .try_filter(move |meta| {
                let path = &meta.location;
                let extension_match = path.as_ref().ends_with(&file_extension);
                let glob_match = url.contains(path, ignore_subdirectory);
                futures::future::ready(extension_match && glob_match)
            })
            .boxed()
    }
}

#[cfg(test)]
mod test {
    use std::{ops::Deref, sync::Arc};

    use anyhow::{Context, Result};
    use datafusion::{
        assert_batches_sorted_eq,
        catalog::{MemoryCatalogProvider, MemorySchemaProvider},
        execution::{
            object_store::{DefaultObjectStoreRegistry, ObjectStoreRegistry},
            runtime_env::RuntimeEnvBuilder,
        },
        prelude::{SessionConfig, SessionContext},
    };
    use object_store::local::LocalFileSystem;
    use tempfile::TempDir;
    use url::Url;

    use super::{DefaultFileMetadataProvider, FileMetadata};

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

        ctx.catalog("datafusion")
            .context("get default catalog")?
            .register_schema("private", Arc::<MemorySchemaProvider>::default())
            .context("register datafusion.private schema")?;

        ctx.register_catalog("datafusion_mv", Arc::<MemoryCatalogProvider>::default());

        ctx.catalog("datafusion_mv")
            .context("get datafusion_mv catalog")?
            .register_schema("public", Arc::<MemorySchemaProvider>::default())
            .context("register datafusion_mv.public schema")?;

        ctx.sql(
            "
            CREATE EXTERNAL TABLE t1 (num INTEGER, year TEXT)
            STORED AS CSV
            PARTITIONED BY (year)
            LOCATION 'file:///t1/'
        ",
        )
        .await?
        .collect()
        .await?;

        ctx.sql(
            "INSERT INTO t1 VALUES
            (1, '2021'),
            (2, '2022'),
            (3, '2023'),
            (4, '2024')
        ",
        )
        .await?
        .collect()
        .await?;

        ctx.sql(
            "
            CREATE EXTERNAL TABLE private.t1 (num INTEGER, year TEXT, month TEXT)
            STORED AS CSV
            PARTITIONED BY (year, month)
            LOCATION 'file:///private/t1/'
        ",
        )
        .await?
        .collect()
        .await?;

        ctx.sql(
            "INSERT INTO private.t1 VALUES
            (1, '2021', '01'),
            (2, '2022', '02'),
            (3, '2023', '03'),
            (4, '2024', '04')
        ",
        )
        .await?
        .collect()
        .await?;

        ctx.sql(
            "
            CREATE EXTERNAL TABLE datafusion_mv.public.t3 (num INTEGER, date DATE)
            STORED AS CSV
            PARTITIONED BY (date)
            LOCATION 'file:///datafusion_mv/public/t3/'
        ",
        )
        .await?
        .collect()
        .await?;

        ctx.sql(
            "INSERT INTO datafusion_mv.public.t3 VALUES
            (1, '2021-01-01'),
            (2, '2022-02-02'),
            (3, '2023-03-03'),
            (4, '2024-04-04')
        ",
        )
        .await?
        .collect()
        .await?;

        ctx.register_table(
            "file_metadata",
            Arc::new(FileMetadata::new(
                Arc::clone(ctx.state().catalog_list()),
                Arc::new(DefaultFileMetadataProvider),
            )),
        )
        .context("register file metadata table")?;

        ctx.sql(
            // Remove timestamps and trim (randomly generated) file names since they're not stable in tests
            "CREATE VIEW file_metadata_test_view AS SELECT
                * EXCLUDE(file_path, last_modified),
                regexp_replace(file_path, '/[^/]*$', '/') AS file_path
            FROM file_metadata",
        )
        .await
        .context("create file metadata test view")?;

        Ok(TestContext { _dir: dir, ctx })
    }

    #[tokio::test]
    async fn test_list_all_files() -> Result<()> {
        let ctx = setup().await.context("setup")?;

        let results = ctx
            .sql("SELECT * FROM file_metadata_test_view")
            .await?
            .collect()
            .await?;

        assert_batches_sorted_eq!(&[
            "+---------------+--------------+------------+------+--------------------------------------------------+",
            "| table_catalog | table_schema | table_name | size | file_path                                        |",
            "+---------------+--------------+------------+------+--------------------------------------------------+",
            "| datafusion    | private      | t1         | 6    | file:///private/t1/year=2021/month=01/           |",
            "| datafusion    | private      | t1         | 6    | file:///private/t1/year=2022/month=02/           |",
            "| datafusion    | private      | t1         | 6    | file:///private/t1/year=2023/month=03/           |",
            "| datafusion    | private      | t1         | 6    | file:///private/t1/year=2024/month=04/           |",
            "| datafusion    | public       | t1         | 6    | file:///t1/year=2021/                            |",
            "| datafusion    | public       | t1         | 6    | file:///t1/year=2022/                            |",
            "| datafusion    | public       | t1         | 6    | file:///t1/year=2023/                            |",
            "| datafusion    | public       | t1         | 6    | file:///t1/year=2024/                            |",
            "| datafusion_mv | public       | t3         | 6    | file:///datafusion_mv/public/t3/date=2021-01-01/ |",
            "| datafusion_mv | public       | t3         | 6    | file:///datafusion_mv/public/t3/date=2022-02-02/ |",
            "| datafusion_mv | public       | t3         | 6    | file:///datafusion_mv/public/t3/date=2023-03-03/ |",
            "| datafusion_mv | public       | t3         | 6    | file:///datafusion_mv/public/t3/date=2024-04-04/ |",
            "+---------------+--------------+------------+------+--------------------------------------------------+",
        ], &results);

        Ok(())
    }

    #[tokio::test]
    async fn test_list_catalog() -> Result<()> {
        let ctx = setup().await.context("setup")?;

        let results = ctx
            .sql(
                "SELECT * FROM file_metadata_test_view
                WHERE table_catalog = 'datafusion_mv'",
            )
            .await?
            .collect()
            .await?;

        assert_batches_sorted_eq!(&[
            "+---------------+--------------+------------+------+--------------------------------------------------+",
            "| table_catalog | table_schema | table_name | size | file_path                                        |",
            "+---------------+--------------+------------+------+--------------------------------------------------+",
            "| datafusion_mv | public       | t3         | 6    | file:///datafusion_mv/public/t3/date=2021-01-01/ |",
            "| datafusion_mv | public       | t3         | 6    | file:///datafusion_mv/public/t3/date=2022-02-02/ |",
            "| datafusion_mv | public       | t3         | 6    | file:///datafusion_mv/public/t3/date=2023-03-03/ |",
            "| datafusion_mv | public       | t3         | 6    | file:///datafusion_mv/public/t3/date=2024-04-04/ |",
            "+---------------+--------------+------------+------+--------------------------------------------------+",
        ], &results);

        Ok(())
    }

    #[tokio::test]
    async fn test_list_catalog_and_schema() -> Result<()> {
        let ctx = setup().await.context("setup")?;

        let results = ctx
            .sql(
                "SELECT * FROM file_metadata_test_view
                WHERE table_catalog = 'datafusion' AND table_schema = 'private'",
            )
            .await?
            .collect()
            .await?;

        assert_batches_sorted_eq!(&[
            "+---------------+--------------+------------+------+----------------------------------------+",
            "| table_catalog | table_schema | table_name | size | file_path                              |",
            "+---------------+--------------+------------+------+----------------------------------------+",
            "| datafusion    | private      | t1         | 6    | file:///private/t1/year=2021/month=01/ |",
            "| datafusion    | private      | t1         | 6    | file:///private/t1/year=2022/month=02/ |",
            "| datafusion    | private      | t1         | 6    | file:///private/t1/year=2023/month=03/ |",
            "| datafusion    | private      | t1         | 6    | file:///private/t1/year=2024/month=04/ |",
            "+---------------+--------------+------------+------+----------------------------------------+",
        ], &results);

        Ok(())
    }

    #[tokio::test]
    async fn test_list_schema_only() -> Result<()> {
        let ctx = setup().await.context("setup")?;

        let results = ctx
            .sql(
                "SELECT * FROM file_metadata_test_view
                WHERE table_schema = 'public'",
            )
            .await?
            .collect()
            .await?;

        assert_batches_sorted_eq!(&[
            "+---------------+--------------+------------+------+--------------------------------------------------+",
            "| table_catalog | table_schema | table_name | size | file_path                                        |",
            "+---------------+--------------+------------+------+--------------------------------------------------+",
            "| datafusion    | public       | t1         | 6    | file:///t1/year=2021/                            |",
            "| datafusion    | public       | t1         | 6    | file:///t1/year=2022/                            |",
            "| datafusion    | public       | t1         | 6    | file:///t1/year=2023/                            |",
            "| datafusion    | public       | t1         | 6    | file:///t1/year=2024/                            |",
            "| datafusion_mv | public       | t3         | 6    | file:///datafusion_mv/public/t3/date=2021-01-01/ |",
            "| datafusion_mv | public       | t3         | 6    | file:///datafusion_mv/public/t3/date=2022-02-02/ |",
            "| datafusion_mv | public       | t3         | 6    | file:///datafusion_mv/public/t3/date=2023-03-03/ |",
            "| datafusion_mv | public       | t3         | 6    | file:///datafusion_mv/public/t3/date=2024-04-04/ |",
            "+---------------+--------------+------------+------+--------------------------------------------------+",
        ], &results);

        Ok(())
    }

    #[tokio::test]
    async fn test_list_catalog_schema_and_table() -> Result<()> {
        let ctx = setup().await.context("setup")?;

        let results = ctx
            .sql(
                "SELECT * FROM file_metadata_test_view
                WHERE table_catalog = 'datafusion' AND table_schema = 'public' AND table_name = 't1'",
            )
            .await?
            .collect()
            .await?;

        assert_batches_sorted_eq!(
            &[
                "+---------------+--------------+------------+------+-----------------------+",
                "| table_catalog | table_schema | table_name | size | file_path             |",
                "+---------------+--------------+------------+------+-----------------------+",
                "| datafusion    | public       | t1         | 6    | file:///t1/year=2021/ |",
                "| datafusion    | public       | t1         | 6    | file:///t1/year=2022/ |",
                "| datafusion    | public       | t1         | 6    | file:///t1/year=2023/ |",
                "| datafusion    | public       | t1         | 6    | file:///t1/year=2024/ |",
                "+---------------+--------------+------------+------+-----------------------+",
            ],
            &results
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_list_table_only() -> Result<()> {
        let ctx = setup().await.context("setup")?;

        let results = ctx
            .sql(
                "SELECT * FROM file_metadata_test_view
                WHERE table_name = 't1'",
            )
            .await?
            .collect()
            .await?;

        assert_batches_sorted_eq!(
            &[
                "+---------------+--------------+------------+------+----------------------------------------+",
                "| table_catalog | table_schema | table_name | size | file_path                              |",
                "+---------------+--------------+------------+------+----------------------------------------+",
                "| datafusion    | private      | t1         | 6    | file:///private/t1/year=2021/month=01/ |",
                "| datafusion    | private      | t1         | 6    | file:///private/t1/year=2022/month=02/ |",
                "| datafusion    | private      | t1         | 6    | file:///private/t1/year=2023/month=03/ |",
                "| datafusion    | private      | t1         | 6    | file:///private/t1/year=2024/month=04/ |",
                "| datafusion    | public       | t1         | 6    | file:///t1/year=2021/                  |",
                "| datafusion    | public       | t1         | 6    | file:///t1/year=2022/                  |",
                "| datafusion    | public       | t1         | 6    | file:///t1/year=2023/                  |",
                "| datafusion    | public       | t1         | 6    | file:///t1/year=2024/                  |",
                "+---------------+--------------+------------+------+----------------------------------------+",
            ],
            &results
        );

        Ok(())
    }
}
