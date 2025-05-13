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

/// Track dependencies of materialized data in object storage
pub mod dependencies;

/// Pluggable metadata sources for incremental view maintenance
pub mod row_metadata;

/// A virtual table that exposes files in object storage.
pub mod file_metadata;

/// A UDF that parses Hive partition elements from object storage paths.
mod hive_partition;

/// Some private utility functions
mod util;

use std::{
    any::{type_name, Any, TypeId},
    fmt::Debug,
    sync::{Arc, LazyLock},
};

use dashmap::DashMap;
use datafusion::{
    catalog::TableProvider,
    datasource::listing::{ListingTable, ListingTableUrl},
};
use datafusion_expr::LogicalPlan;
use itertools::Itertools;

use crate::MaterializedConfig;

/// The identifier of the column that [`RowMetadataSource`](row_metadata::RowMetadataSource) implementations should store row metadata in.
pub const META_COLUMN: &str = "__meta";

static TABLE_TYPE_REGISTRY: LazyLock<TableTypeRegistry> = LazyLock::new(TableTypeRegistry::default);

/// A [`TableProvider`] whose data is backed by Hive-partitioned files in object storage.
pub trait ListingTableLike: TableProvider + 'static {
    /// Object store URLs for this table
    fn table_paths(&self) -> Vec<ListingTableUrl>;

    /// Hive partition columns
    fn partition_columns(&self) -> Vec<String>;

    /// File extension used by this listing table
    fn file_ext(&self) -> String;
}

impl ListingTableLike for ListingTable {
    fn table_paths(&self) -> Vec<ListingTableUrl> {
        self.table_paths().clone()
    }

    fn partition_columns(&self) -> Vec<String> {
        self.options()
            .table_partition_cols
            .iter()
            .map(|(name, _data_type)| name.clone())
            .collect_vec()
    }

    fn file_ext(&self) -> String {
        self.options().file_extension.clone()
    }
}

/// Register a [`ListingTableLike`] implementation in this registry.
/// This allows `cast_to_listing_table` to easily downcast a [`TableProvider`]
/// into a `ListingTableLike` where possible.
pub fn register_listing_table<T: ListingTableLike>() {
    TABLE_TYPE_REGISTRY.register_listing_table::<T>();
}

/// Attempt to cast the given TableProvider into a [`ListingTableLike`].
/// If the table's type has not been registered using [`register_listing_table`], will return `None`.
pub fn cast_to_listing_table(table: &dyn TableProvider) -> Option<&dyn ListingTableLike> {
    TABLE_TYPE_REGISTRY
        .cast_to_listing_table(table)
        .or_else(|| {
            TABLE_TYPE_REGISTRY
                .cast_to_decorator(table)
                .and_then(|decorator| cast_to_listing_table(decorator.base()))
        })
}

/// A hive-partitioned table in object storage that is defined by a user-provided query.
pub trait Materialized: ListingTableLike {
    /// The query that defines this materialized view.
    fn query(&self) -> LogicalPlan;

    /// Configuration to control materialized view related features.
    /// By default, returns the default value for [`MaterializedConfig`]
    fn config(&self) -> MaterializedConfig {
        MaterializedConfig::default()
    }
}

/// Register a [`Materialized`] implementation in this registry.
/// This allows `cast_to_materialized` to easily downcast a [`TableProvider`]
/// into a `Materialized` where possible.
///
/// Note that this will also register `T` as a [`ListingTableLike`].
pub fn register_materialized<T: Materialized>() {
    TABLE_TYPE_REGISTRY.register_materialized::<T>();
}

/// Attempt to cast the given TableProvider into a [`Materialized`].
/// If the table's type has not been registered using [`register_materialized`], will return `None`.
pub fn cast_to_materialized(table: &dyn TableProvider) -> Option<&dyn Materialized> {
    TABLE_TYPE_REGISTRY.cast_to_materialized(table).or_else(|| {
        TABLE_TYPE_REGISTRY
            .cast_to_decorator(table)
            .and_then(|decorator| cast_to_materialized(decorator.base()))
    })
}

