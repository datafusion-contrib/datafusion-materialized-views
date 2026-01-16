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

/*!

This module contains code primarily used for view matching. We implement the view matching algorithm from [this paper](https://dsg.uwaterloo.ca/seminars/notes/larson-paper.pdf),
which provides a method for determining when one Select-Project-Join query can be rewritten in terms of another Select-Project-Join query.

The implementation is contained in [`SpjNormalForm::rewrite_from`]. The method can be summarized as follows:
1. Compute column equivalence classes for the query and the view.
2. Compute range intervals for the query and the view.
3. (Equijoin subsumption test) Check that each column equivalence class of the view is a subset of a column equivalence class of the query.
4. (Range subsumption test) Check that each range of the view contains the corresponding range from the query.
5. (Residual subsumption test) Check that every filter in the view that is not a column equivalence relation or a range filter matches a filter from the query.
6. Compute any compensating filters needed in order to restrict the view's rows to match the query.
7. Check that the output of the query, and the compensating filters, can be rewritten using the view's columns as inputs.

# Example

Consider the following table:

```sql
CREATE TABLE example (
    l_orderkey INT,
    l_partkey INT,
    l_shipdate DATE,
    l_quantity DOUBLE,
    l_extendedprice DOUBLE,
    o_custkey INT,
    o_orderkey INT,
    o_orderdate DATE,
    p_name VARCHAR,
    p_partkey INT,
)
```

And consider the follow view:

```sql
CREATE VIEW mv AS SELECT
    l_orderkey,
    o_custkey,
    l_partkey,
    l_shipdate, o_orderdate,
    l_quantity*l_extendedprice AS gross_revenue
FROM example
WHERE
    l_orderkey = o_orderkey AND
    l_partkey = p_partkey AND
    p_partkey >= 150 AND
    o_custkey >= 50 AND
    o_custkey <= 500 AND
    p_name LIKE '%abc%'
```

During analysis, we look at the implied equivalence classes and possible range of values for each equivalence class.
For this view, the following nontrivial equivalence classes are generated:
 * `{l_orderkey, o_orderkey}`
 * `{l_partkey, p_partkey}`

Note that all other columns have their own singleton equivalence classes, but are not shown here.
Likewise, the following nontrivial ranges are generated:
 * `150 <= {l_partkey, p_partkey} < inf`
 * `50 <= {o_custkey} <= 500`

The rest of the equivalence classes are considered to have ranges of (-inf, inf).
The remaining filter `p_name LIKE '%abc%'` is considered 'residual' as it is not a column equivalence nor a range filter.

Now consider the following query, which we will rewrite to use the view:

```sql
SELECT
    l_orderkey,
    o_custkey,
    l_partkey,
    l_quantity*l_extendedprice
FROM example
WHERE
    l_orderkey = o_orderkey AND
    l_partkey = p_partkey AND
    l_partkey >= 150 AND
    l_partkey <= 160 AND
    o_custkey = 123 AND
    o_orderdate = l_shipdate AND
    p_name like '%abc%' AND
    l_quantity*l_extendedprice > 100
````

This generates the following equivalence classes:
 * `{l_orderkey, o_orderkey}`
 * `{l_partkey, p_partkey}`
 * `{o_orderdate, l_shipdate}`

And the following ranges:
 * `150 <= {l_partkey, p_partkey} <= 160`
 * `123 <= {o_custkey} <= 123`

As before, we still have the residual filter `p_name LIKE '%abc'`. However, note that `l_quantity*l_extendedprice > 100` is also
a residual filter, as it is not a range filter on a column -- it's a range filter on a mathematical expression.

We perform the three subsumption tests:
 * Equijoin subsumption test:
   * View equivalence classes: `{l_orderkey, o_orderkey}, {l_partkey, p_partkey}`
   * Query equivalence classes: `{l_orderkey, o_orderkey}, {l_partkey, p_partkey}, {o_orderdate, l_shipdate}`
   * Every view equivalence class is a subset of one from the query, so the test is passed.
   * We generate the compensating filter `o_orderdate = l_shipdate`.
 * Range subsumption test:
   * View ranges:
     * `150 <= {l_partkey, p_partkey} < inf`
     * `50 <= {o_custkey} <= 500`
   * Query ranges:
     * `150 <= {l_partkey, p_partkey} <= 160`
     * `123 <= {o_custkey} <= 123`
   * Both of the view's ranges contain corresponding ranges from the query, therefore the test is passed.
   * Since they're both strict inclusions, we include them both as compensating filters.
 * Residual subsumption test:
   * View residuals: `p_name LIKE '%abc'`
   * Query residuals: `p_name LIKE '%abc'`, `l_quantity*l_extendedprice > 100`
   * Every view residual has a matching residual from the query, and the test is passed.
   * The leftover residual in the query, `l_quantity*l_extendedprice > 100`, is included as a compensating filter.

Ultimately we have the following compensating filters:
 * `o_orderdate = l_shipdate`
 * `150 <= {l_partkey, p_partkey} <= 160`
 * `123 <= {o_custkey} <= 123`
 * `l_quantity*l_extendedprice > 100`

The final check is to ensure that the output of our query can be computed from the view. This includes
any expressions used in the compensating filters.
This is a relatively simple check that mostly involves rewriting expressions to use columns from the view,
and ensuring that no references to the original tables are left.

This example is included as a unit test. After rewriting the query to use the view, the resulting plan looks like this:

```text
+---------------+----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------+
| plan_type     | plan                                                                                                                                                                                                     |
+---------------+----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------+
| logical_plan  | Projection: mv.l_orderkey AS l_orderkey, mv.o_custkey AS o_custkey, mv.l_partkey AS l_partkey, mv.gross_revenue AS example.l_quantity * example.l_extendedprice                                          |
|               |   Filter: mv.o_orderdate = mv.l_shipdate AND mv.l_partkey >= Int32(150) AND mv.l_partkey <= Int32(160) AND mv.o_custkey >= Int32(123) AND mv.o_custkey <= Int32(123) AND mv.gross_revenue > Float64(100) |
|               |     TableScan: mv projection=[l_orderkey, o_custkey, l_partkey, l_shipdate, o_orderdate, gross_revenue]                                                                                                  |
| physical_plan | ProjectionExec: expr=[l_orderkey@0 as l_orderkey, o_custkey@1 as o_custkey, l_partkey@2 as l_partkey, gross_revenue@5 as example.l_quantity * example.l_extendedprice]                                   |
|               |   CoalesceBatchesExec: target_batch_size=8192                                                                                                                                                            |
|               |     FilterExec: o_orderdate@4 = l_shipdate@3 AND l_partkey@2 >= 150 AND l_partkey@2 <= 160 AND o_custkey@1 >= 123 AND o_custkey@1 <= 123 AND gross_revenue@5 > 100                                       |
|               |       MemoryExec: partitions=16, partition_sizes=[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]                                                                                                        |
|               |                                                                                                                                                                                                          |
+---------------+----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------+
```

As one can see, all compensating filters are included, and the query only uses the view.

*/

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::Arc,
};

