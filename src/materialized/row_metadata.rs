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
use datafusion_common::{DataFusionError, Result};
use datafusion_expr::{LogicalPlanBuilder, TableScan};
use datafusion_sql::ResolvedTableReference;
use std::sync::Arc;

/// Registry that manages metadata sources for different tables.
/// Provides a centralized way to register and retrieve metadata sources
/// that can be used to obtain row-level metadata for tables.
#[derive(Default)]
pub struct RowMetadataRegistry {
    metadata_sources: DashMap<String, Arc<dyn RowMetadataSource>>,
}

impl RowMetadataRegistry {
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
            .ok_or_else(|| DataFusionError::Internal(format!("No metadata source for {}", table)))
    }
}

/// A source for "row metadata", that associates rows from a table with
/// metadata used for incremental view maintenance.
///
/// Most use cases should default to using [`FileMetadata`](super::file_metadata::FileMetadata) for their [`RowMetadataSource`],
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
        self: Arc<Self>,
        table: ResolvedTableReference,
        scan: &TableScan,
    ) -> Result<LogicalPlanBuilder>;
}
