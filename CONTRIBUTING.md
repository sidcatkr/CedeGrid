# Contributing to CedeGrid

Use issues for reproducible bugs and proposals, and pull requests for changes.
Include the platform, configuration with secrets removed, exact revision, expected
behavior, actual result and a small reproducer. Keep application integrations
separate from the generic core. Do not upload credentials, private datasets,
unrelated process information or raw deployment logs.

Follow the home-local verification commands in [README](README.md#verification).
Run checks affected by your change; preserve meaningful fault tests and earlier
failed evidence. Compilation is not runtime validation. Never weaken unknown
telemetry, ownership checks, launch barriers or retained reservations to pass a test.
Only run hardware or shared-host experiments inside an authorized resource envelope.

Contributions are supplied under the project Apache-2.0 license unless explicitly
stated otherwise. Preserve applicable third-party notices. Security-sensitive reports
belong in the [private reporting channel](SECURITY.md), not a public issue.