use datafusion_common::{
    tree_node::{Transformed, TransformedResult, TreeNode, TreeNodeRecursion, TreeNodeRewriter},
    Column, DFSchema, DataFusionError, ExprSchema, Result, ScalarValue, TableReference,
};
use datafusion_expr::{
    interval_arithmetic::{satisfy_greater, Interval},
    lit,
    utils::split_conjunction,
    BinaryExpr, Expr, LogicalPlan, LogicalPlanBuilder, Operator, TableScan, TableSource,
};
use itertools::Itertools;

/// A normalized representation of a plan containing only Select/Project/Join in the relational algebra sense.
/// In DataFusion terminology this also includes Filter nodes.
/// Joins are not currently supported, but are planned.
#[derive(Debug, Clone)]
pub struct SpjNormalForm {
    output_schema: Arc<DFSchema>,
    output_exprs: Vec<Expr>,
    referenced_tables: Vec<TableReference>,
    predicate: Predicate,
}

/// Rewrite an expression to re-use output columns from this plan, where possible.
impl TreeNodeRewriter for &SpjNormalForm {
    type Node = Expr;

    fn f_down(&mut self, node: Self::Node) -> Result<Transformed<Self::Node>> {
        Ok(match self.output_exprs.iter().position(|x| x == &node) {
            Some(idx) => Transformed::yes(Expr::Column(Column::new_unqualified(
                self.output_schema.field(idx).name().clone(),
            ))),
            None => Transformed::no(node),
        })
    }
}

impl SpjNormalForm {
    /// Schema of data output by this plan.
    pub fn output_schema(&self) -> &Arc<DFSchema> {
        &self.output_schema
    }

    /// Expressions output by this plan.
    /// These expressions can be used to rewrite this plan as a cross join followed by a projection;
    /// however, this does not include any filters in the original plan, so the result will be a superset.
    pub fn output_exprs(&self) -> &[Expr] {
        &self.output_exprs
    }

    /// All tables referenced in this plan.
    pub fn referenced_tables(&self) -> &[TableReference] {
        &self.referenced_tables
    }

    /// Analyze an existing `LogicalPlan` and rewrite it in select-project-join normal form.
    pub fn new(original_plan: &LogicalPlan) -> Result<Self> {
        let predicate = Predicate::new(original_plan)?;
        let output_exprs = get_output_exprs(original_plan)?
            .into_iter()
            .map(|expr| predicate.normalize_expr(expr))
            .collect();

        Ok(Self {
            output_schema: Arc::clone(original_plan.schema()),
            output_exprs,
            // Reuse referenced_tables collected during Predicate::new to avoid extra traversal
            referenced_tables: predicate.referenced_tables.clone(),
            predicate,
        })
    }

