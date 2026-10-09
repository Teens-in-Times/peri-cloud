# Contributor guide

Read README.md and docs/architecture.md before changing the cloud/executor boundary.
The workspace contains the cloud/executor dependency closure; do not add TUI dependencies.

- Keep the cloud Agent loop and existing permission middleware as the approval authority.
- Keep executor transport validation distinct from tool permission policy.
- SSH credentials, transport tokens and endpoint configuration must not become model tool arguments.
- Registration is not evidence of a live authenticated executor.
- Preserve task/turn/session/device identity and do not replay unknown execution or delivery outcomes.
- Keep QQ behind the MessageAdapter interface; ordinary chat displays AI text only.
- Account UI must preserve OAuth human consent, CSRF, same-origin checks and exact approval/claim ownership.
- Keep full untrusted parameters in text nodes, escaped summaries and authenticated detail views.
- Never commit credentials, runtime databases, logs, personal histories or machine-specific deployment files.

Run the targeted tests documented in README.md, cargo fmt, and clippy for changed crates.
Add regression tests for changed execution/authentication behavior, rather than tests mirroring CSS.
Keep source/test files below 1000 lines; generated workflow artifacts are exempt.

Upstream source: KonghaYao/peri, revision d7ee444efe7a461b0696bd6c27c6c91f946fdec1.
This distribution contains modified source; preserve LICENSE, NOTICE and third-party attribution.