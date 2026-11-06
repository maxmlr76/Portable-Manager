# Contributing

Issues and pull requests are welcome. For anything larger, please open an issue first so
we can agree on the direction before you spend time on it.

## Build and test

```
cargo test -p maxtrix-engine
cargo build -p maxtrix-app
```

To make the release package:

```
cargo build --release -p maxtrix-app
pwsh packaging/package.ps1
```

## Rules of the project

- No build step beyond Cargo, no WebView, one portable executable.
- PowerShell scripts from Scoop manifests are never executed. A tool that only installs
  through a setup program stays out of the catalog.
- Every download is checked against its checksum.
- All colors in the UI come from `crates/app/ui/tokens.slint`. Status is shown by shape,
  not by color.

By submitting a contribution you agree that it is published under the MIT license.