    /// Rewrite this plan as as selection/projection on top of another plan,
    /// which we use `qualifier` to refer to.
    /// This is useful for rewriting queries to use materialized views.
    pub fn rewrite_from(
        &self,
        mut other: &Self,
        qualifier: TableReference,
        source: Arc<dyn TableSource>,
    ) -> Result<Option<LogicalPlan>> {
        log::trace!("rewriting from {qualifier}");
        let mut new_output_exprs = Vec::with_capacity(self.output_exprs.len());
        // check that our output exprs are sub-expressions of the other one's output exprs
        for (i, output_expr) in self.output_exprs.iter().enumerate() {
            let new_output_expr = other
                .predicate
                .normalize_expr(output_expr.clone())
                .rewrite(&mut other)?
                .data;

            // Check that all references to the original tables have been replaced.
            // All remaining column expressions should be unqualified, which indicates
            // that they refer to the output of the sub-plan (in this case the view)
            if new_output_expr
                .column_refs()
                .iter()
                .any(|c| c.relation.is_some())
            {
                return Ok(None);
            }

            let column = &self.output_schema.columns()[i];
            new_output_exprs.push(
                new_output_expr.alias_qualified(column.relation.clone(), column.name.clone()),
            );
        }

        log::trace!("passed output rewrite");

        // Check the subsumption tests, and compute any auxiliary needed filter expressions.
        // If we pass all three subsumption tests, this plan's output is a subset of the other
        // plan's output.
        let ((eq_filters, range_filters), residual_filters) = match self
            .predicate
            .equijoin_subsumption_test(&other.predicate)
            .zip(self.predicate.range_subsumption_test(&other.predicate)?)
            .zip(self.predicate.residual_subsumption_test(&other.predicate))
        {
            None => return Ok(None),
            Some(filters) => filters,
        };

        log::trace!("passed subsumption tests");

        let all_filters = eq_filters
            .into_iter()
            .chain(range_filters)
            .chain(residual_filters)
            .map(|expr| expr.rewrite(&mut other).unwrap().data)
            .reduce(|a, b| a.and(b));

        if all_filters
            .as_ref()
            .map(|expr| expr.column_refs())
            .is_some_and(|columns| columns.iter().any(|c| c.relation.is_some()))
        {
            return Ok(None);
        }

        let mut builder = LogicalPlanBuilder::scan(qualifier, source, None)?;

        if let Some(filter) = all_filters {
            builder = builder.filter(filter)?;
        }

        builder.project(new_output_exprs)?.build().map(Some)
    }
}

/// Stores information on filters from a Select-Project-Join plan.
#[derive(Debug, Clone)]
struct Predicate {
    /// Full table schema, including all possible columns.
    schema: DFSchema,
    /// List of column equivalence classes.
    eq_classes: Vec<ColumnEquivalenceClass>,
    /// Reverse lookup by eq class elements
    eq_class_idx_by_column: HashMap<Column, usize>,
    /// Stores (possibly empty) intervals describing each equivalence class.
    ranges_by_equivalence_class: Vec<Option<Interval>>,
    /// Filter expressions that aren't column equality predicates or range filters.
    residuals: HashSet<Expr>,
    /// Tables referenced in this plan (collected during single-pass traversal)
    referenced_tables: Vec<TableReference>,
}

impl Predicate {
    /// Create a new Predicate by analyzing the given logical plan.
    /// Uses single-pass traversal to collect schema, columns, filters, and referenced tables.
    fn new(plan: &LogicalPlan) -> Result<Self> {
        let mut schema = DFSchema::empty();
        let mut columns_info: Vec<(Column, arrow::datatypes::DataType)> = Vec::new();
        let mut filters: Vec<Expr> = Vec::new();
        let mut referenced_tables: Vec<TableReference> = Vec::new();

        // Single traversal to collect everything
        plan.apply(|node| {
            match node {
                LogicalPlan::TableScan(scan) => {
                    // Collect referenced table
                    referenced_tables.push(scan.table_name.clone());

                    // Build schema
                    let new_schema = DFSchema::try_from_qualified_schema(
                        scan.table_name.clone(),
                        scan.source.schema().as_ref(),
                    )?;

                    // Collect columns with their data types
                    for (table_ref, field) in new_schema.iter() {
                        columns_info.push((
                            Column::new(table_ref.cloned(), field.name()),
                            field.data_type().clone(),
                        ));
                    }

                    // Merge schema
                    schema = if schema.fields().is_empty() {
                        new_schema
                    } else {
                        schema.join(&new_schema)?
                    };

                    // Collect filters from TableScan
                    filters.extend(scan.filters.iter().cloned());
                }
                LogicalPlan::Filter(filter) => {
                    filters.push(filter.predicate.clone());
                }
                LogicalPlan::Join(_join) => {
                    return Err(DataFusionError::Internal(
                        "joins are not supported yet".to_string(),
                    ));
                }
                LogicalPlan::Projection(_) => {}
                _ => {
                    return Err(DataFusionError::Plan(format!(
                        "unsupported logical plan: {}",
                        node.display()
                    )));
                }
            }
            Ok(TreeNodeRecursion::Continue)
        })?;

        // Initialize data structures with known capacity
        let n = columns_info.len();
        let mut eq_classes = Vec::with_capacity(n);
        let mut eq_class_idx_by_column = HashMap::with_capacity(n);
        let mut ranges_by_equivalence_class = Vec::with_capacity(n);

        for (i, (column, data_type)) in columns_info.into_iter().enumerate() {
            eq_classes.push(ColumnEquivalenceClass::new_singleton(column.clone()));
            eq_class_idx_by_column.insert(column, i);
            ranges_by_equivalence_class.push(Some(Interval::make_unbounded(&data_type)?));
        }

        let mut new = Self {
            schema,
            eq_classes,
            eq_class_idx_by_column,
            ranges_by_equivalence_class,
            residuals: HashSet::new(),
            referenced_tables,
        };

        // Process all collected filters
        for expr in filters.iter().flat_map(split_conjunction) {
            new.insert_conjuct(expr)?;
        }

        Ok(new)
    }

