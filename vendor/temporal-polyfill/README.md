# Temporal polyfill

This directory vendors `@js-temporal/polyfill` 0.5.1's UMD distribution,
formatted with Prettier 3.6.2 so QuickJS can report useful source locations.

- Source: https://github.com/js-temporal/temporal-polyfill
- Package integrity: `sha512-hloP58zRVCRSp[...]wvZvmapQnKwFQ==`
- License: ISC

The QuickJS backend installs the polyfill as the engine's `Temporal` intrinsic.
Named time-zone calculations are supplied by `temporal_rs` and bundled
zoneinfo64 data through the backend's `Intl.DateTimeFormat` host implementation.
