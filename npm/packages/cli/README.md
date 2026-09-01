# `@xgen/cli`

`@xgen/cli` is the npm distribution wrapper for the native XGENy CLI.

```bash
npm install --global @xgen/cli@0.1.0-rc.3
xgeny --version
```

The package selects one exact-version, platform-specific optional dependency and executes the
bundled Rust binary. It does not download code during installation, compile native code, or run an
install lifecycle script. Installing with `--omit=optional` is unsupported because it removes the
native binary package.

The native binary can also be installed without Node.js from the matching GitHub Release. See the
[XGENy repository](https://github.com/PlateerLab/xgeny-cli) for model onboarding and security
boundaries.