    fn class_for_column(&self, col: &Column) -> Option<&ColumnEquivalenceClass> {
        self.eq_class_idx_by_column
            .get(col)
            .and_then(|&idx| self.eq_classes.get(idx))
    }

    /// Add a new column equivalence
    fn add_equivalence(&mut self, c1: &Column, c2: &Column) -> Result<()> {
        match (
            self.eq_class_idx_by_column.get(c1),
            self.eq_class_idx_by_column.get(c2),
        ) {
            (None, None) => {
                // Make a new eq class [c1, c2]
                self.eq_classes
                    .push(ColumnEquivalenceClass::new([c1.clone(), c2.clone()]));
                self.ranges_by_equivalence_class
                    .push(Some(Interval::make_unbounded(
                        self.schema.field_from_column(c1).unwrap().data_type(),
                    )?));
            }

            // These two cases are just adding a column to an existing class
            (None, Some(&idx)) => {
                self.eq_classes[idx].columns.insert(c1.clone());
            }
            (Some(&idx), None) => {
                self.eq_classes[idx].columns.insert(c2.clone());
            }
            (Some(&i), Some(&j)) => {
                if i == j {
                    // The two columns are already in the same equivalence class.
                    return Ok(());
                }
                // We need to merge two existing column eq classes.

                // Delete the eq class with a larger index,
                // so that the other one has its position preserved.
                // Not necessary, but it's just a little simpler this way
                let (i, j) = if i < j { (i, j) } else { (j, i) };

                // Merge the deleted eq class with its new partner
                let new_columns = self.eq_classes.remove(j).columns;
                self.eq_classes[i].columns.extend(new_columns.clone());
                for column in new_columns {
                    self.eq_class_idx_by_column.insert(column, i);
                }
                // update all moved entries
                for idx in self.eq_class_idx_by_column.values_mut() {
                    if *idx > j {
                        *idx -= 1;
                    }
                }

                // Merge ranges
                // Now that we know the two equivalence classes are equal,
                // the new range is the intersection of the existing two ranges.
                self.ranges_by_equivalence_class[i] = self.ranges_by_equivalence_class[i]
                    .clone()
                    .zip(self.ranges_by_equivalence_class.remove(j))
                    .and_then(|(range, other_range)| range.intersect(other_range).transpose())
                    .transpose()?;
            }
        }

        Ok(())
    }

    /// Update range for a column's equivalence class
    fn add_range(&mut self, c: &Column, op: &Operator, value: &ScalarValue) -> Result<()> {
        // first coerce the value if needed
        let value = value.cast_to(self.schema.data_type(c)?)?;
        let range = self
            .eq_class_idx_by_column
            .get(c)
            .ok_or_else(|| {
                DataFusionError::Plan(format!("column {c} not found in equivalence classes"))
            })
            .and_then(|&idx| {
                self.ranges_by_equivalence_class
                    .get_mut(idx)
                    .ok_or_else(|| {
                        DataFusionError::Plan(format!(
                            "range not found class not found for column {c} with equivalence class {:?}", self.eq_classes.get(idx)
                        ))
                    })
            })?;

        let new_range = match op {
            Operator::Eq => Interval::try_new(value.clone(), value.clone()),
            Operator::LtEq => {
                Interval::try_new(ScalarValue::try_from(value.data_type())?, value.clone())
            }
            Operator::GtEq => {
                Interval::try_new(value.clone(), ScalarValue::try_from(value.data_type())?)
            }

            // Note: This is a roundabout way (read: hack) to construct an open Interval.
            // DataFusion's Interval type represents closed intervals,
            // so handling of open intervals is done by adding/subtracting the smallest increment.
            // However, there is not really a public API to do this,
            // other than the satisfy_greater method.
            Operator::Lt => {
                let range_val = match satisfy_greater(
                    &Interval::try_new(value.clone(), value.clone())?,
                    &Interval::make_unbounded(&value.data_type())?,
                    true,
                )? {
                    Some((_, range)) => range,
                    None => {
                        *range = None;
                        return Ok(());
                    }
                };
                // If the type is not discrete (e.g. Utf8), satisfy_greater may return an unchanged value.
                // This means the interval could not be tightened and it is unsafe to produce a closed interval
                if range_val.upper() == &value {
                    Err(DataFusionError::Plan(
                        "cannot represent strict inequality as closed interval for non-discrete types".to_string(),
                    ))
                } else {
                    Ok(range_val)
                }
            }
            Operator::Gt => {
                let range_val = match satisfy_greater(
                    &Interval::make_unbounded(&value.data_type())?,
                    &Interval::try_new(value.clone(), value.clone())?,
                    true,
                )? {
                    Some((range, _)) => range,
                    None => {
                        *range = None;
                        return Ok(());
                    }
                };
                if range_val.lower() == &value {
                    Err(DataFusionError::Plan(
                        "cannot represent strict inequality as closed interval for non-discrete types".to_string(),
                    ))
                } else {
                    Ok(range_val)
                }
            }
            _ => Err(DataFusionError::Plan(
                "unsupported binary expression".to_string(),
            )),
        }?;

        *range = match range {
            None => Some(new_range),
            Some(range) => range.intersect(new_range)?,
        };

        Ok(())
    }

