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

use dashmap::DashMap;
use datafusion::catalog::TableProvider;
use datafusion_common::{DataFusionError, Result};
use datafusion_expr::{LogicalPlanBuilder, TableScan};
use datafusion_sql::ResolvedTableReference;
use std::{collections::BTreeMap, sync::Arc};

use super::{file_metadata::FileMetadata, hive_partition::hive_partition, META_COLUMN};

/// Registry that manages metadata sources for different tables.
/// Provides a centralized way to register and retrieve metadata sources
/// that can be used to obtain row-level metadata for tables.
pub struct RowMetadataRegistry {
    metadata_sources: DashMap<String, Arc<dyn RowMetadataSource>>,
    default_source: Option<Arc<dyn RowMetadataSource>>,
}

impl std::fmt::Debug for RowMetadataRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RowMetadataRegistry")
            .field(
                "metadata_sources",
                &self
                    .metadata_sources
                    .iter()
                    .map(|r| (r.key().clone(), r.value().name().to_string()))
                    .collect::<BTreeMap<_, _>>(),
            )
            .finish()
    }
}

impl RowMetadataRegistry {
    /// Initializes this `RowMetadataRegistry` with a default [`RowMetadataSource`]
    /// to be used if a table has not been explicitly registered with a specific source.
    ///
    /// Typically the [`FileMetadata`] source should be used as the default.
    pub fn new_with_default_source(default_source: Arc<dyn RowMetadataSource>) -> Self {
        Self {
            metadata_sources: Default::default(),
            default_source: Some(default_source),
        }
    }

    /// Initializes a new `RowMetadataRegistry` with no default [`RowMetadataSource`].
    ///
    /// Users should typically use [`RowMetadataRegistry::new_with_default_source`].
    pub fn new_empty() -> Self {
        Self {
            metadata_sources: Default::default(),
            default_source: None,
        }
    }

    /// Registers a metadata source for a specific table.
    /// Returns the previously registered source for this table, if any
    pub fn register_source(
        &self,
        table: &ResolvedTableReference,
        source: Arc<dyn RowMetadataSource>,
    ) -> Option<Arc<dyn RowMetadataSource>> {
        self.metadata_sources.insert(table.to_string(), source)
    }

    /// Retrieves the registered [`RowMetadataSource`] for a specific table.
    pub fn get_source(&self, table: &ResolvedTableReference) -> Result<Arc<dyn RowMetadataSource>> {
        self.metadata_sources
            .get(&table.to_string())
            .map(|o| Arc::clone(o.value()))
            .or_else(|| self.default_source.clone())
            .ok_or_else(|| DataFusionError::Internal(format!("No metadata source for {}", table)))
    }
}

/// A source for "row metadata", that associates rows from a table with
/// metadata used for incremental view maintenance.
///
/// Most use cases should default to using [`FileMetadata`] for their [`RowMetadataSource`],
/// which uses object store metadata to perform incremental view maintenance on Hive-partitioned tables.
/// However, in some use cases it is necessary to track metadata at a more granular level than Hive partitions.
/// In such cases, users may implement a custom [`RowMetadataSource`] containing this metadata.
///
/// A [`RowMetadataSource`] may contain metadata for multiple tables.
/// As such, it is the user's responsibility to register each table with the appropriate
/// [`RowMetadataSource`] in the [`RowMetadataRegistry`].
pub trait RowMetadataSource: Send + Sync {
    /// The name of this row metadata source.
    fn name(&self) -> &str;

    /// Rewrite this [`TableScan`] as query against this [`RowMetadataSource`],
    /// this time adding a new struct column, `__meta`, whose shape conforms to the following schema:
    ///
    /// ```json
    /// {
    ///     "table_catalog": "string",
    ///     "table_schema": "string",
    ///     "table_name": "string",
    ///     "source_uri": "string",
    ///     "last_modified": "timestamp",
    /// }
    /// ```
    ///
    /// The returned data should contain the original table scan, up to multiplicity.
    /// That is, for each row in the original table scan, the [`RowMetadataSource`] should contain at least
    /// one row (but potentially more) with the same values, plus the `__meta` column.
    fn row_metadata(
        &self,
        table: ResolvedTableReference,
        scan: &TableScan,
    ) -> Result<LogicalPlanBuilder>;
}

