# Changes

## 2.1.0-dev.12

- Refresh tray history after an incoming copy has been committed to the local
  cache. Received entries no longer require an agent restart to appear.
- Preserve clipboard ordering: rejected older copies do not change history or
  trigger a misleading menu refresh.

Historical caches remain local to each device. Offline periods and private
copies can produce different older entries; this change does not merge archives.