/// A `TableProvider` that decorates other `TableProvider`s.
/// Sometimes users may implement a `TableProvider` that overrides functionality of a base `TableProvider`.
/// This API allows the decorator to also be recognized as `ListingTableLike` or `Materialized` automatically.
pub trait Decorator: TableProvider + 'static {
    /// The underlying `TableProvider` that this decorator wraps.
    fn base(&self) -> &dyn TableProvider;
}

/// Register `T` as a [`Decorator`].
pub fn register_decorator<T: Decorator>() {
    TABLE_TYPE_REGISTRY.register_decorator::<T>()
}

type Downcaster<T> = Arc<dyn Fn(&dyn Any) -> Option<&T> + Send + Sync>;

/// A registry for implementations of library-defined traits, used for downcasting
/// arbitrary TableProviders into `ListingTableLike` and `Materialized` trait objects where possible.
///
/// This is used throughout the crate as a singleton to store all known implementations of `ListingTableLike` and `Materialized`.
/// By default, [`ListingTable`] is registered as a `ListingTableLike`.
struct TableTypeRegistry {
    listing_table_accessors: DashMap<TypeId, (&'static str, Downcaster<dyn ListingTableLike>)>,
    materialized_accessors: DashMap<TypeId, (&'static str, Downcaster<dyn Materialized>)>,
    decorator_accessors: DashMap<TypeId, (&'static str, Downcaster<dyn Decorator>)>,
}

impl Debug for TableTypeRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TableTypeRegistry")
            .field(
                "listing_table_accessors",
                &self
                    .listing_table_accessors
                    .iter()
                    .map(|r| r.value().0)
                    .collect_vec(),
            )
            .finish()
    }
}

impl Default for TableTypeRegistry {
    fn default() -> Self {
        let new = Self {
            listing_table_accessors: DashMap::new(),
            materialized_accessors: DashMap::new(),
            decorator_accessors: DashMap::new(),
        };
        new.register_listing_table::<ListingTable>();

        new
    }
}

impl TableTypeRegistry {
    fn register_listing_table<T: ListingTableLike>(&self) {
        self.listing_table_accessors.insert(
            TypeId::of::<T>(),
            (
                type_name::<T>(),
                Arc::new(|any| any.downcast_ref::<T>().map(|t| t as &dyn ListingTableLike)),
            ),
        );
    }

    fn register_materialized<T: Materialized>(&self) {
        self.materialized_accessors.insert(
            TypeId::of::<T>(),
            (
                type_name::<T>(),
                Arc::new(|any| any.downcast_ref::<T>().map(|t| t as &dyn Materialized)),
            ),
        );

        self.register_listing_table::<T>();
    }

    fn register_decorator<T: Decorator>(&self) {
        self.decorator_accessors.insert(
            TypeId::of::<T>(),
            (
                type_name::<T>(),
                Arc::new(|any| any.downcast_ref::<T>().map(|t| t as &dyn Decorator)),
            ),
        );
    }

    fn cast_to_listing_table<'a>(
        &'a self,
        table: &'a dyn TableProvider,
    ) -> Option<&'a dyn ListingTableLike> {
        self.listing_table_accessors
            .get(&table.as_any().type_id())
            .and_then(|r| r.value().1(table.as_any()))
    }

    fn cast_to_materialized<'a>(
        &'a self,
        table: &'a dyn TableProvider,
    ) -> Option<&'a dyn Materialized> {
        self.materialized_accessors
            .get(&table.as_any().type_id())
            .and_then(|r| r.value().1(table.as_any()))
    }

    fn cast_to_decorator<'a>(&'a self, table: &'a dyn TableProvider) -> Option<&'a dyn Decorator> {
        self.decorator_accessors
            .get(&table.as_any().type_id())
            .and_then(|r| r.value().1(table.as_any()))
    }
}
