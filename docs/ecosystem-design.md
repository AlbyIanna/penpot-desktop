# Penpot ecosystem — design sketch (not yet built)

One concrete design that follows from the [core ideas](ecosystem-concept.md). The principles
there are settled; this mechanism is not — the open questions below are the parts not yet
verified, and they are load-bearing.

## The mechanism

- **Unit of sharing:** component libraries, plugins, and templates share ONE distribution path —
  a git repo under `.penpot-packages/` next to your designs. How each *activates* differs, and
  that difference is real: templates and component libraries are design data the sync daemon
  imports like any other folder; a plugin is not design JSON but executable code Penpot loads
  through its manifest/URL boundary, so a plugin package is *carried and pointed at*, never
  imported into the design DB — which also keeps it inside the "only URLs reach the canvas"
  invariant.
- **Versioning (components):** a component's contract is its variant names, exposed properties,
  and tokens used — nothing else. A change is patch (implementation only), minor (contract grew),
  or major (contract lost or renamed something), diffed automatically. A lockfile pins what each
  file uses; updates are surfaced, never applied silently — the conflict policy, applied to
  packages instead of files. (Plugins and templates need their own contract definitions; a
  plugin's is its API surface, a template's is closer to "none" — it's a starting point, not a
  dependency.)

## Open questions (not yet verified)

- **Is the contract machine-extractable?** `vault-index` already reads component names, colors,
  and typographies from the normalized JSON — half the corpus is proven. Whether variant names,
  exposed properties, and "tokens used" are all cleanly present and stable-diffable is a spike the
  versioning story depends on.
- **Tokens are a dependency graph, not a footnote.** `tokens.json` already ships in binfile-v3. If
  a component's contract includes the tokens it uses, then tokens versioning independently is what
  turns "just a folder" into cross-package dependencies — the exact line between a folder and a
  package manager. Unresolved.
- **Plugin supply chain.** Federated, certification-free trust means executable code from
  arbitrary repos. The mitigation is architectural, not social: the manifest/URL boundary above
  must bound what a plugin package can touch. Name that limit before shipping plugin packages.