    /// Add a generic filter expression to our collection of filters.
    /// A conjunct is a term T_i of an expression T_1 AND T_2 AND T_3 AND ...
    fn insert_conjuct(&mut self, expr: &Expr) -> Result<()> {
        match expr {
            Expr::BinaryExpr(BinaryExpr { left, op, right }) => {
                self.insert_binary_expr(left, *op, right)?;
            }
            Expr::Not(e) => match e.as_ref() {
                Expr::BinaryExpr(BinaryExpr { left, op, right }) => {
                    if let Some(negated) = op.negate() {
                        self.insert_binary_expr(left, negated, right)?;
                    } else {
                        self.residuals.insert(expr.clone());
                    }
                }
                _ => {
                    self.residuals.insert(expr.clone());
                }
            },
            _ => {
                self.residuals.insert(expr.clone());
            }
        }

        Ok(())
    }

    /// Add a binary expression to our collection of filters.
    fn insert_binary_expr(&mut self, left: &Expr, op: Operator, right: &Expr) -> Result<()> {
        match (left, op, right) {
            (Expr::Column(c), op, Expr::Literal(v, _)) => {
                if let Err(e) = self.add_range(c, &op, v) {
                    // Add a range can fail in some cases, so just fallthrough
                    log::debug!("failed to add range filter: {e}");
                } else {
                    return Ok(());
                }
            }
            (Expr::Literal(_, _), op, Expr::Column(_)) => {
                if let Some(swapped) = op.swap() {
                    return self.insert_binary_expr(right, swapped, left);
                }
            }
            // update eq classes & merge ranges by eq class
            (Expr::Column(c1), Operator::Eq, Expr::Column(c2)) => {
                self.add_equivalence(c1, c2)?;
                return Ok(());
            }
            _ => {}
        }

        self.residuals.insert(Expr::BinaryExpr(BinaryExpr {
            left: Box::new(left.clone()),
            op,
            right: Box::new(right.clone()),
        }));

        Ok(())
    }

    /// Test that all column equivalence classes of `other` are subsumed by one from `self`.
    /// This is called the 'equijoin' subsumption test because column equivalences often
    /// result from join predicates.
    /// Returns any compensating column equality predicates that should be applied to
    /// make this plan match the output of the other one.
    fn equijoin_subsumption_test(&self, other: &Self) -> Option<Vec<Expr>> {
        let mut new_equivalences = vec![];
        // check that all equivalence classes of `other` are contained in one from `self`
        for other_class in &other.eq_classes {
            let (representative, eq_class) = match other_class
                .columns
                .iter()
                .find_map(|c| self.class_for_column(c).map(|class| (c, class)))
            {
                // We don't contain any columns from this eq class.
                // Technically this is alright if the equivalence class is trivial,
                // because we're allowed to be a subset of the other plan.
                // If the equivalence class is nontrivial then we can't compute
                // compensating filters because we lack the columns that would be
                // used in the filter.
                None if other_class.columns.len() == 1 => continue,
                // We do contain columns from this eq class.
                Some(tuple) => tuple,
                // We don't contain columns from this eq class and the
                // class is nontrivial.
                _ => return None,
            };

            if !other_class.columns.is_subset(&eq_class.columns) {
                return None;
            }

            for column in eq_class.columns.difference(&other_class.columns) {
                new_equivalences
                    .push(Expr::Column(representative.clone()).eq(Expr::Column(column.clone())))
            }
        }

        log::trace!("passed equijoin subsumption test");

        Some(new_equivalences)
    }

    /// Test that all range filters of `self` are contained in one from `other`.
    /// This includes equality comparisons, which map to ranges of the form [v, v]
    /// for some value v.
    /// Returns any compensating range filters that should be applied to this plan
    /// to make its output match the other one.
    fn range_subsumption_test(&self, other: &Self) -> Result<Option<Vec<Expr>>> {
        let mut extra_range_filters = vec![];
        for (eq_class, range) in self
            .eq_classes
            .iter()
            .zip(self.ranges_by_equivalence_class.iter())
        {
            let range = match range {
                None => {
                    // empty; it's always contained in another range
                    // also this range is never satisfiable, so it's always False
                    extra_range_filters.push(lit(false));
                    continue;
                }
                Some(range) => range,
            };

            let (other_column, other_range) = match eq_class.columns.iter().find_map(|c| {
                other.eq_class_idx_by_column.get(c).and_then(|&idx| {
                    other.ranges_by_equivalence_class[idx]
                        .as_ref()
                        .map(|range| (other.eq_classes[idx].columns.first().unwrap(), range))
                })
            }) {
                None => return Ok(None),
                Some(range) => range,
            };

            if other_range.contains(range)? != Interval::TRUE {
                return Ok(None);
            }

            if range.contains(other_range)? != Interval::TRUE {
                if !(range.lower().is_null() || range.upper().is_null())
                    && (range.lower().eq(range.upper()))
                {
                    // Certain datafusion code paths only work if eq expressions are preserved
                    // that is, col >= val AND col <= val is not treated the same as col = val
                    // We special-case this to make sure everything works as expected.
                    // todo: could this be a logical optimizer?
                    extra_range_filters.push(Expr::BinaryExpr(BinaryExpr {
                        left: Box::new(Expr::Column(other_column.clone())),
                        op: Operator::Eq,
                        right: Box::new(Expr::Literal(range.lower().clone(), None)),
                    }))
                } else {
                    if !range.lower().is_null() {
                        extra_range_filters.push(Expr::BinaryExpr(BinaryExpr {
                            left: Box::new(Expr::Column(other_column.clone())),
                            op: Operator::GtEq,
                            right: Box::new(Expr::Literal(range.lower().clone(), None)),
                        }))
                    }

                    if !range.upper().is_null() {
                        extra_range_filters.push(Expr::BinaryExpr(BinaryExpr {
                            left: Box::new(Expr::Column(other_column.clone())),
                            op: Operator::LtEq,
                            right: Box::new(Expr::Literal(range.upper().clone(), None)),
                        }))
                    }
                }
            }
        }

        log::trace!("passed range subsumption test");

        Ok(Some(extra_range_filters))
    }

