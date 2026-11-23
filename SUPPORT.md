# Support

## Community

Open an issue in this repository:

<https://github.com/xqh-jason/printkit/issues>

Bug reports and feature requests are read on a best-effort basis. There is no
response-time commitment.

For a print failure, include the output of `printkit doctor --json`, the
`traceId` from `--json` and the failing command. The Logs and Records pages,
`printkit logs`, `printkit stats`, `trace.*` and `records.*` belong to the
`observability` and `records` plugins, which are off under the default `core`
preset: enable them with `printkit plugin enable observability` / `records`
before collecting that evidence. Core printing works without them.

## Commercial

Integration help, custom development, and priority support are available under
a separate agreement. Contact the maintainer through the GitHub profile:

<https://github.com/xqh-jason>

## Security

Do not report vulnerabilities in a public issue. Use GitHub private
vulnerability reporting, or the minimal contact request described in
[SECURITY.md](SECURITY.md). Public issues must not contain exploit details,
tokens, internal hostnames or customer data.

## License

This project is distributed under `MIT OR Apache-2.0`; see `LICENSE-MIT` and
`LICENSE-APACHE`. The license grants the rights stated there and includes no
warranty, maintenance, or support obligation. Paid support is a separate
service, not a license condition.
