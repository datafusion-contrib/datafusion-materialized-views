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

//! # datafusion-materialized-views
//!
//! `datafusion-materialized-views` provides robust algorithms and core functionality for working with materialized views in [DataFusion](https://arrow.apache.org/datafusion/).
//!
//! ## Key Features
//!
//! - **Incremental View Maintenance**: Efficiently tracks dependencies between Hive-partitioned tables and their materialized views, allowing users to determine which partitions need to be refreshed when source data changes. This is achieved via UDTFs such as `mv_dependencies` and `stale_files`.
//! - **Query Rewriting**: Implements a view matching optimizer that rewrites queries to automatically leverage materialized views when beneficial, based on the techniques described in the [paper](https://dsg.uwaterloo.ca/seminars/notes/larson-paper.pdf).
//! - **Pluggable Metadata Sources**: Supports custom metadata sources for incremental view maintenance, with default support for object store metadata via the `FileMetadata` and `RowMetadataRegistry` components.
//! - **Extensible Table Abstractions**: Defines traits such as `ListingTableLike` and `Materialized` to abstract over Hive-partitioned tables and materialized views, enabling custom implementations and easy registration for use in the maintenance and rewriting logic.
//!
//! ## Typical Workflow
//!
//! 1. **Define and Register Views**: Implement a custom table type that implements the `Materialized` trait, and register it using `register_materialized`.
//! 2. **Metadata Initialization**: Set up `FileMetadata` and `RowMetadataRegistry` to track file-level and row-level metadata.
//! 3. **Dependency Tracking**: Use the `mv_dependencies` UDTF to generate build graphs for materialized views, and `stale_files` to identify partitions that require recomputation.
//! 4. **Query Optimization**: Enable the query rewriting optimizer to transparently rewrite queries to use materialized views where possible.
//!
//! ## Example
//!
//! See the README and integration tests for a full walkthrough of setting up and maintaining a materialized view, including dependency tracking and query rewriting.
//!
//! ## Limitations
//!
//! - Currently supports only Hive-partitioned tables in object storage, with the smallest update unit being a file.
//! - Future work may generalize to other storage backends and partitioning schemes.
//!
//! ## References
//!
//! - [Optimizing Queries Using Materialized Views: A Practical, Scalable Solution](https://dsg.uwaterloo.ca/seminars/notes/larson-paper.pdf)
//! - [DataFusion documentation](https://datafusion.apache.org/)

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
///
/// The implementation is based heavily on [this paper](https://dsg.uwaterloo.ca/seminars/notes/larson-paper.pdf),
/// *Optimizing Queries Using Materialized Views: A Practical, Scalable Solution*.
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
