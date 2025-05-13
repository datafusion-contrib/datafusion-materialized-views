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

#![deny(missing_docs)]

//! `datafusion-materialized-views` implements algorithms and functionality for materialized views in DataFusion.

/// Code for incremental view maintenance against Hive-partitioned tables.
///
/// An example of a Hive-partitioned table is the [`ListingTable`](datafusion::datasource::listing::ListingTable).
/// By analyzing the fragment of the materialized view query pertaining to the partition columns,
/// we can derive a build graph that relates the files of a materialized views and the files of the tables it depends on.
///
/// Two central traits are defined:
///
/// * [`ListingTableLike`](materialized::ListingTableLike): a trait that abstracts Hive-partitioned tables in object storage;
/// * [`Materialized`](materialized::Materialized): a materialized `ListingTableLike` defined by a user-provided query.
///
/// Note that all implementations of `ListingTableLike` and `Materialized` must be registered using the
/// [`register_listing_table`](materialized::register_listing_table) and
/// [`register_materialized`](materialized::register_materialized) functions respectively,
/// otherwise the tables may not be detected by the incremental view maintenance code,
/// including components such as [`FileMetadata`](materialized::file_metadata::FileMetadata),
/// [`RowMetadataRegistry`](materialized::row_metadata::RowMetadataRegistry), or the
/// [`mv_dependencies`](materialized::dependencies::mv_dependencies) UDTF.
///
/// By default, `ListingTableLike` is implemented for [`ListingTable`](datafusion::datasource::listing::ListingTable),
pub mod materialized;

/// An implementation of Query Rewriting, an optimization that rewrites queries to make use of materialized views.
pub mod rewrite;

/// Configuration options for materialized view related features.
#[derive(Debug, Clone)]
pub struct MaterializedConfig {
    /// Whether or not query rewriting should exploit this materialized view.
    pub use_in_query_rewrite: bool,
}

impl Default for MaterializedConfig {
    fn default() -> Self {
        Self {
            use_in_query_rewrite: true,
        }
    }
}