/// A [`RowMetadataSource`] that uses an object storage API to retrieve
/// partition columns and timestamp metadata.
///
/// Object store metadata by default comes from [`FileMetadata`], but
/// may be overridden with a custom [`TableProvider`] using
/// [`Self::with_file_metadata`].
#[derive(Debug, Clone)]
pub struct ObjectStoreRowMetadataSource {
    file_metadata: Arc<dyn TableProvider>,
}

impl ObjectStoreRowMetadataSource {
    /// Create a new [`ObjectStoreRowMetadataSource`] from the [`FileMetadata`] table
    pub fn new(file_metadata: Arc<FileMetadata>) -> Self {
        Self::with_file_metadata(file_metadata)
    }

    /// Create a new [`ObjectStoreRowMetadataSource`] using a custom file metadata source
    pub fn with_file_metadata(file_metadata: Arc<dyn TableProvider>) -> Self {
        Self { file_metadata }
    }
}

impl RowMetadataSource for ObjectStoreRowMetadataSource {
    fn name(&self) -> &str {
        "ObjectStoreRowMetadataSource"
    }

    /// Scan for partition column values using object store metadata.
    /// This allows us to efficiently scan for distinct partition column values without
    /// ever reading from a table directly, which is useful for low-overhead
    /// incremental view maintenance.
    fn row_metadata(
        &self,
        table: datafusion_sql::ResolvedTableReference,
        scan: &datafusion_expr::TableScan,
    ) -> Result<datafusion_expr::LogicalPlanBuilder> {
        use datafusion::{datasource::provider_as_source, prelude::*};

        // Disable this check in tests
        #[cfg(not(test))]
        {
            // Check that the remaining columns in the source table scans are indeed partition columns
            let partition_cols = super::cast_to_listing_table(
                datafusion::datasource::source_as_provider(&scan.source)?.as_ref(),
            )
            .ok_or_else(|| {
                DataFusionError::Internal(format!(
                    "Table '{}' was not registered in TableTypeRegistry",
                    scan.table_name
                ))
            })?
            .partition_columns();

            for column in scan.projected_schema.columns() {
                if !partition_cols.contains(&column.name) {
                    return Err(DataFusionError::Internal(format!("Row metadata not available on non-partition column from source table '{table}': {}", column.name)));
                }
            }
        }

        let fields = scan.projected_schema.fields();

        let row_metadata_expr = named_struct(vec![
            lit("table_catalog"),
            col("table_catalog"),
            lit("table_schema"),
            col("table_schema"),
            lit("table_name"),
            col("table_name"),
            lit("source_uri"), // Map file_path to source_uri
            col("file_path"),
            lit("last_modified"),
            col("last_modified"),
        ])
        .alias(META_COLUMN);

        datafusion_expr::LogicalPlanBuilder::scan(
            "file_metadata",
            provider_as_source(Arc::clone(&self.file_metadata)),
            None,
        )?
        .filter(
            col("table_catalog")
                .eq(lit(table.catalog.as_ref()))
                .and(col("table_schema").eq(lit(table.schema.as_ref())))
                .and(col("table_name").eq(lit(table.table.as_ref()))),
        )?
        .project(
            fields
                .iter()
                .map(|field| {
                    // CAST(hive_partition(file_path, 'field_name', true) AS field_data_type) AS field_name
                    cast(
                        hive_partition(vec![col("file_path"), lit(field.name()), lit(true)]),
                        field.data_type().clone(),
                    )
                    .alias(field.name())
                })
                .chain(Some(row_metadata_expr)),
        )
    }
}
