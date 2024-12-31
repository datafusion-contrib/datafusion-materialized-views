# Changelog

## 0.1.0

### Added

* v0.1.0 `FileMetadata` table, used for tracking object storage metadata. 
* v0.1.0 `RowMetadataRegistry` and `RowMetadataSource`, an abstraction layer for metadata used in incremental view maintenance.
* v0.1.0 `ListingTableLike` and `Materialized` traits, an API for describing Hive-partitioned tables in object storage.
* v0.1.0 `mv_dependencies` and `stale_files` UDTFs, the core features for incremental view maintenance.
