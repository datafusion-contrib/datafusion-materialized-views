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

use arrow::array::{Array, StringArray, StringBuilder};
use datafusion::arrow::datatypes::DataType;

use datafusion_common::{DataFusionError, Result, ScalarValue};
use datafusion_expr::{
    expr::ScalarFunction, ColumnarValue, Expr, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl,
    Signature, TypeSignature, Volatility,
};

pub static HIVE_PARTITION_UDF_NAME: &str = "hive_partition";

/// A DataFusion UDF with the following signature:
///
/// ```ignore
///     hive_partition(file_path: Utf8, partition_column: Utf8, null_if_missing: bool = false) -> Utf8
/// ```
///
/// that extracts the partition column from a path, assuming the file path follows the Hive partitioning scheme.
/// Partition values are returned as strings;
/// use the ARROW_CAST function to coerce the partition values into the correct data type.
///
/// # `null_if_missing`
///
/// By default, `hive_partition` throws an error if the named partition column is absent.
/// One can instead return null in such cases by passing a third argument:
/// ```sql
///     hive_partition(<path>, <column_name>, true)
/// ```
/// which will not throw an error if <column_name> is absent from <path>.
///
/// # Examples
///
/// ```sql
/// SELECT
///     column_name,
///     hive_partition(
///         's3://sip/trades/year=2006/month=01/day=02/trades-2006-01-02.parquet',
///         column_name
///     ) AS partition_value
/// FROM (VALUES ('year'), ('month'), ('day')) AS partition_columns (column_name);
///
/// // +-------------+-----------------+
/// // | column_name | partition_value |
/// // +-------------+-----------------+
/// // | year        | 2006            |
/// // | month       | 01              |
/// // | day         | 02              |
/// // +-------------+-----------------+
/// ```
pub fn hive_partition_udf() -> ScalarUDF {
    let signature = Signature::one_of(
        vec![
            TypeSignature::Uniform(2, vec![DataType::Utf8]),
            TypeSignature::Exact(vec![DataType::Utf8, DataType::Utf8, DataType::Boolean]),
        ],
        Volatility::Immutable,
    );

    let udf_impl = HivePartitionUdf { signature };
    ScalarUDF::new_from_impl(udf_impl)
}

#[derive(Debug, Hash, PartialEq, Eq)]
struct HivePartitionUdf {
    pub signature: Signature,
}

impl ScalarUDFImpl for HivePartitionUdf {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn name(&self) -> &str {
        HIVE_PARTITION_UDF_NAME
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> Result<DataType> {
        Ok(DataType::Utf8)
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        let values = args.args;
        let null_if_missing = values
            .get(2)
            .map(|val| match val {
                ColumnarValue::Scalar(ScalarValue::Boolean(Some(b))) => Ok(*b),
                _ => Err(DataFusionError::Execution(
                    "expected a boolean scalar for argument #3 of 'hive_partition'".to_string(),
                )),
            })
            .transpose()?
            .unwrap_or(false);

        let arrays = ColumnarValue::values_to_arrays(&values)?;

        let [file_paths, table_partition_columns]: [Option<&StringArray>; 2] =
            [&arrays[0], &arrays[1]].map(|arg| arg.as_any().downcast_ref());

        file_paths
            .zip(table_partition_columns)
            .ok_or_else(|| {
                DataFusionError::Internal(format!(
                    "Error while executing {HIVE_PARTITION_UDF_NAME}: \
                    Type check failed"
                ))
            })
            .and_then(|(file_paths, table_partition_columns)| {
                let mut builder = StringBuilder::new();

                for (file_path, table_partition_column) in
                    file_paths.iter().zip(table_partition_columns.iter())
                {
                    match file_path.zip(table_partition_column) {
                        Some((file_path, table_partition_column)) => {
                            builder.append_option(parse_partitions_for_path(
                                file_path,
                                table_partition_column,
                                null_if_missing,
                            )?);
                        }
                        _ => builder.append_null(),
                    };
                }

                Ok(ColumnarValue::Array(Arc::new(builder.finish())))
            })
    }
}

pub fn hive_partition(args: Vec<Expr>) -> Expr {
    Expr::ScalarFunction(ScalarFunction {
        func: Arc::new(hive_partition_udf()),
        args,
    })
}

// Extract partition column values from a file path, erroring if any partition columns are not found.
// Accepts a ListingTableUrl that will be trimmed from the file paths as well.
fn parse_partitions_for_path<'a>(
    file_path: &'a str,
    table_partition_col: &str,
    null_if_missing: bool,
) -> Result<Option<&'a str>> {
    match file_path.split('/').find_map(|part| {
        part.split_once('=')
            .and_then(|(name, val)| (name == table_partition_col).then_some(val))
    }) {
        Some(part) => Ok(Some(part)),
        None if null_if_missing => Ok(None),
        _ => Err(DataFusionError::Execution(format!(
            "Error while executing {HIVE_PARTITION_UDF_NAME}: \
                    Path '{file_path}' did not contain any values corresponding to \
                    partition column '{table_partition_col}'"
        ))),
    }
}

#[cfg(test)]
mod test {
    use super::HIVE_PARTITION_UDF_NAME;

    use datafusion::{assert_batches_sorted_eq, prelude::SessionContext};
    use datafusion_common::Result;

    struct TestCase {
        file_path_expr: &'static str,
        partition_columns: &'static [&'static str],
        expected_output: &'static str,
    }

    async fn run_test(context: &SessionContext, case: TestCase) -> Result<()> {
        let query = format!(
            "SELECT \
                column_name, \
                {HIVE_PARTITION_UDF_NAME}({}, column_name) AS partition_value \
            FROM (VALUES {}) AS partition_columns (column_name)",
            case.file_path_expr,
            case.partition_columns
                .iter()
                .map(|s| format!("('{s}')"))
                .collect::<Vec<_>>()
                .join(", ")
        );

        let df = context.sql(dbg!(&query)).await?;
        df.clone().show().await?;

        let results = df.collect().await?;
        assert_batches_sorted_eq!(
            case.expected_output
                .split_terminator('\n')
                .filter(|s| !s.is_empty())
                .map(str::trim)
                .collect::<Vec<_>>(),
            &results
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_extract_hive_partitions() {
        let context = SessionContext::new();
        context.register_udf(super::hive_partition_udf());

        let cases = vec![TestCase {
            file_path_expr: "'sip/trades/year=2006/month=01/day=02/trades-2006-01-02.parquet'",
            partition_columns: &["year", "month", "day"],
            expected_output: "
                +-------------+-----------------+
                | column_name | partition_value |
                +-------------+-----------------+
                | year        | 2006            |
                | month       | 01              |
                | day         | 02              |
                +-------------+-----------------+",
        }];

        for case in cases {
            run_test(&context, case).await.unwrap();
        }
    }

    #[tokio::test]
    async fn test_extract_hive_partitions_fails() {
        let context = SessionContext::new();
        context.register_udf(super::hive_partition_udf());

        let cases = vec![
            TestCase {
                file_path_expr: "'sip/trades/year=2006/month=01/day=02/trades-2006-01-02.parquet'",
                partition_columns: &["this-is-not-a-valid-partition-column"],
                expected_output: "",
            },
            TestCase {
                file_path_expr: "1", // numbers should fail the type check
                partition_columns: &["year", "month", "day"],
                expected_output: "",
            },
        ];

        for case in cases {
            dbg!(run_test(&context, case).await).expect_err("test should fail");
        }
    }
}
