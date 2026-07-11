# Homebrew handoff

The public install command is:

```sh
brew install joaquincampo/tap/codex-for-grok
```

The tap repository is `joaquincampo/homebrew-tap`; Homebrew maps that repository to `joaquincampo/tap`.

After a GitHub release is published, run the **Homebrew handoff** workflow with the version without `v`. The workflow downloads the release checksum manifest, renders and syntax-checks `Formula/codex-for-grok.rb`, and always uploads it as a workflow artifact.

If the source repository has a `HOMEBREW_TAP_TOKEN` secret with contents read/write and pull-request access to the tap, the workflow also pushes a branch and opens a tap pull request. Without that secret, download the generated artifact, copy it to `Formula/codex-for-grok.rb` in the tap, and open the pull request manually.

Before merging, run in the tap checkout:

```sh
brew audit --strict --online joaquincampo/tap/codex-for-grok
brew install --build-from-source joaquincampo/tap/codex-for-grok
brew test joaquincampo/tap/codex-for-grok
```

Do not merge a formula until all four URLs and SHA-256 values match the published release.