    /// Test that any "residual" filters (not column equivalence or range filters) from
    /// `other` have matching entries in `self`.
    /// For example, a residual filter might look like `x * y > 100`, as this expression
    /// is neither a column equivalence nor a range filter (importantly, not a range filter
    /// directly on a column).)
    /// This ensures that `self` is a subset of `other`.
    /// Return any residual filters in this plan that are not in the other one.
    fn residual_subsumption_test(&self, other: &Self) -> Option<Vec<Expr>> {
        let [self_residuals, other_residuals] = [self.residuals.clone(), other.residuals.clone()]
            .map(|set| {
                set.into_iter()
                    .map(|r| self.normalize_expr(r.clone()))
                    .collect::<HashSet<Expr>>()
            });

        if !self_residuals.is_superset(&other_residuals) {
            return None;
        }

        log::trace!("passed residual subsumption test");

        Some(
            self_residuals
                .difference(&other.residuals)
                .cloned()
                .collect_vec(),
        )
    }

    /// Rewrite all expressions in terms of their normal representatives
    /// with respect to this predicate's equivalence classes.
    fn normalize_expr(&self, e: Expr) -> Expr {
        e.transform(&|e| {
            let c = match e {
                Expr::Column(c) => c,
                Expr::Alias(alias) => return Ok(Transformed::yes(alias.expr.as_ref().clone())),
                _ => return Ok(Transformed::no(e)),
            };

            if let Some(eq_class) = self.class_for_column(&c) {
                Ok(Transformed::yes(Expr::Column(
                    eq_class.columns.first().unwrap().clone(),
                )))
            } else {
                Ok(Transformed::no(Expr::Column(c)))
            }
        })
        .map(|t| t.data)
        // No chance of error since we never return Err -- this unwrap is safe
        .unwrap()
    }
}

/// A collection of columns that are all considered to be equivalent.
/// In some cases we normalize expressions so that they use the "normal" representative
/// in place of any other columns in the class.
/// This normal representative is chosen arbitrarily.
#[derive(Debug, Clone, Default)]
struct ColumnEquivalenceClass {
    // first element is the normal representative of the equivalence class
    columns: BTreeSet<Column>,
}

impl ColumnEquivalenceClass {
    fn new(columns: impl IntoIterator<Item = Column>) -> Self {
        Self {
            columns: BTreeSet::from_iter(columns),
        }
    }

    fn new_singleton(column: Column) -> Self {
        Self {
            columns: BTreeSet::from([column]),
        }
    }
}

/// For each field in the plan's schema, get an expression that represents the field's definition.
/// Furthermore, normalize all expressions so that the only column expressions refer to directly to tables,
/// not alias subqueries or child plans.
///
/// This essentially is equivalent to rewriting the query as a projection against a cross join.
fn get_output_exprs(plan: &LogicalPlan) -> Result<Vec<Expr>> {
    use datafusion_expr::logical_plan::*;

    let output_exprs = match plan {
        // ignore filter, sort, and limit
        // they don't change the schema or the definitions
        LogicalPlan::Filter(_)
        | LogicalPlan::Sort(_)
        | LogicalPlan::Limit(_)
        | LogicalPlan::Distinct(_) => return get_output_exprs(plan.inputs()[0]),
        LogicalPlan::Projection(Projection { expr, .. }) => Ok(expr.clone()),
        LogicalPlan::Aggregate(Aggregate {
            group_expr,
            aggr_expr,
            ..
        }) => Ok(Vec::from_iter(
            group_expr.iter().chain(aggr_expr.iter()).cloned(),
        )),
        LogicalPlan::Window(Window {
            input, window_expr, ..
        }) => Ok(Vec::from_iter(
            input
                .schema()
                .fields()
                .iter()
                .map(|field| Expr::Column(Column::new_unqualified(field.name())))
                .chain(window_expr.iter().cloned()),
        )),
        // if it's a table scan, just exit early with explicit return
        LogicalPlan::TableScan(table_scan) => {
            return Ok(get_table_scan_columns(table_scan)?
                .into_iter()
                .map(Expr::Column)
                .collect())
        }
        LogicalPlan::Unnest(unnest) => Ok(unnest
            .schema
            .columns()
            .into_iter()
            .map(Expr::Column)
            .collect()),
        LogicalPlan::Join(join) => Ok(join
            .left
            .schema()
            .columns()
            .into_iter()
            .chain(join.right.schema().columns())
            .map(Expr::Column)
            .collect_vec()),
        LogicalPlan::SubqueryAlias(sa) => return get_output_exprs(&sa.input),
        _ => Err(DataFusionError::NotImplemented(format!(
            "Logical plan not supported: {}",
            plan.display()
        ))),
    }?;

    flatten_exprs(output_exprs, plan)
}

