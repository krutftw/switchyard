# Security and deployment model

Switchyard is an operator-managed gateway to configured provider accounts.
Run it as a dedicated, unprivileged OS user and keep its configuration and
data directory private to that account. Use TLS before exposing it beyond
loopback. Client authentication is enabled by default; keep it enabled on
shared networks. The admin API is local by default, with remote access an
explicit option. Container port publishing and admin access are separate
controls: see the loopback-only Docker example in the README.

## Administrative access

The admin secret grants control over the whole gateway: provider endpoints,
credentials, environment-variable references, client keys, logs and captured
requests. It is not a limited tenant role. Only trusted operators should have
it, and imported configurations need the same trust and review as handwritten
configuration. A process environment variable referenced by configuration can
be sent to the configured provider; do not give the gateway unrelated host
secrets.

The dashboard keeps the admin secret in this tab's `sessionStorage`. Selecting
**Remember on this device** also stores it in `localStorage`; this is off by
default. These are bearer credentials accessible to scripts on the dashboard
origin and to anyone who can read the browser profile. There is no separate,
expiring browser session credential in this version. Use a trusted browser
profile, leave Remember off on shared devices, and do not host untrusted code
on the same origin. Signing out removes the browser's copies; rotate the admin
secret to revoke a copy that has been obtained elsewhere.

## Provider accounts and client keys

A client key's model allow-list and rate limit control model access. They do
not create a separate provider account or a per-client file namespace. Native
provider file handles, uploaded files, cached content and Google Cloud Storage
URIs are resolved by the upstream using the configured provider credentials.
Clients sharing those credentials can therefore share access to those
provider resources. Use separate provider accounts or gateway instances when
clients require resource isolation. Grant service accounts only the storage
permissions needed for that instance.

Provider responses and model output remain untrusted content. Protocol
translation does not prevent prompt injection or make model-generated text
safe to execute. Copied shell examples must be run in the shell named by the
dashboard.

## Stored data

Request-body capture is optional and can contain prompts, responses, personal
data and application secrets even after credential redaction. Protect the
data directory, restrict retention and access, and enable capture only when
needed. Windows files inherit their directory's ACLs: choose a private
directory and review those permissions before storing real credentials.

## Verification limits

Automated tests use synthetic fixtures and mock upstreams. They establish
the behavior tested; they do not establish compatibility with every live
provider, deployment, model or SDK version. Review release notes for the
platforms actually built and tested. A source review or a clean dependency
audit is not a guarantee that all security defects have been found.
