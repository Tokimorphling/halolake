# new-api Web Frontend

This directory is the extracted new-api frontend workspace. It is kept separate
from Rust crates and can be built into static assets served by
`halolake-control-api`.

Build both themes:

```sh
bun install
(cd default && VITE_REACT_APP_VERSION=halolake-dev bun run build)
(cd classic && VITE_REACT_APP_VERSION=halolake-dev bun run build)
```

The control API serves `default/dist` and `classic/dist` when `[web] enabled =
true` in the control-api TOML config.