/// Recursively normalize expressions so that any columns refer directly to tables and not subqueries.
fn flatten_exprs(exprs: Vec<Expr>, parent: &LogicalPlan) -> Result<Vec<Expr>> {
    if matches!(parent, LogicalPlan::TableScan(_)) {
        return Ok(exprs);
    }

    let schemas = parent
        .inputs()
        .iter()
        .map(|input| input.schema().as_ref())
        .collect_vec();
    let using_columns = parent.using_columns()?;

    let output_exprs_by_child = parent
        .inputs()
        .into_iter()
        .map(get_output_exprs)
        .collect::<Result<Vec<_>>>()?;

    exprs
        .into_iter()
        .map(|expr| {
            expr.transform_up(&|e| match e {
                // if the relation is None, it's a column referencing one of the child plans
                // if the relation is Some, it's a column of a table (most likely) and can be ignored since it's a leaf node
                // (technically it can also refer to an aliased subquery)
                Expr::Column(col) => {
                    // Figure out which child the column belongs to
                    let col = {
                        let col = if let LogicalPlan::SubqueryAlias(sa) = parent {
                            // If the parent is an aliased subquery, with the alias 'x',
                            // any expressions of the form `x.column1`
                            // refer to `column` in the input
                            if col.relation.as_ref() == Some(&sa.alias) {
                                Column::new_unqualified(col.name)
                            } else {
                                // All other columns are assumed to be leaf nodes (direct references to tables)
                                return Ok(Transformed::no(Expr::Column(col)));
                            }
                        } else {
                            col
                        };

                        col.normalize_with_schemas_and_ambiguity_check(&[&schemas], &using_columns)?
                    };

                    // first schema that matches column
                    // the check from earlier ensures that this will always be Some
                    // and that there should be only one schema that matches
                    // (except if it is a USING column, in which case we can pick any match equivalently)
                    let (child_idx, expr_idx) = schemas
                        .iter()
                        .enumerate()
                        .find_map(|(schema_idx, schema)| {
                            Some(schema_idx).zip(schema.maybe_index_of_column(&col))
                        })
                        .unwrap();

                    Ok(Transformed::yes(
                        output_exprs_by_child[child_idx][expr_idx].clone(),
                    ))
                }
                _ => Ok(Transformed::no(e)),
            })
            .data()
        })
        .collect()
}

/// Return the columns output by this [`TableScan`].
fn get_table_scan_columns(scan: &TableScan) -> Result<Vec<Column>> {
    let fields = {
        let mut schema = scan.source.schema().as_ref().clone();
        if let Some(ref p) = scan.projection {
            schema = schema.project(p)?;
        }
        schema.fields
    };

    Ok(fields
        .into_iter()
        .map(|field| Column::new(Some(scan.table_name.to_owned()), field.name()))
        .collect())
}

#[cfg(test)]
mod test {
    use arrow::compute::concat_batches;
    use datafusion::{
        datasource::provider_as_source,
        prelude::{SessionConfig, SessionContext},
    };
    use datafusion_common::{DataFusionError, Result};
    use datafusion_sql::TableReference;
    use tempfile::tempdir;

    use super::SpjNormalForm;

    async fn setup() -> Result<SessionContext> {
        let ctx = SessionContext::new_with_config(
            SessionConfig::new()
                .set_bool("datafusion.execution.parquet.pushdown_filters", true)
                .set_bool("datafusion.explain.logical_plan_only", true),
        );

        let t1_path = tempdir()?;

        // Create external table to exercise parquet filter pushdown.
        // This will put the filters directly inside the `TableScan` node.
        // This is important because `TableScan` can have filters on
        // columns not in its own output.
        ctx.sql(&format!(
            "
                CREATE EXTERNAL TABLE t1 (
                    column1 VARCHAR,
                    column2 BIGINT,
                    column3 CHAR
                )
                STORED AS PARQUET
                LOCATION '{}'",
            t1_path.path().to_string_lossy()
        ))
        .await
        .map_err(|e| e.context("setup `t1` table"))?
        .collect()
        .await?;

        ctx.sql(
            "INSERT INTO t1 VALUES
            ('2021', 3, 'A'),
            ('2022', 4, 'B'),
            ('2023', 5, 'C')",
        )
        .await
        .map_err(|e| e.context("parse `t1` table ddl"))?
        .collect()
        .await?;

        ctx.sql(
            "CREATE TABLE example (
                l_orderkey INT,
                l_partkey INT,
                l_shipdate DATE,
                l_quantity DOUBLE,
                l_extendedprice DOUBLE,
                o_custkey INT,
                o_orderkey INT,
                o_orderdate DATE,
                p_name VARCHAR,
                p_partkey INT
            )",
        )
        .await
        .map_err(|e| e.context("parse `example` table ddl"))?
        .collect()
        .await?;

