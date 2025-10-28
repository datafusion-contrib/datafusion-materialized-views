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

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::sync::Arc;
use std::time::Duration;

use datafusion::datasource::provider_as_source;
use datafusion::datasource::TableProvider;
use datafusion::prelude::SessionContext;
use datafusion_common::Result as DfResult;
use datafusion_expr::LogicalPlan;
use datafusion_materialized_views::rewrite::normal_form::SpjNormalForm;
use datafusion_sql::TableReference;
use tokio::runtime::Builder;

// Utility: generate CREATE TABLE SQL with n columns named c0..c{n-1}
fn make_create_table_sql(table_name: &str, ncols: usize) -> String {
    let cols = (0..ncols)
        .map(|i| format!("c{} INT", i))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CREATE TABLE {table} ({cols})",
        table = table_name,
        cols = cols
    )
}

// Utility: generate a base SELECT that projects all columns and has a couple filters
fn make_base_sql(table_name: &str, ncols: usize) -> String {
    let cols = (0..ncols)
        .map(|i| format!("c{}", i))
        .collect::<Vec<_>>()
        .join(", ");
    let mut where_clauses = vec![];
    if ncols > 0 {
        where_clauses.push("c0 >= 0".to_string());
    }
    if ncols > 1 {
        where_clauses.push("c0 + c1 >= 0".to_string());
    }
    let where_part = if where_clauses.is_empty() {
        "".to_string()
    } else {
        format!(" WHERE {}", where_clauses.join(" AND "))
    };
    format!("SELECT {cols} FROM {table}{where}", cols = cols, table = table_name, where = where_part)
}

// Utility: generate a query that is stricter and selects subset (so rewrite_from has a chance)
fn make_query_sql(table_name: &str, ncols: usize) -> String {
    let take = std::cmp::max(1, ncols / 2);
    let cols = (0..take)
        .map(|i| format!("c{}", i))
        .collect::<Vec<_>>()
        .join(", ");

    let mut where_clauses = vec![];
    if ncols > 0 {
        where_clauses.push("c0 >= 10".to_string());
    }
    if ncols > 1 {
        where_clauses.push("c0 * c1 > 100".to_string());
    }
    if ncols > 10 {
        where_clauses.push(format!("c{} >= 0", ncols - 1));
    }

    let where_part = if where_clauses.is_empty() {
        "".to_string()
    } else {
        format!(" WHERE {}", where_clauses.join(" AND "))
    };

    format!("SELECT {cols} FROM {table}{where}", cols = cols, table = table_name, where = where_part)
}

// Build fixture: create SessionContext, the table, then return LogicalPlans for base & query and table provider
fn build_fixture_for_cols(
    rt: &tokio::runtime::Runtime,
    ncols: usize,
) -> DfResult<(LogicalPlan, LogicalPlan, Arc<dyn TableProvider>)> {
    rt.block_on(async move {
        let ctx = SessionContext::new();

        // create table
        let table_name = "t";
        let create_sql = make_create_table_sql(table_name, ncols);
        ctx.sql(&create_sql).await?.collect().await?; // create table in catalog

        // base and query plans (optimize to normalize)
        let base_sql = make_base_sql(table_name, ncols);
        let query_sql = make_query_sql(table_name, ncols);

        let base_df = ctx.sql(&base_sql).await?;
        let base_plan = base_df.into_optimized_plan()?;

        let query_df = ctx.sql(&query_sql).await?;
        let query_plan = query_df.into_optimized_plan()?;

        // get table provider (Arc<dyn TableProvider>)
        let table_ref = TableReference::bare(table_name);
        let provider: Arc<dyn TableProvider> = ctx.table_provider(table_ref.clone()).await?;

        Ok((base_plan, query_plan, provider))
    })
}

// Criterion benchmark
fn criterion_benchmark(c: &mut Criterion) {
    // columns to test
    let col_cases = vec![1usize, 10, 20, 40, 80, 160, 320];

    // build a tokio runtime that's broadly compatible
    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let mut group = c.benchmark_group("view_matcher_spj");
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(5));
    group.sample_size(30);

    for &ncols in &col_cases {
        // Build fixture
        let (base_plan, query_plan, provider) =
            build_fixture_for_cols(&rt, ncols).expect("fixture");

        // Measure SpjNormalForm::new for base_plan and query_plan separately
        let id_base = BenchmarkId::new("spj_normal_form_new", format!("cols={}", ncols));
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(id_base, &base_plan, |b, plan| {
            b.iter(|| {
                let _nf = SpjNormalForm::new(plan).unwrap();
            });
        });

        let id_query_nf = BenchmarkId::new("spj_normal_form_new_query", format!("cols={}", ncols));
        group.bench_with_input(id_query_nf, &query_plan, |b, plan| {
            b.iter(|| {
                let _nf = SpjNormalForm::new(plan).unwrap();
            });
        });

        // Precompute normal forms once (to measure rewrite_from cost only)
        let base_nf = SpjNormalForm::new(&base_plan).expect("base_nf");
        let query_nf = SpjNormalForm::new(&query_plan).expect("query_nf");

        // qualifier for rewrite_from and a source created from the provider
        let qualifier = TableReference::bare("mv");
        let source = provider_as_source(Arc::clone(&provider));

        // Benchmark rewrite_from (this is the heavy check)
        let id_rewrite = BenchmarkId::new("rewrite_from", format!("cols={}", ncols));
        group.bench_with_input(id_rewrite, &ncols, |b, &_n| {
            b.iter(|| {
                let _res = query_nf.rewrite_from(&base_nf, qualifier.clone(), Arc::clone(&source));
            });
        });
    }

    group.finish();
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
