# Penpot ecosystem — core ideas

The folder is the source of truth for a design file; a package is just another kind of folder.
The **principles** below are durable — they are why the project exists. One design that follows
from them lives in [ecosystem-design.md](ecosystem-design.md) and may change as it meets reality.

## Principles (durable)

- **Local-first, no server.** Packages live on disk and travel as folders; nothing requires a
  service to be running to find, install, or use one.
- **Git repos, not a registry.** Discovery is federated and optional — anyone can run an index,
  none is authoritative.
- **Flat governance.** No verified tier, no badges, no monetization — trust comes from forks and
  usage, not certification.
- **Surface, don't apply.** Updates and conflicts are shown, never applied silently.
- **Contract over implementation.** What a package promises is versioned; how it's built is not.
