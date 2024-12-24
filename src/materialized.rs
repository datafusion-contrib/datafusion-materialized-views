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
mod dependencies;

/// Pluggable metadata sources for incremental view maintenance
pub mod row_metadata;

/// A virtual table that exposes files in object storage.
pub mod file_metadata;

/// A UDF that parses Hive partition elements from object storage paths.
mod hive_partition;

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
/// into a [`ListingTableLike`] where possible.
pub fn register_listing_table<T: ListingTableLike>() {
    TABLE_TYPE_REGISTRY.register_listing_table::<T>();
}

/// Attempt to cast the given TableProvider into a [`ListingTableLike`].
/// If the table's type has not been registered using [`register_listing_table`], will return `None`.
pub fn cast_to_listing_table(table: &dyn TableProvider) -> Option<&dyn ListingTableLike> {
    TABLE_TYPE_REGISTRY.cast_to_listing_table(table)
}

/// A hive-partitioned table in object storage that is defined by a user-provided query.
pub trait Materialized: ListingTableLike {
    /// The query that defines this materialized view.
    fn query(&self) -> LogicalPlan;
}

type Downcaster<T> = Arc<dyn Fn(&dyn Any) -> Option<&T> + Send + Sync>;

/// A registry for implementations of [`ListingTableLike`], used for downcasting
/// arbitrary TableProviders into `dyn ListingTableLike` where possible.
///
/// This is used throughout the crate as a singleton to store all known implementations of `ListingTableLike`.
/// By default, [`ListingTable`] is registered.
struct TableTypeRegistry {
    listing_table_accessors: DashMap<TypeId, (&'static str, Downcaster<dyn ListingTableLike>)>,
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

    fn cast_to_listing_table<'a>(
        &'a self,
        table: &'a dyn TableProvider,
    ) -> Option<&'a dyn ListingTableLike> {
        self.listing_table_accessors
            .get(&table.as_any().type_id())
            .and_then(|r| r.value().1(table.as_any()))
    }
}
