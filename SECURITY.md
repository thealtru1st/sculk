# Security Policy

Sculk parses untrusted network input, so memory-safety, crash and denial-of-service problems are taken seriously.

## Reporting a vulnerability

Report vulnerabilities privately through GitHub's **Report a vulnerability** button on the repository's Security tab. Don't open public issues for them. Include the Sculk revision, the command line, and a minimal reproduction if you can.

## Scope

Sculk is an offline-mode server. Missing authentication, client-trusted inventory contents and client-trusted movement are documented design limits, not vulnerabilities. Crashes, hangs, unbounded memory growth, and anything that lets one client disrupt others are in scope.
