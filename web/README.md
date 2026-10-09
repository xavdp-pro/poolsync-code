# Optional legacy dashboard

This React interface uses the legacy hub API for presence, clipboard history and
layout editing. It does not represent the direct hubless agents' live state.
Hubless daily operation does not require this dashboard or its server.

Use a current supported Node.js version compatible with the package lock:

```sh
npm ci
npm test
npm run build
```

Build output is `dist/`. For an explicitly selected legacy deployment, the
`poolsync-hub --web-dir PATH` option serves that directory. The dashboard asks
for a hub token, keeps it in browser local storage and sends it in the
Authorization header. Review browser access and storage accordingly.

For local development, `npm run dev` listens on `127.0.0.1:9471`. The development
proxy targets `127.0.0.1:9470`; change the Vite configuration for a different
local legacy server. No hosting platform or automatic deployment is configured.