        Ok(ctx)
    }

    struct TestCase {
        name: &'static str,
        base: &'static str,
        query: &'static str,
    }

    async fn run_test(case: &TestCase) -> Result<()> {
        let context = setup()
            .await
            .map_err(|e| e.context("setup test environment"))?;

        let base_plan = context.sql(case.base).await?.into_optimized_plan()?; // Optimize plan to eliminate unnormalized wildcard exprs
        let base_normal_form = SpjNormalForm::new(&base_plan)?;

        context
            .sql(&format!("CREATE TABLE mv AS {}", case.base))
            .await?
            .collect()
            .await?;

        let query_plan = context.sql(case.query).await?.into_optimized_plan()?;
        let query_normal_form = SpjNormalForm::new(&query_plan)?;

        for plan in [&base_plan, &query_plan] {
            context
                .execute_logical_plan(plan.clone())
                .await?
                .explain(false, false)?
                .show()
                .await?;
        }

        let table_ref = TableReference::bare("mv");
        let rewritten = query_normal_form
            .rewrite_from(
                &base_normal_form,
                table_ref.clone(),
                provider_as_source(context.table_provider(table_ref).await?),
            )?
            .ok_or(DataFusionError::Internal(
                "expected rewrite to succeed".to_string(),
            ))?;

        context
            .execute_logical_plan(rewritten.clone())
            .await?
            .explain(false, false)?
            .show()
            .await?;

        assert_eq!(rewritten.schema().as_ref(), query_plan.schema().as_ref());

        let expected = concat_batches(
            &query_plan.schema().inner().clone(),
            &context
                .execute_logical_plan(query_plan)
                .await?
                .collect()
                .await?,
        )?;

        let result = concat_batches(
            &rewritten.schema().inner().clone(),
            &context
                .execute_logical_plan(rewritten)
                .await?
                .collect()
                .await?,
        )?;

        assert_eq!(result, expected);

        Ok(())
    }

    #[tokio::test]
    async fn test_rewrite() -> Result<()> {
        let _ = env_logger::builder().is_test(true).try_init();
        let cases = vec![
            TestCase {
                name: "simple selection",
                base: "SELECT * FROM t1",
                query: "SELECT column1, column2 FROM t1",
            },
            TestCase {
                name: "selection with equality predicate",
                base: "SELECT * FROM t1",
                query: "SELECT column1, column2 FROM t1 WHERE column1 = column3",
            },
            TestCase {
                name: "selection with range filter",
                base: "SELECT * FROM t1 WHERE column2 > 3",
                query: "SELECT column1, column2 FROM t1 WHERE column2 > 4",
            },
            TestCase {
                name: "nontrivial projection",
                base: "SELECT concat(column1, column2), column2 FROM t1",
                query: "SELECT concat(column1, column2) FROM t1",
            },
            TestCase {
                name: "range filter + equality predicate",
                base:
                "SELECT column1, column2 FROM t1 WHERE column1 = column3 AND column1 >= '2022'",
                query:
                // Since column1 = column3 in the original view,
                // we are allowed to substitute column1 for column3 and vice versa.
                "SELECT column2, column3 FROM t1 WHERE column1 = column3 AND column3 >= '2023'",
            },
            TestCase {
                name: "range filter with inequality on non-discrete type",
                base: "SELECT * FROM t1",
                query: "SELECT column1 FROM t1 WHERE column1 < '2022'",
            },
            TestCase {
                name: "duplicate expressions (X-209)",
                base: "SELECT * FROM t1",
                query:
                    "SELECT column1, NULL AS column2, NULL AS column3, column3 AS column4 FROM t1",
            },
            TestCase {
                name: "example from paper",
                base: "\
                SELECT
                    l_orderkey,
                    o_custkey,
                    l_partkey,
                    l_shipdate, o_orderdate,
                    l_quantity*l_extendedprice AS gross_revenue
                FROM example
                WHERE
                    l_orderkey = o_orderkey AND
                    l_partkey = p_partkey AND
                    p_partkey >= 150 AND
                    o_custkey >= 50 AND
                    o_custkey <= 500 AND
                    p_name LIKE '%abc%'
                ",
                query: "SELECT
                    l_orderkey,
                    o_custkey,
                    l_partkey,
                    l_quantity*l_extendedprice
                FROM example
                WHERE
                    l_orderkey = o_orderkey AND
                    l_partkey = p_partkey AND
                    l_partkey >= 150 AND
                    l_partkey <= 160 AND
                    o_custkey = 123 AND
                    o_orderdate = l_shipdate AND
                    p_name like '%abc%' AND
                    l_quantity*l_extendedprice > 100
                ",
            },
            TestCase {
                name: "naked table scan with pushed down filters",
                base: "SELECT column1 FROM t1 WHERE column2 <= 3",
                query: "SELECT FROM t1 WHERE column2 <= 3",
            },
        ];

        for case in cases {
            println!("executing test: {}", case.name);
            run_test(&case).await.map_err(|e| e.context(case.name))?;
        }

        Ok(())
    }
}
