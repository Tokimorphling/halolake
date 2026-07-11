# new-api Web Frontend

This directory is the extracted new-api frontend workspace. It is kept separate
from Rust crates and can be built into static assets served by
`halolake-control-api`.

Build the default theme (embedded into `halolake-control-api`):

```sh
bun install
(cd default && VITE_REACT_APP_VERSION=halolake-dev bun run build)
```

`classic/` is optional legacy UI: not embedded in Docker/release by default.
To use it, build `classic/dist` and point `[web].classic_dist` + `theme = "classic"`.
