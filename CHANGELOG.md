# Changelog

## 3.0.0

The library is now named `chromey` everywhere. The crate was already `chromey` on
crates.io, but the import was still `chromiumoxide`. Version 3 makes the two match. No behavior changes beyond the two noted at the end.

### Breaking

- The library name is `chromey`. Replace `use chromiumoxide::` with `use chromey::`.
- The support crates have new names, and all five crates now share one version.

  | 2.x | 3.0 |
  |---|---|
  | `spider_chromiumoxide_types` | `chromey_types` |
  | `spider_chromiumoxide_pdl` | `chromey_pdl` |
  | `spider_chromiumoxide_cdp` | `chromey_cdp` |
  | `spider_chromiumoxide_fetcher` | `chromey_fetcher` |

- Generated code refers to `chromey_types::`. If you run the PDL generator yourself,
  regenerate your output.

### Upgrading without editing imports

Cargo can keep the old name for you. Rename the dependency and your code compiles as it
did on 2.x.

```toml
[dependencies]
chromiumoxide = { package = "chromey", version = "3" }
```

### Changed

- When you launch Chrome without a user data directory, chromey now uses
  `chromey-runner` under the system temp directory. It used `chromiumoxide-runner`
  before, so a profile left there by 2.x is not reused.
- The `storage-cookie` example sets a cookie named `set_from_chromey`.

### Docs

- New README with a Puppeteer mapping table and an upgrade guide.
- Crate metadata for all five crates points at this repository.
