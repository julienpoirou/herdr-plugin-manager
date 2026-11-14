# Security policy

## Reporting a vulnerability
Please **do not** open a public issue for a security vulnerability.
Contact: **julienpoirou@protonmail.com** (subject `[SECURITY]`).

Include the affected version, platform, source type (CLI, GitOps, marketplace,
popup), a minimal reproduction, and whether execution occurred before the
trust confirmation. A safe proof using a temporary marker file is preferred
to a destructive payload.

## Disclosure process and timelines
We follow coordinated vulnerability disclosure:
1. We acknowledge every vulnerability report within 3 days (72 hours).
2. We aim to confirm the vulnerability and share a remediation plan within 14 days.
3. We aim to ship a fix or mitigation within 90 days of confirmation.
4. We coordinate public disclosure of the vulnerability with the reporter once a
   fix is released, and credit the reporter unless they ask to stay anonymous.

If a vulnerability is not acknowledged within 7 days, or a fix is not delivered
within the 90-day disclosure window, the reporter may request an extension or,
as a last resort, disclose the vulnerability independently.

## Supported versions
- Latest release + previous (security patches).
