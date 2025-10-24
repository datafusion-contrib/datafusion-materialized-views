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

use std::sync::Arc;

use datafusion::catalog::{CatalogProviderList, TableProvider};
use datafusion_common::{DataFusionError, Result};
use datafusion_sql::ResolvedTableReference;

/// Retrieves a table from the catalog list given a resolved table reference.
pub fn get_table(
    catalog_list: &dyn CatalogProviderList,
    table_ref: &ResolvedTableReference,
) -> Result<Arc<dyn TableProvider>> {
    let catalog = catalog_list
        .catalog(table_ref.catalog.as_ref())
        .ok_or_else(|| DataFusionError::Plan(format!("no such catalog {}", table_ref.catalog)))?;

    let schema = catalog
        .schema(table_ref.schema.as_ref())
        .ok_or_else(|| DataFusionError::Plan(format!("no such schema {}", table_ref.schema)))?;

    // NOTE: this is bad, we are calling async code in a sync context.
    // We should file an issue about async in UDTFs.
    // See: https://github.com/apache/datafusion/issues/17663
    futures::executor::block_on(schema.table(table_ref.table.as_ref()))
        .map_err(|e| e.context(format!("couldn't get table '{}'", table_ref.table)))?
        .ok_or_else(|| DataFusionError::Plan(format!("no such table {}", table_ref.schema)))
}
