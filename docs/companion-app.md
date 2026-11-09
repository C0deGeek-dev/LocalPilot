# The embedded companion app

The remote gateway (`localpilot remote gateway`) serves the companion phone
app, LocalLink, from inside the `localpilot` binary. The app is built in its
own repository; this repository keeps a copy of one release build and the pin
that names it.

## What is embedded

- `crates/localpilot-cli/client/` is the complete build output: `index.html`,
  `manifest.webmanifest`, `assets/<digest>/...` and `bundle.json` (the build's
  manifest: version, source revision, protocol range, and every file's path,
  size, SHA-256 and content type).
- `crates/localpilot-cli/client.pin` names the one build allowed:

  ```text
  revision <40-hex LocalLink commit>
  bundle-sha256 <64-hex SHA-256 of bundle.json>
  ```

The build script refuses to compile, with the reason, when:

- the bundle or the pin is missing;
- `bundle.json` does not have the pinned digest or revision;
- the build was made from uncommitted source (`source_dirty`);
- a file's size, digest or content type differs from `bundle.json`;
- a path is not a plain relative path (no `..`, no leading `/`, no `\`);
- a content type is not one of HTML, JavaScript, CSS, web manifest, PNG or SVG;
- the folder holds a file `bundle.json` does not list.

A test checks that the app's protocol range includes this server's RPC
version and that every capability it requires is one this server offers.

## How it is served

Only the listed files exist. `/` is `index.html`; there is no fallback route,
and `bundle.json` is never served. `index.html` and the web manifest are sent
with `Cache-Control: no-cache`; files under `assets/` are content-addressed
and sent as `public, max-age=31536000, immutable`. Every response keeps the
gateway's security headers (a same-origin-only content security policy and
`nosniff`). `localpilot remote status` shows the embedded app's version and
revision.

## Updating the app

1. In a clean LocalLink checkout at the commit to ship, with the Node version
   from its `.node-version`: `npm ci --ignore-scripts`, then
   `npm run build:release`. It prints the revision and the `bundle.json`
   SHA-256.
2. Replace `crates/localpilot-cli/client/` with the new `dist/` folder as a
   whole (delete the old folder first, so no stale asset stays behind).
3. Write both printed values into `crates/localpilot-cli/client.pin`.
4. Build and run the tests; commit the folder and the pin together.

`.gitattributes` marks the folder `-text`, so every checkout gets the exact
bytes the digests were made from.
