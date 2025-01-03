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
use datafusion_common::DataFusionError;
use datafusion_sql::ResolvedTableReference;

/// List every table in the catalog list.
/// Always returns a fully qualified table reference.
pub async fn list_tables(
    catalog_list: &dyn CatalogProviderList,
) -> Result<Vec<(ResolvedTableReference, Arc<dyn TableProvider>)>, DataFusionError> {
    use datafusion_common::Result;
    use futures::stream::{self, StreamExt, TryStreamExt};

    let catalogs_by_name = catalog_list
        .catalog_names()
        .into_iter()
        .map(|catalog_name| {
            catalog_list
                .catalog(&catalog_name)
                .ok_or(DataFusionError::Internal(format!(
                    "could not find named catalog: {catalog_name}"
                )))
                .map(|catalog| (catalog, catalog_name))
        })
        .collect::<Result<Vec<_>>>()?;

    let schemas_by_catalog_and_schema_name = catalogs_by_name
        .into_iter()
        .flat_map(|(catalog, catalog_name)| {
            catalog.schema_names().into_iter().map(move |schema_name| {
                catalog
                    .schema(&schema_name)
                    .ok_or(DataFusionError::Internal(format!(
                        "could not find named schema: {catalog_name}.{schema_name}"
                    )))
                    .map(|schema| (schema, catalog_name.clone(), schema_name))
            })
        })
        .collect::<Result<Vec<_>>>()?;

    stream::iter(schemas_by_catalog_and_schema_name)
        .flat_map(|(schema, catalog_name, schema_name)| {
            stream::iter(schema.table_names().into_iter().map(move |table_name| {
                (
                    schema.clone(),
                    ResolvedTableReference {
                        catalog: catalog_name.clone().into(),
                        schema: schema_name.clone().into(),
                        table: table_name.into(),
                    },
                )
            }))
        })
        .then(|(schema, table_ref)| async move {
            schema
                .table(&table_ref.table)
                .await?
                .ok_or(DataFusionError::Internal(format!(
                    "could not find named table: {table_ref}"
                )))
                .map(|table| (table_ref, table))
        })
        .try_collect()
        .await
}
