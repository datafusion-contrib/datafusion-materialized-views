# datafusion-materialized-views

An implementation of incremental view maintenance & query rewriting for materialized views in DataFusion.

A **materialized view** is a view whose query has been pre-computed and saved for later use. This can drastically speed up workloads by pre-computing at least a large fragment of a user-provided query. Furthermore, by implementing a _view matching_ algorithm, we can implement an optimizer that rewrites queries to automatically make use of materialized views where possible and beneficial, a concept known as _query rewriting_.

Efficiently maintaining the up-to-dateness of a materialized view is a problem known as _incremental view maintenance_. It is a hard problem in general, but we make some simplifying assumptions:

* Data is stored as Hive-partitioned files in object storage.
* The smallest unit of data that can be updated is a single file.

This is a typical pattern with DataFusion, as files in object storage usually are immutable (especially if they are Parquet) and can only be replaced, not appended to or modified. However, it does mean that our implementation of incremental view maintenance only works for Hive-partitioned materialized views in object storage. (Future work may generalize this to alternate storage sources, but the requirement of logically partitioned tables remains.) In contrast, the view matching problem does not depend on the underlying physical representation of the tables.

## Example

Here we walk through a hypothetical example of setting up a materialized view, to illustrate
what this library offers. The core of the incremental view maintenance implementation is a UDTF (User-Defined Table Function),
called `mv_dependencies`, that outputs a build graph for a materialized view. This gives users the information they need to determine
when partitions of the materialized view need to be recomputed.

```sql
-- Create a base table
CREATE EXTERNAL TABLE t1 (column0 TEXT, date DATE)
STORED AS PARQUET
PARTITIONED BY (date)
LOCATION 's3://t1/';

INSERT INTO t1 VALUES 
('a', '2021-01-01'), 
('b', '2022-02-02'), 
('c', '2022-02-03'), -- Two values in the year 2022
('d', '2023-03-03');

-- Pretend we can create materialized views in SQL
-- The TableProvider implementation will need to implement the Materialized trait.
CREATE MATERIALIZED VIEW m1 AS SELECT
    COUNT(*) AS count,
   date_part('YEAR', date) AS year
PARTITIONED BY (year)
LOCATION 's3://m1/';

-- Show the dependency graph for m1 using the mv_dependencies UDTF
SELECT * FROM mv_dependencies('m1');

+--------------------+----------------------+---------------------+-------------------+--------------------------------------+----------------------+
| target             | source_table_catalog | source_table_schema | source_table_name | source_uri                           | source_last_modified |
+--------------------+----------------------+---------------------+-------------------+--------------------------------------+----------------------+
| s3://m1/year=2021/ | datafusion           | public              | t1                | s3://t1/date=2021-01-01/data.parquet | 2023-07-11T16:29:26  |
| s3://m1/year=2022/ | datafusion           | public              | t1                | s3://t1/date=2022-02-02/data.parquet | 2023-07-11T16:45:22  |
| s3://m1/year=2022/ | datafusion           | public              | t1                | s3://t1/date=2022-02-03/data.parquet | 2023-07-11T16:45:44  |
| s3://m1/year=2023/ | datafusion           | public              | t1                | s3://t1/date=2023-03-03/data.parquet | 2023-07-11T16:45:44  |
+--------------------+----------------------+---------------------+-------------------+--------------------------------------+----------------------+
```

## More detailed example (with code)

As of now, actually implementing materialized views is somewhat complicated, as the library is initially focused on providing a minimal kernel of functionality that can be shared across multiple implementations of materialized views. Broadly, the process includes these steps:

* Define a custom `MaterializedListingTable` type that implements `Materialized`
* Register the type globally using the `register_materialized` global function
* Initialize the `FileMetadata` component
* Initialize the `RowMetadataRegistry`
* Register the `mv_dependencies` and `stale_files` UDTFs (User Defined Table Functions) in your DataFusion `SessionContext`
* Periodically regenerate directories marked as stale by `stale_files`

A full walkthrough of this process including implementation can be seen in an integration test, under [`tests/materialized_listing_table.rs`](tests/materialized_listing_table.rs).
