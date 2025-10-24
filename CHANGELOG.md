# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0](https://github.com/datafusion-contrib/datafusion-materialized-views/compare/v0.1.1...v0.2.0) - 2025-10-24

### Added
- `Decorator` trait ([#26](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/26)) (by @suremarc) - #26

### Other
- remove useless lines in changelog ([#97](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/97)) (by @xudong963) - #97
- Improve the doc ([#95](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/95)) (by @xudong963) - #95
- Support limit pushdown for OneOfExec ([#94](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/94)) (by @xudong963) - #94
- Improved documentation on IVM algorithm ([#90](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/90)) (by @suremarc) - #90
- Support static partition columns for MV ([#89](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/89)) (by @suremarc) - #89
- upgrade to DF50 ([#87](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/87)) (by @xudong963) - #87
- Fix empty unnest columns handling when pushdown_projection_inexact ([#88](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/88)) (by @zhuqi-lucas) - #88
- make cost fn accept candidates ([#83](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/83)) (by @xudong963) - #83
- Upgrade DF to 49.0.2 ([#86](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/86)) (by @zhuqi-lucas) - #86
- Upgrade to DF49 ([#75](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/75)) (by @xudong963) - #75
- Upgrade DataFusion 48.0.0 ([#61](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/61)) (by @xudong963) - #61
- Allow customization of `list_all_files` function. ([#69](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/69)) (by @jared-m-combs) - #69
- Allow for 'special' partitions that are omitted in the staleness check. ([#68](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/68)) (by @jared-m-combs) - #68
- don't panic if eq class is not found ([#60](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/60)) (by @suremarc) - #60
- Handle table scan filters that reference dropped columns ([#59](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/59)) (by @suremarc) - #59
- exclude some materialized views from query rewriting ([#57](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/57)) (by @suremarc) - #57
- Optimize performance bottleneck if projection is large ([#56](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/56)) (by @xudong963) - #56
- Upgrade df47 ([#55](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/55)) (by @xudong963) - #55
- Update itertools requirement from 0.13 to 0.14 ([#32](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/32)) (by @dependabot[bot]) - #32
- Update ordered-float requirement from 4.6.0 to 5.0.0 ([#49](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/49)) (by @dependabot[bot]) - #49
- Upgrade DF46 ([#48](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/48)) (by @xudong963) - #48
- Update extension ([#45](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/45)) (by @matthewmturner) - #45
- make explain output stable ([#44](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/44)) (by @suremarc) - #44
- Add alternate analysis for MVs with no partition columns ([#39](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/39)) (by @suremarc) - #39
- upgrade to datafusion 45 ([#38](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/38)) (by @suremarc) - #38
- use nanosecond timestamps in file metadata ([#28](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/28)) (by @suremarc) - #28

### Contributors

* @xudong963
* @suremarc
* @zhuqi-lucas
* @jared-m-combs
* @dependabot[bot]
* @matthewmturner

## [0.1.1](https://github.com/datafusion-contrib/datafusion-materialized-views/compare/v0.1.0...v0.1.1) - 2025-01-07

### Added
- view exploitation (#19) (by @suremarc) - #19
- SPJ normal form (#18) (by @suremarc) - #18

### Other
- add constructor for RowMetadataRegistry from FileMetadata ([#25](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/25)) (by @suremarc) - #25
- add changelog manually ([#14](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/14)) (by @suremarc) - #14
- don't use paths-ignore ([#15](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/15)) (by @suremarc) - #15

### Contributors

* @suremarc

## [0.1.0](https://github.com/datafusion-contrib/datafusion-materialized-views/releases/tag/v0.1.0) - 2025-01-03

### Other
- some api improvements + remove manual changelog ([#12](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/12)) (by @suremarc) - #12
- Integration test ([#10](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/10)) (by @suremarc) - #10
- setup changelog ([#9](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/9)) (by @suremarc) - #9
- Release plz ([#7](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/7)) (by @suremarc) - #7
- stale_files + rename to mv_dependencies ([#6](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/6)) (by @suremarc) - #6
- Incremental view maintenance ([#3](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/3)) (by @suremarc) - #3
- Add `FileMetadata` table and `RowMetadataRegistry` ([#2](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/2)) (by @suremarc) - #2
- Setup cargo + CI ([#1](https://github.com/datafusion-contrib/datafusion-materialized-views/pull/1)) (by @suremarc)
- Initial commit (by @matthewmturner)

### Contributors

* @suremarc
* @matthewmturner
