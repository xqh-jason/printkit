# Security policy

printkit runs a loopback printing service that can submit jobs, read exported
files and fetch documents from URLs. We take reports about that surface
seriously.

## Supported versions

| Version | Supported |
|---|---|
| 0.2.x | Yes; security fixes are backported to the latest 0.2.x patch |

The latest published release is v0.2.4. The `main` branch is 0.2.6-dev and
has not been tagged or released.

The project is pre-1.0. Older pre-release builds may receive a fix only when
the report also affects the current 0.2.x line.

## Reporting a vulnerability

**Preferred:** use GitHub private vulnerability reporting for this repository:
Security tab → Report a vulnerability. The report stays private until a fix and
an advisory are ready.

**Fallback:** if private reporting is unavailable, open a minimal public issue
titled `security contact request` that contains **no** exploit details, affected
versions or reproduction steps. The maintainer will reply with a private
channel. Do not put vulnerability details in a public issue, pull request,
discussion or commit message.

Please include:

- affected version or commit;
- platform (macOS/Windows, architecture);
- the endpoint, WebSocket method or CLI command involved;
- whether the service is reachable from a browser page, a local process, or a
  non-loopback interface;
- a minimal reproduction, if it does not contain real customer data.

## Scope

In scope:

- the loopback HTTP/WebSocket service (`printkit serve`), including
  authentication, session cookies, CSRF, the exact `Origin` allowlist, token
  issuance and storage;
- the administrator token file and its permissions;
- `/exports/<path>` path confinement and `Host` checks;
- `data.url` fetching (SSRF, redirects, DNS rebinding, body limits);
- privilege boundaries between `print`, `raw` and `admin` scopes;
- local file writes, SQLite persistence and log/export redaction.
- the built-in plugin registry: `plugins.toml` parsing/atomic replacement, the
  service-lifetime lock, `/admin/plugins` authorization, and the guarantee that
  plugin state can only disable features, never bypass print/raw/admin scopes.

Out of scope:

- a printer driver, WebView, CUPS or Windows spooler vulnerability that printkit
  merely calls into (report it upstream);
- physical access to the user's machine or a process already running as the
  same user;
- denial of service caused by the user deliberately configuring an extreme
  queue/body limit;
- the ad-hoc/unsigned state of development builds (release artifacts document
  the signing status separately).

This build ships the two built-in plugins only. Loading third-party plugin
packages, the debug-only `PRINTKIT_TEST_HOOK` seam and a compromised local
account are not part of the supported attack surface.

## Response

- Acknowledgement target: 3 business days.
- Initial triage and severity: 10 business days.
- Fix target: 30 days for high-severity issues, or a documented mitigation
  when an upstream dependency blocks the fix.
- We will credit reporters in the advisory unless they ask to stay anonymous.

There is no bug bounty. This policy does not create a support or warranty
obligation beyond the project licenses.
